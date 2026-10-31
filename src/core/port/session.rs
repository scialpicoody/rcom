//! A session: one open port, identified by a `session_id` the bus consumers filter
//! on. The session owns the reader-forwarder task (reader events → bus) and the
//! writer handle; the [`super::manager::PortManager`] creates and replaces sessions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::mock::MockControl;
use super::{OpenPort, PortConfig, PortWriter, ReadEvent};
use crate::core::bus::{publish_rx, Event, EventBus, LogEvent, LogLevel, PortEvent, PortState};
use crate::core::error::Result;

/// Upper bound for a graceful close: the reader thread answers within one read
/// timeout; anything beyond this means something is deeply wrong and we stop
/// waiting (the threads clean up on their own — the UI is never blocked on them).
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct Session {
    pub id: u64,
    pub port: String,
    pub config: PortConfig,
    writer: PortWriter,
    cancel: Arc<AtomicBool>,
    forwarder: JoinHandle<()>,
    /// Liveness (ticket #35): `false` once the forwarder task ended — the
    /// reader hit a terminal event (error/clean close) or its stream ended.
    /// A dead session lingers in the manager's map until `open()` replaces it
    /// or the user closes it; `open()` refuses a port whose session is alive.
    alive: Arc<AtomicBool>,
    /// The playback control of a mock port (`None` for real ports) — the
    /// manager surfaces it as `mock_controls(session_id)` (design-ui §5.7).
    mock_control: Option<MockControl>,
}

/// Clears the session's liveness flag when the forwarder task ends — every
/// exit path of the task drops it.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl Session {
    /// Starts a session over an opened port: spawns the task that forwards reader
    /// events onto the bus as RX data, port state and log events.
    pub fn spawn(id: u64, port: String, opened: OpenPort, config: PortConfig, bus: EventBus) -> Self {
        let OpenPort {
            mut events,
            writer,
            cancel,
            control,
        } = opened;
        let session_port = port.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let forwarder = {
            let alive = Arc::clone(&alive);
            tokio::spawn(async move {
                let _alive_guard = AliveGuard(Arc::clone(&alive));
                let report = |state, detail: Option<String>| {
                    bus.publish(Event::Port(PortEvent {
                        session_id: id,
                        port: session_port.clone(),
                        state,
                        detail,
                        timestamp: chrono::Local::now(),
                        // Terminal events of a dying session carry no open config;
                        // the Open/Reopened events (with baud) come from the manager
                        // and the reconnect machine.
                        baud: None,
                    }))
                };
                while let Some(event) = events.recv().await {
                    match event {
                        ReadEvent::Data(bytes) => publish_rx(&bus, id, bytes),
                        ReadEvent::Note(text) => bus.publish(Event::Log(LogEvent {
                            level: LogLevel::Info,
                            timestamp: chrono::Local::now(),
                            message: text,
                            session_id: Some(id),
                        })),
                        ReadEvent::Error(detail) => {
                            report(PortState::Error, Some(detail));
                            return;
                        }
                        ReadEvent::Closed => {
                            report(PortState::Closed, None);
                            return;
                        }
                    }
                }
                // The stream ended without a terminal event (all senders dropped): report
                // a plain close so consumers never wait for a state that never comes.
                report(PortState::Closed, None);
            })
        };

        Self {
            id,
            port,
            config,
            writer,
            cancel,
            forwarder,
            alive,
            mock_control: control,
        }
    }

    /// Whether the session is still live: its forwarder has not ended (no
    /// terminal reader event yet). A dead session stays in the manager's map
    /// until [`super::manager::PortManager::open`] replaces it or the user
    /// closes it (the reconnect machine's contract).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The playback control of this session's port, when it is a mock port.
    pub fn mock_control(&self) -> Option<MockControl> {
        self.mock_control.clone()
    }

    /// Queues `bytes` for transmission and publishes the TX data event. The TX event
    /// is published when the bytes are accepted onto the port's write queue — before
    /// the wire confirms anything; that matches what a serial log means by TX.
    pub fn write(&self, bytes: Vec<u8>) -> Result<()> {
        self.writer.try_write(bytes)?;
        Ok(())
    }

    /// Closes the session: cancels the reader, drops the writer (which ends the
    /// port's writer thread) and waits (bounded) for the forwarder to publish the
    /// terminal port event.
    pub async fn close(self) {
        let Session {
            cancel,
            writer,
            forwarder,
            ..
        } = self;
        cancel.store(true, Ordering::Relaxed);
        drop(writer); // disconnects the write queue: the writer thread stops
        // The forwarder publishes Closed/Error itself; the timeout only guards a
        // wedged runtime, never blocks the UI path longer than CLOSE_TIMEOUT.
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, forwarder).await;
    }
}
