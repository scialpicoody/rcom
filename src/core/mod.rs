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
//! - [`nettools`]: the network tools (issue #20) — TCP/UDP/TLS server+client
//!   channels with IPv6, the net⇄serial bridge, the `rcom.net` script API and
//!   the `RCOM_NETDEMO` self-demo; channel events reach the bus as
//!   [`bus::Event::Net`].
//! - [`port`]: the serial layer — the [`port::SerialBackend`] trait, the real
//!   `serialport` implementation, the mock backend, the manager/session handles
//!   the UI drives, the reconnect state machine and the hotplug poller.
//! - [`plot`]: the plot engine (issue #24) — the global named-channel point
//!   store, the Lua protocol-decoder host (`decode(bytes, ts)` scripts + the
//!   `rcom.ui` tap API) and the `RCOM_PLOTDEMO` generator; the UI's custom
//!   render control samples the store on its pump tick.

pub mod bus;
pub mod codec;
pub mod error;
pub mod logstore;
pub mod lua;
pub mod nettools;
pub mod plot;
pub mod port;

pub use bus::{
    Event, EventBus, LogEvent, LogLevel, PlotEvent, PlotEventKind, PortEvent, PortState,
    ScriptOutEvent, ScriptOutKind,
};
pub use error::{CoreError, Result};
