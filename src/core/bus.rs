//! The event bus: one tokio `broadcast` channel of [`Event`] per application
//! (docs/design.md, "Event flow"). Every producer — the serial reader, the UI send
//! path, later scripts and net tools — publishes here; every consumer (the log view,
//! the reconnect state machine, plot taps) subscribes independently.
//!
//! Events are cheap to clone: byte payloads are shared through [`Payload`] (`Arc`).

use std::sync::Arc;

use chrono::{DateTime, Local};
use tokio::sync::broadcast;

/// Shared, cheap-to-clone byte payload of a data event.
#[derive(Debug, Clone)]
pub struct Payload(Arc<[u8]>);

impl Payload {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self(bytes.into())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

/// The direction of a data event relative to the local machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Bytes received from the port.
    Rx,
    /// Bytes transmitted to the port.
    Tx,
}

/// Bytes moved over a session: one chunk in one direction at one instant.
#[derive(Debug, Clone)]
pub struct DataEvent {
    pub session_id: u64,
    pub direction: Direction,
    pub timestamp: DateTime<Local>,
    pub bytes: Payload,
}

/// The lifecycle state of a session's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    Open,
    Closed,
    /// The session died on an error (unplug, driver failure). A later ticket's
    /// reconnect state machine watches for these (docs/design.md, "Serial layer").
    Error,
}

/// A port lifecycle event of a session.
#[derive(Debug, Clone)]
pub struct PortEvent {
    pub session_id: u64,
    pub port: String,
    pub state: PortState,
    /// Error detail for [`PortState::Error`], or a human note otherwise.
    pub detail: Option<String>,
    pub timestamp: DateTime<Local>,
}

/// The severity of a [`LogEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// A diagnostics message of the engine itself (distinct from `tracing`, which is for
/// developers; this is the session log a user sees).
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub level: LogLevel,
    pub timestamp: DateTime<Local>,
    pub message: String,
}

/// Everything that can happen in the engine. Later slices extend this enum
/// (`ScriptOut`, net-tool events, ...); consumers must match with a catch-all arm.
#[derive(Debug, Clone)]
pub enum Event {
    Data(DataEvent),
    Port(PortEvent),
    Log(LogEvent),
}

/// The sending half of the event bus. Clones subscribe new consumers through
/// [`EventBus::subscribe`].
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

/// How many events the bus buffers per consumer before the slowest consumer lags
/// (a lagging consumer sees [`broadcast::error::RecvError::Lagged`], never stalls
/// the producers).
const BUS_CAPACITY: usize = 4096;

impl EventBus {
    /// Creates the bus. Producers keep the [`EventBus`]; every consumer takes a
    /// [`Receiver`](broadcast::Receiver) from [`subscribe`](EventBus::subscribe).
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(BUS_CAPACITY).0,
        }
    }

    /// Subscribes a consumer to the events from this point on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publishes an event to every current consumer. With no consumer subscribed
    /// this is a no-op, not an error.
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: stamps and publishes a RX/TX chunk for `session_id`.
pub fn publish_data(bus: &EventBus, session_id: u64, direction: Direction, bytes: Vec<u8>) {
    bus.publish(Event::Data(DataEvent {
        session_id,
        direction,
        timestamp: Local::now(),
        bytes: Payload::from_vec(bytes),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events published before a consumer subscribed are not replayed to it, and
    /// events after the subscription arrive in publish order with their payloads
    /// intact (the bus is the ordering backbone of the log view).
    #[tokio::test]
    async fn events_arrive_in_publish_order_for_subscribers() {
        let bus = EventBus::new();
        bus.publish(Event::Log(LogEvent {
            level: LogLevel::Info,
            timestamp: Local::now(),
            message: "before".into(),
        }));

        let mut rx = bus.subscribe();

        publish_data(&bus, 1, Direction::Tx, vec![0x68, 0x69]);
        publish_data(&bus, 1, Direction::Rx, vec![0x70]);
        bus.publish(Event::Log(LogEvent {
            level: LogLevel::Warn,
            timestamp: Local::now(),
            message: "after".into(),
        }));

        let seen: Vec<String> = (0..3)
            .filter_map(|_| rx.try_recv().ok())
            .map(|event| match event {
                Event::Data(data) => format!(
                    "{:?}#{}#{:x?}",
                    data.direction,
                    data.session_id,
                    data.bytes.as_slice()
                ),
                Event::Log(log) => format!("log#{}", log.message),
                Event::Port(_) => "port".into(),
            })
            .collect();

        assert_eq!(
            seen,
            vec![
                "Tx#1#[68, 69]".to_string(),
                "Rx#1#[70]".to_string(),
                "log#after".to_string(),
            ]
        );
    }

    /// Cloning a payload must not copy the bytes: two payloads of the same vector
    /// point at the same allocation.
    #[test]
    fn payload_clone_shares_bytes() {
        let payload = Payload::from_vec(vec![1, 2, 3]);
        let clone = payload.clone();
        let a: *const [u8] = payload.as_slice();
        let b: *const [u8] = clone.as_slice();
        assert!(std::ptr::eq(a, b));
    }
}
