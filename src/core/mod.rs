//! The core of rcom: the serial engine, with **no FerroUI imports** — the UI drives it
//! over the public API of this module only (docs/design.md, "Architecture").
//!
//! - [`bus`]: the tokio broadcast bus every producer (serial, scripts, net tools) writes
//!   to and every consumer (log view, plot taps, UI) subscribes to.
//! - [`codec`]: byte-level helpers (hex, lossy UTF-8).
//! - [`error`]: the core error type.
//! - [`logstore`]: the session log — ring buffer of raw records plus a timestamped
//!   auto-save file sink.
//! - [`lua`]: the Lua runtime (llcom `sys.*` compatibility) whose output reaches the
//!   bus as script events.
//! - [`port`]: the serial layer — the [`port::SerialBackend`] trait, the real
//!   `serialport` implementation, the mock backend, the manager/session handles
//!   the UI drives, the reconnect state machine and the hotplug poller.

pub mod bus;
pub mod codec;
pub mod error;
pub mod logstore;
pub mod lua;
pub mod port;

pub use bus::{
    Event, EventBus, LogEvent, LogLevel, PortEvent, PortState, ScriptOutEvent, ScriptOutKind,
};
pub use error::{CoreError, Result};
