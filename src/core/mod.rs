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
//! - [`mqtt`]: the MQTT test tool (issue #21) — broker sessions (raw TCP or
//!   TLS) over `rumqttc`, the MQTT⇄serial bridge and the `RCOM_MQTTDEMO`
//!   self-demo against the in-process mock broker; session events reach the
//!   bus as [`bus::Event::Mqtt`].
//! - [`port`]: the serial layer — the [`port::SerialBackend`] trait, the real
//!   `serialport` implementation, the mock backend, the manager/session handles
//!   the UI drives, the reconnect state machine and the hotplug poller.

pub mod bus;
pub mod codec;
pub mod error;
pub mod logstore;
pub mod lua;
pub mod mqtt;
pub mod nettools;
pub mod port;

pub use bus::{
    Event, EventBus, LogEvent, LogLevel, PortEvent, PortState, ScriptOutEvent, ScriptOutKind,
};
pub use error::{CoreError, Result};
