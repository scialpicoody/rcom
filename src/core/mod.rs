//! The core of rcom: the serial engine, with **no FerroUI imports** — the UI drives it
//! over the public API of this module only (docs/design.md, "Architecture").
//!
//! - [`bus`]: the tokio broadcast bus every producer (serial, scripts, net tools) writes
//!   to and every consumer (log view, plot taps, UI) subscribes to.
//! - [`codec`]: byte-level helpers (hex, lossy UTF-8).
//! - [`error`]: the core error type.
//! - [`logstore`]: the session log — ring buffer of raw records plus a timestamped
//!   auto-save file sink.
//! - [`port`]: the serial layer — the [`port::SerialBackend`] trait, the real
//!   `serialport` implementation, the mock backend, the manager/session handles
//!   the UI drives, the reconnect state machine and the hotplug poller.

pub mod bus;
pub mod codec;
pub mod error;
pub mod logstore;
pub mod port;

pub use bus::{Event, EventBus, LogEvent, LogLevel, PortEvent, PortState};
pub use error::{CoreError, Result};
