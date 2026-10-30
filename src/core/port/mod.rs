//! The serial layer (docs/design.md, "Serial layer"): the [`SerialBackend`] trait —
//! enumerate ports, open one into a reader stream plus a writer — with the real
//! `serialport` implementation ([`serial`]), the hardware-free [`mock`] backend, the
//! [`manager`](self) handles the UI drives, the [`reconnect`] auto-reconnect state
//! machine and the [`hotplug`] poller.
//!
//! The design rule this layer exists for: **close never hangs.** The blocking reader
//! runs on its own thread with a short read timeout and a cancel flag, so a session
//! is always dropped within one read timeout, even on unplug.

pub mod hotplug;
pub mod manager;
pub mod mock;
pub mod reconnect;
pub mod serial;
pub mod session;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::core::error::Result;

/// A port as listed by a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The OS name of the port (`COM3`, `/dev/ttyUSB0`, or the mock's `MOCK0`).
    pub name: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    /// Human-readable description, when the OS provides one.
    pub description: Option<String>,
    /// Whether this port is provided by the mock backend (the demo seam).
    pub is_mock: bool,
}

/// The configuration of an open port. This slice: baud; line format fixed 8N1
/// (configurable line parameters come with the settings work).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortConfig {
    pub baud_rate: u32,
    /// The blocking reader's read timeout. Short by design: it bounds how long a
    /// close may take (the reader checks its cancel flag between reads).
    pub read_timeout: Duration,
}

impl PortConfig {
    pub fn new(baud_rate: u32) -> Self {
        Self {
            baud_rate,
            read_timeout: Duration::from_millis(100),
        }
    }
}

impl Default for PortConfig {
    fn default() -> Self {
        Self::new(115_200)
    }
}

/// One terminal event of a reader: a chunk of bytes, a fatal error, or the clean end
/// of the stream. Fatal errors and closure both end the stream (no events after).
#[derive(Debug)]
pub enum ReadEvent {
    Data(Vec<u8>),
    /// The reader died (unplug, driver error). The session closes; the string is a
    /// user-visible detail.
    Error(String),
    /// The reader stopped cleanly (close was requested or the writer went away).
    Closed,
}

/// The write handle of an open port: queue bytes for the port's writer thread.
/// Cheap to clone; dropping the last handle tells the writer thread to stop.
#[derive(Clone)]
pub struct PortWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl PortWriter {
    /// Queues `bytes` for transmission without blocking the caller. `Err` when the
    /// writer thread is gone (port closed) or its queue is full.
    pub fn try_write(&self, bytes: Vec<u8>) -> Result<()> {
        self.tx.try_send(bytes).map_err(|_| crate::core::error::CoreError::ChannelClosed)
    }
}

/// An opened port: the event stream of its reader plus its writer.
pub struct OpenPort {
    /// The reader's events; ends with `Error` or `Closed`.
    pub events: mpsc::Receiver<ReadEvent>,
    pub writer: PortWriter,
    /// Set to cancel the reader thread promptly (see the module doc: close never
    /// hangs). The backend's reader owns the same flag.
    pub cancel: Arc<AtomicBool>,
}

/// A port provider: enumeration and opening. Implemented by the real serial driver
/// ([`serial::SerialPortBackend`]) and the demo seam ([`mock::MockBackend`]); the
/// UI never knows which.
pub trait SerialBackend: Send + Sync {
    /// Lists the ports this backend offers right now.
    fn enumerate(&self) -> Result<Vec<PortInfo>>;

    /// Opens `name` with `config`: starts the reader and writer and returns their
    /// handles. Blocking (it touches the OS); callers run it off the async context.
    fn open(&self, name: &str, config: &PortConfig) -> Result<OpenPort>;
}
