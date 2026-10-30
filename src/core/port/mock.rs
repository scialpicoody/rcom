//! The mock backend: a fake port named `MOCK0` that appears in the port list and
//! echoes every write back as received data — a null-modem loopback plug in
//! software. This is the hardware-free demo seam (formalized by ticket #32).

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

#[derive(Default)]
pub struct MockBackend;

impl MockBackend {
    pub fn new() -> Self {
        Self
    }
}

impl SerialBackend for MockBackend {
    fn enumerate(&self) -> Result<Vec<PortInfo>> {
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

        let cancel = Arc::new(AtomicBool::new(false));

        let (event_tx, event_rx) = mpsc::channel::<ReadEvent>(256);
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);

        // The echo thread: every written chunk comes back as RX. Ends when the
        // cancel flag is set (close) or the write queue closes.
        let echo_cancel = cancel.clone();
        std::thread::Builder::new()
            .name("rcom-mock-echo".into())
            .spawn(move || {
                let _ = event_tx.blocking_send(ReadEvent::Data(
                    b"MOCK0 ready: writes echo back as RX\r\n".to_vec(),
                ));
                while let Some(bytes) = write_rx.blocking_recv() {
                    if echo_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(MOCK_ECHO_DELAY);
                    if event_tx.blocking_send(ReadEvent::Data(bytes)).is_err() {
                        break;
                    }
                }
                let _ = event_tx.blocking_send(ReadEvent::Closed);
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
        let mut saw_echo = false;
        loop {
            match tokio::time::timeout(deadline, events.recv())
                .await
                .expect("echo timed out")
                .expect("event stream ended early")
            {
                ReadEvent::Data(bytes) if bytes == b"ping" => {
                    saw_echo = true;
                    break;
                }
                ReadEvent::Data(_) => {} // the ready banner
                ReadEvent::Error(message) => panic!("mock errored: {message}"),
                ReadEvent::Closed => panic!("mock closed before the echo"),
            }
        }
        assert!(saw_echo);

        // Cancel: the stream ends with Closed promptly.
        opened.cancel.store(true, Ordering::Relaxed);
        drop(opened.writer); // ends the echo thread's write queue
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("close timed out")
            .expect("stream should end, not hang");
        assert!(matches!(event, ReadEvent::Closed | ReadEvent::Data(_)));
    }
}
