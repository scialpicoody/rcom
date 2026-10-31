//! The serial monitor (docs/design.md, "Serial monitor"; issue #23): rcom as the
//! man in the middle of another program's serial traffic — llcom's Windows-only
//! 串口监听, rebuilt honestly for the rest of us.
//!
//! Platform story, decided by the license gate recorded on the ticket:
//! - **Windows: skipped.** llcom's bundled `serial_monitor.dll` is a closed-source
//!   binary blob with no license statement anywhere in its repo (the in-tree
//!   `serial_monitor_rs` rewrite calls the original "closed-source" verbatim), so
//!   shipping FFI against it fails the design.md gate ("its license must be
//!   verified first"). The fallback is the revisit path: build the Apache-2.0
//!   `serial_monitor_rs` rewrite from source instead of shipping the blob.
//! - **Unix: pty-pair proxy** ([`pty`]) — rcom creates a pty and exposes the
//!   slave as a virtual port path (`/dev/ttys…`). The target app opens that path;
//!   rcom sits on the master side and relays every byte between it and the
//!   configured upstream — a real serial port, or a second virtual pty when no
//!   hardware is at hand ([`Upstream::VirtualLoopback`]). All relayed traffic is
//!   published as [`Event::Monitor`](crate::core::bus::Event::Monitor) with
//!   direction + timestamps.
//!
//! Honesty rule (also stated on the monitor page): the monitor sees **what flows
//! through rcom's relay** — it is not a passive wire tap; traffic that never
//! passes the virtual port is invisible to it.
//!
//! Layout: [`pty`] runs one relay thread per monitor (blocking fds + `poll`, the
//! port layer's "close never hangs" discipline), [`hub`] is the registry the page
//! and the demo drive, [`demo`] is `RCOM_MONITORDEMO`. Threading: the hub's
//! methods are sync and callable from the FerroUI thread; the relay thread only
//! touches the bus (a `broadcast::Sender` is `Send + Sync`).

pub mod demo;
pub mod hub;
#[cfg(unix)]
pub mod pty;

use chrono::{DateTime, Local};

use crate::core::bus::Payload;

/// Where the relay forwards the captured traffic to (and reads from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// A second virtual pty: the loopback pair. No hardware needed — the
    /// "device side" is another `/dev/ttys…` path anything can open (the demo,
    /// a device emulator, a test). Both ends virtual.
    VirtualLoopback,
    /// A real serial port, opened by the relay at `baud` for the duration of the
    /// monitor. The target app then talks to the actual device through rcom.
    Real { port: String, baud: u32 },
}

impl Upstream {
    /// The label rows and the page show for the upstream.
    pub fn label(&self) -> String {
        match self {
            Upstream::VirtualLoopback => "loopback (virtual)".to_string(),
            Upstream::Real { port, baud } => format!("{port} @ {baud}"),
        }
    }
}

/// The start configuration of one monitor instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorConfig {
    pub upstream: Upstream,
}

/// The direction of a captured chunk, **relative to the monitored app** (llcom's
/// `comm_state` 3=Receive / 4=Send semantics): the question a user watching
/// another program asks is "what did it send / what did it get".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorDirection {
    /// The app transmitted: bytes written into the virtual port, relayed out the
    /// upstream.
    Tx,
    /// The app received: bytes from the upstream, relayed into the virtual port.
    Rx,
}

impl MonitorDirection {
    /// The arrow of a captured chunk: `→` app→device, `←` device→app.
    pub fn arrow(self) -> &'static str {
        match self {
            MonitorDirection::Tx => "→",
            MonitorDirection::Rx => "←",
        }
    }

    /// The RX/TX tag the log rows and the durable record use.
    pub fn tag(self) -> &'static str {
        match self {
            MonitorDirection::Tx => "TX",
            MonitorDirection::Rx => "RX",
        }
    }
}

/// One thing that happened on a monitor (the UI↔Core contract's provisional
/// `Event::Monitor(MonitorEvent)` sketch, refined per this ticket).
#[derive(Debug, Clone)]
pub struct MonitorEvent {
    /// The monitor instance this event belongs to (ids are process-unique,
    /// allocated by [`hub::MonitorHub`], never reused).
    pub monitor_id: u64,
    pub timestamp: DateTime<Local>,
    pub kind: MonitorEventKind,
}

/// The payload of a [`MonitorEvent`].
#[derive(Debug, Clone)]
pub enum MonitorEventKind {
    /// The relay is live: `virtual_port` is the path the target app opens; for
    /// [`Upstream::VirtualLoopback`] the `upstream` label carries the device-side
    /// twin path after the mode name.
    Started { virtual_port: String, upstream: String },
    /// The relay ended (user stop, upstream death, or an internal error — see
    /// `reason`). Terminal for that monitor id.
    Stopped { reason: Option<String> },
    /// The relay failed to start or hit a fatal error. Terminal; always followed
    /// by [`MonitorEventKind::Stopped`].
    Error { message: String },
    /// One captured chunk in one direction.
    Data { direction: MonitorDirection, bytes: Payload },
}
