//! The port manager: the one handle the UI drives. It owns the backend, hands out
//! session ids, and keeps the current session. This slice carries a single session
//! at a time (design.md's tab-per-session multi-session arrives with the session
//! tabs ticket); the API shape (`open` returning a session id, bus events tagged
//! with it) is already multi-session ready.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use super::session::Session;
use super::{PortConfig, PortInfo, SerialBackend};
use crate::core::bus::{publish_data, Direction, Event, EventBus, PortEvent, PortState};
use crate::core::error::{CoreError, Result};

pub struct PortManager {
    backend: Arc<dyn SerialBackend>,
    bus: EventBus,
    next_session_id: AtomicU64,
    session: Mutex<Option<Session>>,
}

impl PortManager {
    pub fn new(backend: Arc<dyn SerialBackend>, bus: EventBus) -> Self {
        Self {
            backend,
            bus,
            next_session_id: AtomicU64::new(1),
            session: Mutex::new(None),
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

    /// Queues `bytes` on the current session and publishes its TX data event.
    pub async fn write(&self, bytes: Vec<u8>) -> Result<()> {
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
