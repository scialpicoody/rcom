//! The core of rcom: the serial engine, with **no FerroUI imports** — the UI drives it
//! over the public API of this module only (docs/design.md, "Architecture").
//!
//! - [`bus`]: the tokio broadcast bus every producer (serial, scripts, net tools) writes
//!   to and every consumer (log view, plot taps, UI) subscribes to.
//! - [`codec`]: byte-level helpers (hex, lossy UTF-8).
//! - [`error`]: the core error type.
//! - [`port`]: the serial layer — the [`port::SerialBackend`] trait, the real
//!   `serialport` implementation, the mock backend, and the manager/session handles
//!   the UI drives.

pub mod bus;
pub mod codec;
pub mod error;
pub mod port;

pub use bus::{Event, LogEvent, LogLevel, PortEvent, PortState};
pub use error::{CoreError, Result};
