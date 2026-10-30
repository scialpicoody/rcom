//! A session: one open port, identified by a `session_id` the bus consumers filter
//! on. The session owns the reader-forwarder task (reader events → bus) and the
//! writer handle; the [`super::manager::PortManager`] creates and replaces sessions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::{OpenPort, PortConfig, PortWriter, ReadEvent};
use crate::core::bus::{publish_data, Direction, Event, EventBus, PortEvent, PortState};
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
}

impl Session {
    /// Starts a session over an opened port: spawns the task that forwards reader
    /// events onto the bus as RX data, port state and log events.
    pub fn spawn(id: u64, port: String, opened: OpenPort, config: PortConfig, bus: EventBus) -> Self {
        let OpenPort {
            mut events,
            writer,
            cancel,
        } = opened;
        let session_port = port.clone();
        let forwarder = tokio::spawn(async move {
            let report = |state, detail: Option<String>| {
                bus.publish(Event::Port(PortEvent {
                    session_id: id,
                    port: session_port.clone(),
                    state,
                    detail,
                    timestamp: chrono::Local::now(),
                }))
            };
            while let Some(event) = events.recv().await {
                match event {
                    ReadEvent::Data(bytes) => publish_data(&bus, id, Direction::Rx, bytes),
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
        });

        Self {
            id,
            port,
            config,
            writer,
            cancel,
            forwarder,
        }
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
