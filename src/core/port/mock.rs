//! The mock backend: a fake port named `MOCK0` that appears in the port list and
//! echoes every write back as received data — a null-modem loopback plug in
//! software. This is the hardware-free demo seam (formalized by ticket #32).
//!
//! The plug can be pulled: [`MockPlug`] simulates an unplug/replug (contract §5d in
//! minimal form; the full `MockControl` lands with the mock-drawer ticket). While
//! unplugged, `MOCK0` disappears from the listing, `open` refuses, and a live
//! session dies with a reader error — exactly what a real unplug does to
//! [`serial`](super::serial) sessions, so the reconnect machine and hotplug poller
//! run against the same code paths as with hardware.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{OpenPort, PortConfig, PortInfo, PortWriter, ReadEvent, SerialBackend};
use crate::core::error::Result;

/// The name the mock port presents in the port list.
pub const MOCK_PORT_NAME: &str = "MOCK0";

/// How long the mock takes to "wire" a written byte back as RX: just enough for the
/// log view to show TX and RX as two visibly distinct rows.
const MOCK_ECHO_DELAY: Duration = Duration::from_millis(50);

/// How often the echo thread re-checks its cancel/plug flags while the line is idle
/// (bounds how long an injected unplug takes to kill the session).
const MOCK_TICK: Duration = Duration::from_millis(20);

/// The shared plug state and its demo control: `unplug` pulls the plug, `replug`
/// puts it back. Clones share the state; `Send + Sync` so the app-side autodemo can
/// drive it from the core driver task.
#[derive(Clone)]
pub struct MockPlug {
    plugged: Arc<AtomicBool>,
}

impl Default for MockPlug {
    fn default() -> Self {
        Self {
            plugged: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl MockPlug {
    pub fn unplug(&self) {
        self.plugged.store(false, Ordering::Relaxed);
        tracing::info!("MOCK0 unplugged (mock injection)");
    }

    pub fn replug(&self) {
        self.plugged.store(true, Ordering::Relaxed);
        tracing::info!("MOCK0 replugged (mock injection)");
    }

    pub fn is_plugged(&self) -> bool {
        self.plugged.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct MockBackend {
    plug: MockPlug,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle to this backend's plug state (the unplug/replug demo control).
    pub fn plug(&self) -> MockPlug {
        self.plug.clone()
    }
}

impl SerialBackend for MockBackend {
    fn enumerate(&self) -> Result<Vec<PortInfo>> {
        if !self.plug.is_plugged() {
            return Ok(Vec::new());
        }
        Ok(vec![PortInfo {
            name: MOCK_PORT_NAME.into(),
            vid: None,
            pid: None,
            description: Some("rcom loopback mock".into()),
            is_mock: true,
        }])
    }

    fn open(&self, name: &str, _config: &PortConfig) -> Result<OpenPort> {
        if name != MOCK_PORT_NAME {
            return Err(crate::core::error::CoreError::NotOpen(name.to_string()));
        }
        if !self.plug.is_plugged() {
            return Err(crate::core::error::CoreError::NotOpen(
                "MOCK0 is unplugged".into(),
            ));
        }

        let cancel = Arc::new(AtomicBool::new(false));

        let (event_tx, event_rx) = mpsc::channel::<ReadEvent>(256);
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);

        // The echo thread: every written chunk comes back as RX. Ends when the
        // cancel flag is set (close), the plug is pulled (unplug injection → the
        // session dies with an error, like a real reader on a vanished port), or
        // the write queue closes.
        let echo_cancel = cancel.clone();
        let echo_plug = self.plug.clone();
        std::thread::Builder::new()
            .name("rcom-mock-echo".into())
            .spawn(move || {
                let _ = event_tx.blocking_send(ReadEvent::Data(
                    b"MOCK0 ready: writes echo back as RX\r\n".to_vec(),
                ));
                loop {
                    if echo_cancel.load(Ordering::Relaxed) {
                        let _ = event_tx.blocking_send(ReadEvent::Closed);
                        break;
                    }
                    if !echo_plug.is_plugged() {
                        let _ = event_tx.blocking_send(ReadEvent::Error(
                            "MOCK0 was unplugged".into(),
                        ));
                        break;
                    }
                    match write_rx.try_recv() {
                        Ok(bytes) => {
                            std::thread::sleep(MOCK_ECHO_DELAY);
                            if echo_cancel.load(Ordering::Relaxed) {
                                let _ = event_tx.blocking_send(ReadEvent::Closed);
                                break;
                            }
                            if event_tx.blocking_send(ReadEvent::Data(bytes)).is_err() {
                                break;
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {
                            std::thread::sleep(MOCK_TICK);
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            let _ = event_tx.blocking_send(ReadEvent::Closed);
                            break;
                        }
                    }
                }
            })
            .expect("spawn rcom-mock-echo");

        Ok(OpenPort {
            events: event_rx,
            writer: PortWriter { tx: write_tx },
            cancel,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The mock lists exactly the MOCK0 port, marked as mock.
    #[test]
    fn mock_lists_mock0() {
        let ports = MockBackend::new().enumerate().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, "MOCK0");
        assert!(ports[0].is_mock);
    }

    /// The demo path in miniature: open MOCK0, write `ping`, and the same bytes
    /// arrive as RX; close cancels within one echo delay (close never hangs).
    #[tokio::test]
    async fn mock_echoes_writes_back_as_rx() {
        let opened = MockBackend::new().open("MOCK0", &PortConfig::new(115_200)).unwrap();
        let mut events = opened.events;
        opened.writer.try_write(b"ping".to_vec()).unwrap();

        let deadline = tokio::time::Duration::from_secs(2);
        loop {
            match tokio::time::timeout(deadline, events.recv())
                .await
                .expect("echo timed out")
                .expect("event stream ended early")
            {
                ReadEvent::Data(bytes) if bytes == b"ping" => break,
                ReadEvent::Data(_) => {} // the ready banner
                ReadEvent::Error(message) => panic!("mock errored: {message}"),
                ReadEvent::Closed => panic!("mock closed before the echo"),
            }
        }

        // Cancel: the stream ends with Closed promptly.
        opened.cancel.store(true, Ordering::Relaxed);
        drop(opened.writer); // ends the echo thread's write queue
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("close timed out")
            .expect("stream should end, not hang");
        assert!(matches!(event, ReadEvent::Closed | ReadEvent::Data(_)));
    }

    /// Pulling the plug kills the live session with a reader error — the unplug
    /// injection behaves like a real vanished port (contract §5d, minimal form).
    #[tokio::test]
    async fn unplug_kills_the_live_session_with_an_error() {
        let backend = MockBackend::new();
        let plug = backend.plug();
        let mut events = backend
            .open("MOCK0", &PortConfig::new(115_200))
            .unwrap()
            .events;

        plug.unplug();
        assert!(!plug.is_plugged());

        // The reader stream ends with an Error within one mock tick (never hangs).
        let deadline = tokio::time::Duration::from_secs(2);
        loop {
            match tokio::time::timeout(deadline, events.recv())
                .await
                .expect("unplug not noticed in time")
                .expect("event stream ended early")
            {
                ReadEvent::Data(_) => {} // the ready banner
                ReadEvent::Error(message) => {
                    assert!(message.contains("unplugged"), "{message}");
                    break;
                }
                ReadEvent::Closed => panic!("unplug must end with an error, not a clean close"),
            }
        }

        plug.replug();
        assert!(plug.is_plugged());
    }

    /// While unplugged the port is delisted and refuses to open; after a replug
    /// both work again — what the hotplug poller and reconnect machine observe.
    #[test]
    fn unplug_delists_and_refuses_open() {
        let backend = MockBackend::new();
        let plug = backend.plug();

        assert_eq!(backend.enumerate().unwrap().len(), 1);
        plug.unplug();
        assert!(backend.enumerate().unwrap().is_empty());
        assert!(backend.open("MOCK0", &PortConfig::new(115_200)).is_err());
        plug.replug();
        assert_eq!(backend.enumerate().unwrap().len(), 1);
        assert!(backend.open("MOCK0", &PortConfig::new(115_200)).is_ok());
    }
}
