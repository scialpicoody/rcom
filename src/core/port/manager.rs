//! The port manager: the one handle the UI drives. It owns the backend
//! (through the [`factory::BackendFactory`] seam), hands out session ids, and
//! keeps the session table.
//!
//! **Multi-session (tickets #37 + #35):** sessions live in a map keyed by
//! session id and are fully independent — each with its own reader-forwarder
//! task, write queue and pre-send hook. Opening a port whose *live* session
//! already exists is refused with [`CoreError::AlreadyOpen`] (`PortBusy` per
//! session — design-ui §3.6); opening over a *dead* session (unplug, error)
//! replaces it — the reconnect machine's "reopen replaces the dead session"
//! (the liveness probe is [`Session::is_alive`]). The reconnect machine tracks
//! every armed/watching session independently ([`super::reconnect`]).
//!
//! Ticket #19's Lua pre-send hook stays: it is per session now
//! ([`Self::set_hook_for`]), with the manager-wide slot (ticket #19's
//! [`Self::set_hook`]) as the fallback the legacy demo paths use.
//! [`Self::write_for`] funnels UI-originated sends through the session's
//! hook when one is set; [`Self::write_raw_for`] bypasses it for sends that
//! are already final (a script's `apiSendUartData` — llcom parity). The
//! post-hook knowledge rides the TX event (`DataEvent.hooked`, design-ui
//! §5.2 as amended by #30).
//!
//! **Cross-session scripting (ticket #35):** one [`PortManager`] per process
//! is published with [`Self::init_global`]; the `rcom.ports` Lua table
//! ([`crate::core::lua::ports_api`]) resolves it with [`Self::global`] to
//! enumerate sessions and cross-send.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;

use super::mock::MockControl;
use super::session::Session;
use super::{PortConfig, PortInfo, SerialBackend};
use crate::core::bus::{
    publish_rx, publish_tx, Event, EventBus, LogEvent, LogLevel, PortEvent, PortState,
};
use crate::core::error::{CoreError, Result};
use crate::core::lua::TxHook;

/// The manager-wide hook slot's key (the legacy `set_hook` path; per-session
/// hooks shadow it).
const GLOBAL_HOOK: u64 = 0;

/// The process-global manager (the `rcom.ports` seam's anchor): published by
/// the app's engine startup (once per process); `init_global` is
/// last-write-wins so tests can point it at their own instance.
static GLOBAL_MANAGER: RwLock<Option<Arc<PortManager>>> = RwLock::new(None);

/// One entry of the session enumeration the `rcom.ports.list()` API returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: u64,
    pub port: String,
    /// Whether the session is live ([`Session::is_alive`]) — a dead session
    /// lingers until replaced or closed; sends to it fail.
    pub open: bool,
}

pub struct PortManager {
    backend: Arc<dyn SerialBackend>,
    bus: EventBus,
    next_session_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Session>>,
    /// The most recently opened session (the legacy single-session API's
    /// target: `write`/`close` without an id).
    current: Mutex<Option<u64>>,
    /// The pre-send hooks: per session id, plus the manager-wide slot under
    /// [`GLOBAL_HOOK`]. `write_for` consults the session's slot first.
    hooks: RwLock<HashMap<u64, Arc<TxHook>>>,
}

impl PortManager {
    pub fn new(backend: Arc<dyn SerialBackend>, bus: EventBus) -> Self {
        Self {
            backend,
            bus,
            next_session_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
            current: Mutex::new(None),
            hooks: RwLock::new(HashMap::new()),
        }
    }

    /// The bus this manager publishes on, for consumers.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Publishes this manager as the process-global instance (the
    /// `rcom.ports` Lua API's anchor). The engine calls this once at startup;
    /// last write wins (tests re-point it at their own instance).
    pub fn init_global(self: &Arc<Self>) {
        *GLOBAL_MANAGER.write().expect("global manager lock") = Some(Arc::clone(self));
    }

    /// The process-global manager, when one was published.
    pub fn global() -> Option<Arc<PortManager>> {
        GLOBAL_MANAGER.read().expect("global manager lock").clone()
    }

    /// Lists the ports the backend offers right now. Blocking enumeration runs off
    /// the async context.
    pub async fn list_ports(&self) -> Result<Vec<PortInfo>> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.enumerate()).await?
    }

    /// Opens `name` as a new session. Other ports' sessions keep running.
    /// Same-port rule (ticket #35): when a **live** session of `name` exists
    /// the open is refused with [`CoreError::AlreadyOpen`] — two sessions on
    /// one port surface `PortBusy` per session, never global (design-ui
    /// §3.6); a **dead** session (unplug/error) is replaced — the reconnect
    /// machine's "reopen replaces the dead session". Returns the new session
    /// id; the bus carries the matching [`Event::Port`] transitions.
    pub async fn open(&self, name: &str, config: PortConfig) -> Result<u64> {
        // Same-port bookkeeping before the OS open: refuse over a live
        // session, replace dead ones. The dead session's `close` may wait on
        // its (already finished) forwarder — keep that out of the lock.
        let dead: Vec<u64> = {
            let sessions = self.sessions.lock().await;
            for session in sessions.values() {
                if session.port == name && session.is_alive() {
                    tracing::info!(session = session.id, port = name, "open refused: port already open");
                    return Err(CoreError::AlreadyOpen(name.to_string()));
                }
            }
            sessions
                .values()
                .filter(|session| session.port == name)
                .map(|session| session.id)
                .collect()
        };
        for id in dead {
            if let Some(previous) = self.sessions.lock().await.remove(&id) {
                tracing::info!(session = id, port = name, "replacing dead session");
                previous.close().await;
            }
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
        *self.current.lock().await = Some(id);
        self.sessions.lock().await.insert(id, session);

        tracing::info!(session = id, port = name, "session open");
        Ok(id)
    }

    /// Closes the session `id`, if it exists. Never hangs: see
    /// [`Session::close`]. Publishes the terminal [`PortState::Closed`] itself:
    /// the explicit event is what tells the reconnect machine (and the UI)
    /// that the *user* closed — the disarm condition of auto-reconnect.
    pub async fn close_session(&self, id: u64) {
        let session = self.sessions.lock().await.remove(&id);
        if let Some(session) = session {
            let (id, port) = (session.id, session.port.clone());
            session.close().await;
            self.publish_port(id, &port, PortState::Closed, None, None);
        }
        let mut current = self.current.lock().await;
        if *current == Some(id) {
            *current = None;
        }
    }

    /// Closes the most recently opened session (the legacy single-session
    /// close).
    pub async fn close(&self) {
        let id = *self.current.lock().await;
        if let Some(id) = id {
            self.close_session(id).await;
        }
    }

    /// Closes every session (the quit guard's "Close all").
    pub async fn close_all(&self) {
        let ids: Vec<u64> = self.sessions.lock().await.keys().copied().collect();
        for id in ids {
            self.close_session(id).await;
        }
    }

    /// Whether any session is open (the legacy single-session probe).
    pub async fn is_open(&self) -> bool {
        !self.sessions.lock().await.is_empty()
    }

    /// The port name of session `id`, when it exists.
    pub async fn session_port(&self, id: u64) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(&id)
            .map(|session| session.port.clone())
    }

    /// The ids of the live sessions.
    pub async fn session_ids(&self) -> Vec<u64> {
        self.sessions.lock().await.keys().copied().collect()
    }

    /// Whether session `id` is still live (its forwarder has not ended).
    pub async fn session_alive(&self, id: u64) -> bool {
        self.sessions
            .lock()
            .await
            .get(&id)
            .is_some_and(|session| session.is_alive())
    }

    /// A snapshot of the session table for **sync** callers (the `rcom.ports`
    /// Lua API runs on arbitrary threads). `None` when the table lock is
    /// contended by an async operation — the caller surfaces that instead of
    /// blocking the Lua VM.
    pub fn try_sessions_snapshot(&self) -> Option<Vec<SessionInfo>> {
        let sessions = self.sessions.try_lock().ok()?;
        let mut list: Vec<SessionInfo> = sessions
            .values()
            .map(|session| SessionInfo {
                session_id: session.id,
                port: session.port.clone(),
                open: session.is_alive(),
            })
            .collect();
        list.sort_by_key(|info| info.session_id);
        Some(list)
    }

    /// The playback control of the session's port, when it is a mock port
    /// (design-ui §5.7: the UI's mock drawer keys off this handle).
    pub fn mock_controls(&self, session_id: u64) -> Option<MockControl> {
        // The session table is behind an async mutex; the controls live on the
        // OpenPort, which the session owns. Use try_lock so this stays callable
        // from sync contexts (the driver task holds no other lock here).
        self.sessions
            .try_lock()
            .ok()
            .and_then(|sessions| sessions.get(&session_id).and_then(|session| session.mock_control()))
    }

    /// Installs the manager-wide pre-send hook (`None` = disabled) — the
    /// legacy path; per-session hooks ([`Self::set_hook_for`]) shadow it.
    pub fn set_hook(&self, hook: Option<TxHook>) {
        self.set_hook_at(GLOBAL_HOOK, hook);
    }

    /// Installs or clears session `id`'s Lua pre-send hook (`None` = disabled).
    /// The script runs once per send (bounded by the 3 s send-VM watchdog);
    /// the hook applies from the next [`Self::write_for`] on.
    pub fn set_hook_for(&self, session_id: u64, hook: Option<TxHook>) {
        self.set_hook_at(session_id, hook);
    }

    fn set_hook_at(&self, key: u64, hook: Option<TxHook>) {
        let mut hooks = self.hooks.write().expect("hook lock");
        match hook {
            Some(hook) => {
                hooks.insert(key, Arc::new(hook));
            }
            None => {
                hooks.remove(&key);
            }
        }
    }

    /// The active hook's script for session `id` (per-session first, then the
    /// manager-wide slot), for status display.
    pub fn hook_script_for(&self, session_id: u64) -> Option<String> {
        let hooks = self.hooks.read().expect("hook lock");
        hooks
            .get(&session_id)
            .or_else(|| hooks.get(&GLOBAL_HOOK))
            .map(|hook| hook.script().to_string())
    }

    /// The manager-wide hook's script, when one is set (for status display).
    pub fn hook_script(&self) -> Option<String> {
        self.hook_script_for(GLOBAL_HOOK)
    }

    /// Queues `bytes` on the legacy current session and publishes its TX data
    /// event, through the manager-wide (or session's) pre-send hook.
    pub async fn write(&self, bytes: Vec<u8>) -> Result<()> {
        let id = *self.current.lock().await;
        let Some(id) = id else {
            return Err(CoreError::NotOpen("(none)".into()));
        };
        self.write_for(id, bytes).await
    }

    /// Queues `bytes` on session `id` and publishes its TX data event. When a
    /// pre-send hook is set (the session's, else the manager-wide one), the
    /// bytes go through it first (blocking Lua, run off the async workers;
    /// llcom's 3 s watchdog bounds a runaway conversion). An empty conversion
    /// result sends nothing (llcom parity: a script returning nil sends
    /// nothing); a failed hook rejects the send with [`CoreError::Hook`] and a
    /// log row — it never panics and never writes. The TX event carries
    /// `hooked` when the hook changed the payload (design-ui §5.2).
    pub async fn write_for(&self, session_id: u64, bytes: Vec<u8>) -> Result<()> {
        let hook = {
            let hooks = self.hooks.read().expect("hook lock");
            hooks
                .get(&session_id)
                .or_else(|| hooks.get(&GLOBAL_HOOK))
                .cloned()
        };
        let (bytes, hooked) = match hook {
            Some(hook) => {
                let bus = self.bus.clone();
                let original = bytes.clone();
                let outcome =
                    tokio::task::spawn_blocking(move || hook.apply(&bytes, &bus)).await?;
                match outcome {
                    Ok(converted) => {
                        if converted.is_empty() {
                            tracing::debug!("tx hook produced no bytes; nothing sent");
                            return Ok(());
                        }
                        let hooked = converted != original;
                        (converted, hooked)
                    }
                    Err(error) => {
                        let message = error.to_string();
                        // Plain-English core diagnostics (the reconnect machine's
                        // convention); the app layers add localized framing.
                        self.bus.publish(Event::Log(LogEvent {
                            level: LogLevel::Error,
                            timestamp: chrono::Local::now(),
                            message: format!("lua tx hook failed: {message}"),
                            session_id: Some(session_id),
                        }));
                        return Err(CoreError::Hook(message));
                    }
                }
            }
            None => (bytes, false),
        };
        self.write_raw_for(session_id, bytes, hooked).await
    }

    /// Queues `bytes` on the legacy current session and publishes its TX data
    /// event, **without** the pre-send hook — the path for sends that are
    /// already final (a script's `apiSendUartData`).
    pub async fn write_raw(&self, bytes: Vec<u8>) -> Result<()> {
        let id = *self.current.lock().await;
        let Some(id) = id else {
            return Err(CoreError::NotOpen("(none)".into()));
        };
        self.write_raw_for(id, bytes, false).await
    }

    /// Queues `bytes` on session `id` and publishes its TX data event,
    /// **without** the pre-send hook, carrying `hooked` (set by the hook-aware
    /// [`Self::write_for`]; false for already-final sends).
    pub async fn write_raw_for(&self, session_id: u64, bytes: Vec<u8>, hooked: bool) -> Result<()> {
        let id = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&session_id)
                .ok_or_else(|| CoreError::NotOpen("(none)".into()))?;
            let id = session.id;
            session.write(bytes.clone())?;
            id
        };
        publish_tx(&self.bus, id, bytes, hooked);
        Ok(())
    }

    /// Publishes an RX chunk for session `id` (the script-VM feed path that
    /// synthesizes device bytes).
    pub fn publish_rx(&self, session_id: u64, bytes: Vec<u8>) {
        publish_rx(&self.bus, session_id, bytes);
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
    use crate::core::bus::{Direction, Payload};
    use crate::core::port::factory::{BackendFactory, MockMode};
    use crate::core::port::mock::{MockTrigger, MOCK_PORT_NAME};
    use crate::core::port::testing::EventTap;

    fn mock_manager(bus: EventBus) -> Arc<PortManager> {
        let backend = BackendFactory::with_mode(MockMode::All);
        Arc::new(PortManager::new(Arc::new(backend), bus))
    }

    /// THE multi-session AC (AC7's core half): two mock sessions stream at
    /// once, each write reaches only its own session, and closing one leaves
    /// the other alive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_mock_sessions_stream_without_cross_talk() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open echo");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open noisy");
        assert_ne!(a, b, "distinct sessions");
        assert_eq!(manager.session_ids().await.len(), 2);

        manager.write_for(a, b"ping-a".to_vec()).await.expect("write a");
        manager.write_for(b, b"ping-b".to_vec()).await.expect("write b");
        let tx_a = tap.next_data(a, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx_a.bytes.as_slice(), b"ping-a");
        let tx_b = tap.next_data(b, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx_b.bytes.as_slice(), b"ping-b");

        // Echoes: MOCK0 echoes a's write; b's echo would be MOCK1's (it does
        // not echo) — the key point is the session ids never swap.
        let rx_a = tap.next_data(a, Direction::Rx, std::time::Duration::from_secs(5));
        assert_eq!(rx_a.bytes.as_slice(), b"ping-a");
        assert!(rx_a.hooked == false);

        // Closing one leaves the other streaming.
        manager.close_session(a).await;
        assert_eq!(manager.session_ids().await.len(), 1);
        manager.write_for(b, b"still-open".to_vec()).await.expect("write b");
        let tx = tap.next_data(b, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"still-open");

        manager.close_all().await;
        assert!(manager.session_ids().await.is_empty());
    }

    /// The PortBusy rule (ticket #35): opening a port whose **live** session
    /// already exists is refused with `AlreadyOpen` — per session, the
    /// existing session untouched — while other ports keep opening.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opening_a_live_port_is_refused_and_leaves_the_session() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open other port");
        let busy = manager.open("MOCK0", PortConfig::new(115_200)).await.expect_err("already open");
        assert!(matches!(busy, CoreError::AlreadyOpen(_)), "{busy}");
        let ids = manager.session_ids().await;
        assert_eq!(ids.len(), 2, "no session was added: {ids:?}");
        assert!(ids.contains(&a) && ids.contains(&b));
        // The refusing open disturbed nothing: the original session writes.
        manager.write_for(a, b"still-mine".to_vec()).await.expect("write a");
        let tx = tap.next_data(a, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"still-mine");

        manager.close_all().await;
    }

    /// Re-opening a port over a **dead** session replaces it (the reconnect
    /// machine's reopen path) while other ports' sessions survive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopening_a_port_replaces_only_that_port_dead_session() {
        let bus = EventBus::new();
        let _tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open");
        let dead = manager.open("MOCK3", PortConfig::new(115_200)).await.expect("open unplug");
        // Kill the MOCK3 session (the unplug scenario's trigger, at 8× so the
        // scripted "unplugged" window is short) and wait for the manager to
        // see it dead (the forwarder ended).
        let control = loop {
            if let Some(control) = manager.mock_controls(dead) {
                break control;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        control.set_speed(8.0);
        control.trigger(MockTrigger::Unplug);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while manager.session_alive(dead).await {
            assert!(
                std::time::Instant::now() < deadline,
                "the unplug trigger never killed the session"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // The backend refuses opens while the scripted unplug window is open;
        // retry until it closes (8× ≈ 625 ms) — the machine does the same.
        let replacement = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match manager.open("MOCK3", PortConfig::new(115_200)).await {
                    Ok(id) => break id,
                    Err(CoreError::NotOpen(_))
                        if std::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    Err(error) => panic!("reopen over dead failed: {error}"),
                }
            }
        };
        assert_ne!(replacement, dead, "a fresh session id for the reopened port");
        let ids = manager.session_ids().await;
        assert_eq!(ids.len(), 2, "the dead session was replaced, others survive: {ids:?}");
        assert!(ids.contains(&a) && ids.contains(&replacement), "{ids:?}");

        manager.close_all().await;
    }

    /// The per-session hook: session a's hook rewrites a's sends only; b's
    /// sends pass through raw; the TX event carries the `hooked` flag (AC3).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_session_hooks_rewrite_only_their_session() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());
        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open");

        manager.set_hook_for(a, Some(crate::core::lua::TxHook::new("return uartData:fromHex()")));
        assert_eq!(
            manager.hook_script_for(a).as_deref(),
            Some("return uartData:fromHex()")
        );
        assert_eq!(manager.hook_script_for(b), None, "b has no hook");

        manager.write_for(a, b"414243".to_vec()).await.expect("hooked write");
        let tx = tap.next_data(a, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"ABC", "hex text must hit the wire converted");
        assert!(tx.hooked, "the rewritten TX row must carry the hook flag");

        // The mock echoes the converted bytes back as RX.
        let rx = tap.next_data(a, Direction::Rx, std::time::Duration::from_secs(5));
        assert_eq!(rx.bytes.as_slice(), b"ABC");
        assert!(!rx.hooked, "RX rows never carry the hook flag");

        manager.write_for(b, b"plain".to_vec()).await.expect("plain write");
        let tx = tap.next_data(b, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"plain");
        assert!(!tx.hooked, "b's session has no hook");

        manager.close_all().await;
    }

    /// An unrewriting hook leaves `hooked` false (the flag means the payload
    /// *changed*).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_hook_leaves_hooked_false() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());
        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open");

        manager.set_hook_for(a, Some(crate::core::lua::TxHook::new("return uartData")));
        manager.write_for(a, b"same".to_vec()).await.expect("write");
        let tx = tap.next_data(a, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"same");
        assert!(!tx.hooked, "an identity hook did not rewrite the payload");

        manager.close_all().await;
    }

    /// The legacy single-session API still works against the map (current =
    /// last opened; the manager-wide hook slot backs it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_write_and_close_target_the_current_session() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());

        let a = manager.open(MOCK_PORT_NAME, PortConfig::new(115_200)).await.expect("open");
        manager.set_hook(Some(crate::core::lua::TxHook::new("return uartData:fromHex()")));
        manager.write(b"414243".to_vec()).await.expect("legacy write");
        let tx = tap.next_data(a, Direction::Tx, std::time::Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"ABC");

        manager.close().await;
        assert!(manager.session_ids().await.is_empty());
        let error = manager.write(b"x".to_vec()).await.expect_err("closed");
        assert!(matches!(error, CoreError::NotOpen(_)));
    }

    /// Mock playback controls ride the session: pause freezes MOCK1's stream,
    /// inject lands a chunk, and an injected error kills the session with the
    /// matching port event (the mock drawer's whole vocabulary).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_controls_ride_the_session() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());
        let a = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open");

        let control = loop {
            if let Some(control) = manager.mock_controls(a) {
                break control;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(control.scenario(), crate::core::port::mock::MockScenario::Noisy);
        assert!(manager.mock_controls(999).is_none(), "unknown session");

        control.pause(true);
        control.inject_rx(b"injected".to_vec());
        // An in-flight frame may race the pause; skip matching events until
        // the injected chunk arrives.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match tap.rx.recv_timeout(left) {
                Ok(Event::Data(data))
                    if data.session_id == a
                        && data.direction == Direction::Rx
                        && data.bytes.as_slice() == b"injected" =>
                {
                    break
                }
                Ok(_) => continue,
                Err(e) => panic!("the injected chunk never arrived: {e}"),
            }
        }
        // Paused: the timeline emits nothing more for 400 ms.
        let drained = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
            let mut count = 0;
            while std::time::Instant::now() < deadline {
                match tap.rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(Event::Data(data)) if data.session_id == a && data.direction == Direction::Rx => count += 1,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            count
        };
        assert_eq!(drained, 0, "the timeline must freeze while paused");
        control.pause(false);

        // Injected error kills the session: a port Error event arrives.
        control.trigger(MockTrigger::Error);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match tap.rx.recv_timeout(left) {
                Ok(Event::Port(port)) if port.session_id == a && port.state == PortState::Error => break,
                Ok(_) => continue,
                Err(e) => panic!("no Error port event for the injected error: {e}"),
            }
        }

        manager.close_all().await;
    }

    /// Mock notes reach the bus as session-scoped log events (the +100 ms
    /// banner of every scenario).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_notes_are_session_scoped_log_rows() {
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let manager = mock_manager(bus.clone());
        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open");
        let log = tap.next_log(std::time::Duration::from_secs(3));
        assert_eq!(log.session_id, Some(a), "the note belongs to the session");
        assert!(log.message.contains("MOCK0"), "{}", log.message);
        assert!(log.message.contains("echo device"), "{}", log.message);
        manager.close_all().await;
    }

    /// Writes without a session fail as before (the hook changes nothing there).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_without_session_is_not_open() {
        let bus = EventBus::new();
        let _tap = EventTap::new(&bus);
        let manager = mock_manager(bus);
        let error = manager.write(b"x".to_vec()).await.expect_err("closed");
        assert!(matches!(error, CoreError::NotOpen(_)));
        let error = manager.write_for(7, b"x".to_vec()).await.expect_err("no session 7");
        assert!(matches!(error, CoreError::NotOpen(_)));
        assert_eq!(manager.next_session_id.load(Ordering::Relaxed), 1);
    }

    /// Payload clones share bytes (kept from the bus suite, exercised here for
    /// the manager's publish path types).
    #[test]
    fn payload_clone_shares_bytes() {
        let payload = Payload::from_vec(vec![1, 2, 3]);
        let clone = payload.clone();
        let a: *const [u8] = payload.as_slice();
        let b: *const [u8] = clone.as_slice();
        assert!(std::ptr::eq(a, b));
    }
}
