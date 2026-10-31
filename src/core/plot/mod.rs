//! The plot engine (issue #24): named value channels with bounded history, the
//! Lua protocol decoders that feed them, and the demo generator.
//!
//! Design (the AC's "render at live data rates without UI stalls"):
//!
//! - [`store`]: the process-global [`PlotStore`] — named channels, each a
//!   bounded [`ring::PointRing`] of `(t, v)` points (default 10k/channel).
//!   Producers append here directly; nothing crosses the bus per point.
//! - [`decoder`]: a decoder is a Lua script defining `decode(bytes,
//!   timestamp_ms)`; the core driver taps every RX bus event into the running
//!   decoder host, and the script plots via the [`lua_api`] table
//!   (`rcom.ui.add_point`). Decoders are ordinary scripts — persisted under
//!   `<config>/rcom/scripts/`, loadable in the script editor.
//! - [`view`]: the pure auto-scale/decimate math the UI's custom render
//!   control (app side) uses; UI pulls a [`store::PlotSnapshot`] once per pump
//!   tick (~30 fps) and draws it.
//! - [`frames`]: the demo framed protocol's Rust side (builder + reference
//!   parser); [`demo`] is the `RCOM_PLOTDEMO` generator on MOCK0.

pub mod decoder;
pub mod demo;
pub mod frames;
pub mod lua_api;
pub mod ring;
pub mod store;
pub mod view;

pub use decoder::DecoderHost;
pub use ring::PlotPoint;
pub use store::{global_store, PlotStore, PlotSnapshot, SeriesSnapshot};
