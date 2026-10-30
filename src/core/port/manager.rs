//! The port manager: the one handle the UI drives. It owns the backend, hands out
//! session ids, and keeps the current session. This slice carries a single session
//! at a time (design.md's tab-per-session multi-session arrives with the session
//! tabs ticket); the API shape (`open` returning a session id, bus events tagged
//! with it) is already multi-session ready.
//!
//! Ticket #19: the session's Lua pre-send hook lives here (docs/design.md: "TX
//! path always goes through the session's Lua pre-send hook"). [`Self::write`]
//! funnels every UI-originated send through the hook when one is set;
//! [`Self::write_raw`] bypasses it for sends that are already final (a script's
//! `apiSendUartData` — llcom parity: the conversion applies to UI sends, not to
//! script-driven uart writes).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;

use super::session::Session;
use super::{PortConfig, PortInfo, SerialBackend};
use crate::core::bus::{
    publish_data, Direction, Event, EventBus, LogEvent, LogLevel, PortEvent, PortState,
};
use crate::core::error::{CoreError, Result};
use crate::core::lua::TxHook;

pub struct PortManager {
    backend: Arc<dyn SerialBackend>,
    bus: EventBus,
    next_session_id: AtomicU64,
    session: Mutex<Option<Session>>,
    /// The Lua pre-send hook (`None` = disabled, the default). Swapped as a whole;
    /// `write` clones the `Arc` out so the blocking conversion never runs under
    /// the lock.
    hook: RwLock<Option<Arc<TxHook>>>,
}

impl PortManager {
    pub fn new(backend: Arc<dyn SerialBackend>, bus: EventBus) -> Self {
        Self {
            backend,
            bus,
            next_session_id: AtomicU64::new(1),
            session: Mutex::new(None),
            hook: RwLock::new(None),
        }
    }

    /// The bus this manager publishes on, for consumers.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Lists the ports the backend offers right now. Blocking enumeration runs off
    /// the async context.
    pub async fn list_ports(&self) -> Result<Vec<PortInfo>> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.enumerate()).await?
    }

    /// Opens `name`, replacing any current session (open is idempotent from the
    /// UI's point of view: "open another port" just works). Returns the new
    /// session id; the bus carries the matching [`Event::Port`] transitions.
    pub async fn open(&self, name: &str, config: PortConfig) -> Result<u64> {
        let mut current = self.session.lock().await;
        if let Some(previous) = current.take() {
            tracing::info!(port = %previous.port, "closing previous session");
            previous.close().await;
        }

        let opened = {
            let backend = self.backend.clone();
            let name = name.to_string();
            let config = config.clone();
            // `open` touches the OS: keep it off the async workers.
            tokio::task::spawn_blocking(move || backend.open(&name, &config)).await??
        };

        let id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        // Publish the open state before the session's forwarder can deliver data,
        // so the log reads "opened" before any RX rows of the new session. The baud
        // rides along: the reconnect machine re-opens with the user's config.
        self.publish_port(id, name, PortState::Open, None, Some(config.baud_rate));
        let session = Session::spawn(id, name.to_string(), opened, config, self.bus.clone());
        *current = Some(session);

        tracing::info!(session = id, port = name, "session open");
        Ok(id)
    }

    /// Closes the current session, if any. Never hangs: see [`Session::close`].
    ///
    /// Publishes the terminal [`PortState::Closed`] itself: the session's forwarder
    /// has already ended when the session died earlier (mid-stream error), so the
    /// explicit event is what tells the reconnect machine (and the UI) that the
    /// *user* closed — the disarm condition of auto-reconnect.
    pub async fn close(&self) {
        let mut current = self.session.lock().await;
        if let Some(session) = current.take() {
            let (id, port) = (session.id, session.port.clone());
            session.close().await;
            self.publish_port(id, &port, PortState::Closed, None, None);
        }
    }

    /// Whether a session is currently open.
    pub async fn is_open(&self) -> bool {
        self.session.lock().await.is_some()
    }

    /// Installs or clears the session's Lua pre-send hook (`None` = disabled).
    /// The script runs once per send (bounded by the 3 s send-VM watchdog); the
    /// hook applies from the next [`Self::write`] on, whatever the session state.
    pub fn set_hook(&self, hook: Option<TxHook>) {
        *self.hook.write().expect("hook lock") = hook.map(Arc::new);
    }

    /// The active hook's script, when one is set (for status display).
    pub fn hook_script(&self) -> Option<String> {
        self.hook
            .read()
            .expect("hook lock")
            .as_ref()
            .map(|hook| hook.script().to_string())
    }

    /// Queues `bytes` on the current session and publishes its TX data event.
    /// When a pre-send hook is set, the bytes go through it first (blocking Lua,
    /// run off the async workers; llcom's 3 s watchdog bounds a runaway
    /// conversion). An empty conversion result sends nothing (llcom parity: a
    /// script returning nil sends nothing); a failed hook rejects the send with
    /// [`CoreError::Hook`] and a log row — it never panics and never writes.
    pub async fn write(&self, bytes: Vec<u8>) -> Result<()> {
        // Clone the `Arc` out of the lock first: the guard must not live across
        // the await below.
        let hook = self.hook.read().expect("hook lock").clone();
        let bytes = match hook {
            Some(hook) => {
                let bus = self.bus.clone();
                let outcome =
                    tokio::task::spawn_blocking(move || hook.apply(&bytes, &bus)).await?;
                match outcome {
                    Ok(converted) => {
                        if converted.is_empty() {
                            tracing::debug!("tx hook produced no bytes; nothing sent");
                            return Ok(());
                        }
                        converted
                    }
                    Err(error) => {
                        let message = error.to_string();
                        // Plain-English core diagnostics (the reconnect machine's
                        // convention); the app layers add localized framing.
                        self.bus.publish(Event::Log(LogEvent {
                            level: LogLevel::Error,
                            timestamp: chrono::Local::now(),
                            message: format!("lua tx hook failed: {message}"),
                        }));
                        return Err(CoreError::Hook(message));
                    }
                }
            }
            None => bytes,
        };
        self.write_raw(bytes).await
    }

    /// Queues `bytes` on the current session and publishes its TX data event,
    /// **without** the pre-send hook — the path for sends that are already final
    /// (a script's `apiSendUartData`).
    pub async fn write_raw(&self, bytes: Vec<u8>) -> Result<()> {
        let current = self.session.lock().await;
        let session = current
            .as_ref()
            .ok_or_else(|| CoreError::NotOpen("(none)".into()))?;
        let id = session.id;
        session.write(bytes.clone())?;
        drop(current);
        publish_data(&self.bus, id, Direction::Tx, bytes);
        Ok(())
    }

    fn publish_port(&self, id: u64, port: &str, state: PortState, detail: Option<String>, baud: Option<u32>) {
        self.bus.publish(Event::Port(PortEvent {
            session_id: id,
            port: port.to_string(),
            state,
            detail,
            timestamp: chrono::Local::now(),
            baud,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::port::mock::{MockBackend, MOCK_PORT_NAME};

    /// Drains the bus in the background into a channel the test awaits.
    struct EventTap {
        rx: std::sync::mpsc::Receiver<Event>,
        _keep: tokio::task::JoinHandle<()>,
    }

    impl EventTap {
        fn new(bus: &EventBus) -> Self {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut events = bus.subscribe();
            let keep = tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        Err(_) => continue,
                    }
                }
            });
            Self { rx, _keep: keep }
        }

        /// The next TX event's bytes.
        fn next_tx(&self, within: std::time::Duration) -> Vec<u8> {
            let deadline = std::time::Instant::now() + within;
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match self.rx.recv_timeout(left) {
                    Ok(Event::Data(data)) if data.direction == Direction::Tx => {
                        return data.bytes.as_slice().to_vec()
                    }
                    Ok(_) => continue,
                    Err(e) => panic!("no TX event within {within:?}: {e}"),
                }
            }
        }

        /// The next log message.
        fn next_log(&self, within: std::time::Duration) -> String {
            let deadline = std::time::Instant::now() + within;
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match self.rx.recv_timeout(left) {
                    Ok(Event::Log(log)) => return log.message,
                    Ok(_) => continue,
                    Err(e) => panic!("no log event within {within:?}: {e}"),
                }
            }
        }
    }

    /// THE #19 TX-path AC: with the hook `return uartData:fromHex()` set, hex text
    /// `414243` reaches the wire as the bytes `41 42 43` — the mock echoes them
    /// back, proving the conversion happened *before* the port writer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_goes_through_the_pre_send_hook() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = PortManager::new(Arc::new(MockBackend::new()), bus.clone());
        manager
            .open(MOCK_PORT_NAME, PortConfig::new(115_200))
            .await
            .expect("mock open");
        manager.set_hook(Some(TxHook::new("return uartData:fromHex()")));
        assert_eq!(
            manager.hook_script().as_deref(),
            Some("return uartData:fromHex()")
        );

        manager.write(b"414243".to_vec()).await.expect("hooked write");
        let tx = tap.next_tx(std::time::Duration::from_secs(5));
        assert_eq!(tx, vec![0x41, 0x42, 0x43], "hex text must hit the wire converted");

        // The mock echoes the *converted* bytes back as RX.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let event = tap.rx.recv_timeout(left).expect("no RX echo");
            if let Event::Data(data) = event {
                if data.direction == Direction::Rx && data.bytes.as_slice() == [0x41, 0x42, 0x43] {
                    break;
                }
            }
        }

        // Clearing the hook restores the passthrough.
        manager.set_hook(None);
        assert_eq!(manager.hook_script(), None);
        manager.write(b"raw".to_vec()).await.expect("plain write");
        assert_eq!(tap.next_tx(std::time::Duration::from_secs(5)), b"raw".to_vec());

        manager.close().await;
    }

    /// A failing hook rejects the send (contract §2.4 `SessionError::Hook` shape:
    /// here [`CoreError::Hook`]), publishes a log row, and nothing reaches the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failing_hook_rejects_the_send_with_a_log_row() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = PortManager::new(Arc::new(MockBackend::new()), bus.clone());
        manager
            .open(MOCK_PORT_NAME, PortConfig::new(115_200))
            .await
            .expect("mock open");
        manager.set_hook(Some(TxHook::new("error('boom', 0)")));

        let error = manager
            .write(b"x".to_vec())
            .await
            .expect_err("a failing hook must reject the send");
        assert!(error.to_string().contains("boom"), "{error}");
        let log = tap.next_log(std::time::Duration::from_secs(5));
        assert!(
            log.contains("lua tx hook failed") && log.contains("boom"),
            "{log}"
        );

        // An empty conversion result sends nothing (llcom: nil → no data).
        manager.set_hook(Some(TxHook::new("return nil")));
        manager.write(b"dropped".to_vec()).await.expect("nil hook ok");
        manager.set_hook(None);

        // `write_raw` bypasses the hook even while one is set (script uart writes).
        manager.set_hook(Some(TxHook::new("return 'nope'")));
        manager
            .write_raw(b"final".to_vec())
            .await
            .expect("raw write");
        assert_eq!(tap.next_tx(std::time::Duration::from_secs(5)), b"final".to_vec());

        manager.close().await;
    }

    /// Writes without a session fail as before (the hook changes nothing there).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_without_session_is_not_open() {
        let bus = EventBus::new();
        let _tap = EventTap::new(&bus);
        let manager = PortManager::new(Arc::new(MockBackend::new()), bus);
        let error = manager.write(b"x".to_vec()).await.expect_err("closed");
        assert!(matches!(error, CoreError::NotOpen(_)));
        assert_eq!(manager.next_session_id.load(Ordering::Relaxed), 1);
    }
}
