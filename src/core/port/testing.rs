//! Test-only helpers for the port layer: an event-bus tap that splits the
//! broadcast stream into a channel tests can await with timeouts. Shared by
//! the manager and reconnect suites (the same multi-session ACs prove from
//! both sides).

#![cfg(test)]

use std::time::{Duration, Instant};

use crate::core::bus::{DataEvent, Direction, Event, EventBus, LogEvent, PortEvent};

/// Drains the bus in the background into a channel the test awaits.
pub(crate) struct EventTap {
    pub rx: std::sync::mpsc::Receiver<Event>,
    _keep: tokio::task::JoinHandle<()>,
}

impl EventTap {
    pub fn new(bus: &EventBus) -> Self {
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

    /// The next data event for `session_id` in `direction`.
    pub fn next_data(&self, session_id: u64, direction: Direction, within: Duration) -> DataEvent {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Event::Data(data))
                    if data.session_id == session_id && data.direction == direction =>
                {
                    return data
                }
                Ok(_) => continue,
                Err(e) => panic!(
                    "no {direction:?} event for session {session_id} within {within:?}: {e}; \
                     events seen: {:?}",
                    self.dump_debug()
                ),
            }
        }
    }

    /// A debug dump of the events queued but not yet consumed (timeout diagnostics).
    fn dump_debug(&self) -> Vec<String> {
        self.rx
            .try_iter()
            .map(|event| match event {
                Event::Data(data) => format!(
                    "Data session={} {:?} {}B hooked={}",
                    data.session_id,
                    data.direction,
                    data.bytes.len(),
                    data.hooked,
                ),
                Event::Port(port) => format!("Port session={} {} {:?}", port.session_id, port.port, port.state),
                Event::Log(log) => format!("Log session={:?}: {}", log.session_id, log.message),
                Event::ScriptOut(out) => format!(
                    "ScriptOut session={:?} kind={kind:?}",
                    out.session_id,
                    kind = out.kind,
                ),
                Event::Net(_) => "Net".into(),
                Event::Mqtt(_) => "Mqtt".into(),
                Event::Plot(_) => "Plot".into(),
                Event::Monitor(_) => "Monitor".into(),
            })
            .collect()
    }

    /// The next log message.
    pub fn next_log(&self, within: Duration) -> LogEvent {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Event::Log(log)) => return log,
                Ok(_) => continue,
                Err(e) => panic!("no log event within {within:?}: {e}"),
            }
        }
    }

    /// The next port event for `session_id` in `state` (`None` = any session).
    pub fn next_port_state(
        &self,
        session_id: Option<u64>,
        state: crate::core::bus::PortState,
        within: Duration,
    ) -> PortEvent {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Event::Port(port))
                    if port.state == state && session_id.is_none_or(|id| port.session_id == id) =>
                {
                    return port
                }
                Ok(_) => continue,
                Err(e) => panic!("no {state:?} port event within {within:?}: {e}"),
            }
        }
    }
}
