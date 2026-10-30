//! The net tools (docs/design.md, "nettools"): TCP/UDP/TLS server+client channels
//! with IPv6 support, the net⇄serial bridge, the `rcom.net` script API and the
//! `RCOM_NETDEMO` self-demo — the llcom network-tools parity slice (issue #20).
//!
//! Layout: [`channel`] runs one worker per instance (protocol × mode) and reports
//! progress as [`Event::Net`] on the bus (the UI↔Core contract §1.5 sketch,
//! refined here — the ticket owns the final schema); [`hub`] is the registry the
//! page and the bridge drive; [`bridge`] glues a channel to the serial session's
//! RX/TX events; [`tls`] generates the self-signed server certificate on demand
//! (rcgen) and builds the rustls configs, including the debugging-tool
//! "accept any certificate" client; [`lua_api`] installs the sandboxed `rcom.net`
//! one-shot table next to the other Lua builtins; [`oneshot`] is the blocking
//! std-network implementation behind it; [`demo`] is `RCOM_NETDEMO`.
//!
//! Threading: everything here is core-side tokio; the UI reaches it through
//! [`NetHub`], whose methods are sync (they only spawn tasks on the stored
//! runtime handle) and therefore callable from the FerroUI thread.

pub mod bridge;
pub mod channel;
pub mod demo;
pub mod hub;
pub mod lua_api;
pub mod oneshot;
pub mod tls;

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use chrono::{DateTime, Local};

use crate::core::bus::Payload;

/// The transport of a net channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetProtocol {
    Tcp,
    Udp,
    /// TCP wrapped in TLS (client and server sides both supported; the server
    /// presents the on-demand self-signed certificate).
    Tls,
}

impl NetProtocol {
    /// Short label for log rows and the page.
    pub fn label(self) -> &'static str {
        match self {
            NetProtocol::Tcp => "TCP",
            NetProtocol::Udp => "UDP",
            NetProtocol::Tls => "TLS",
        }
    }

    /// The protocols the tools page offers, in selector order.
    pub fn all() -> [NetProtocol; 3] {
        [NetProtocol::Tcp, NetProtocol::Udp, NetProtocol::Tls]
    }
}

/// Whether a channel dials out (client) or listens (server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    Client,
    Server,
}

impl NetMode {
    pub fn label(self) -> &'static str {
        match self {
            NetMode::Client => "client",
            NetMode::Server => "server",
        }
    }
}

/// The direction of a [`NetEventKind::Data`] chunk relative to the local channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetDirection {
    /// Bytes received from the remote (net RX).
    Rx,
    /// Bytes sent by the local channel (net TX).
    Tx,
}

/// The full start configuration of one channel instance.
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    pub protocol: NetProtocol,
    pub mode: NetMode,
    /// Host or IP literal (`::1` included). Servers bind this address.
    pub host: String,
    pub port: u16,
    /// TLS client only: accept any server certificate (self-signed included).
    /// This is a debugging tool (llcom parity); the toggle is explicit in the UI.
    pub accept_any_cert: bool,
}

impl ChannelConfig {
    /// The `proto mode host:port` label used by log/page rows.
    pub fn label(&self) -> String {
        format!("{} {} {}:{}", self.protocol.label(), self.mode.label(), self.host, self.port)
    }
}

/// One thing that happened on a net channel (docs/ui-drafts/05-contract.md §1.5:
/// `Event::Net(NetEvent)` — the provisional sketch refined per this ticket).
#[derive(Debug, Clone)]
pub struct NetEvent {
    /// The channel instance this event belongs to (ids are process-unique,
    /// allocated by [`hub::NetHub`], never reused).
    pub channel_id: u64,
    pub protocol: NetProtocol,
    pub mode: NetMode,
    pub timestamp: DateTime<Local>,
    pub kind: NetEventKind,
}

/// The payload of a [`NetEvent`].
#[derive(Debug, Clone)]
pub enum NetEventKind {
    /// A server channel finished binding and is listening (`local` = the bound
    /// address — with port `0` in the config this carries the real port).
    Listening { local: SocketAddr },
    /// A client channel connected, or a server channel accepted a remote.
    Connected { remote: SocketAddr },
    /// The channel ended (remote closed, connection failed, or the user
    /// stopped it — see `reason`). Terminal for that channel id.
    Disconnected { reason: Option<String> },
    /// The channel failed to start (bad address, bind refused, TLS setup).
    /// Terminal; always followed by [`NetEventKind::Disconnected`].
    Error { message: String },
    /// One chunk in one direction. `remote` is the peer the chunk moved over
    /// (for servers this identifies the client within the chunk).
    Data { direction: NetDirection, remote: SocketAddr, bytes: Payload },
}

/// Resolves `host:port` into one socket address, with IPv6 literals first-class:
/// `::1`, `[::1]` and DNS names all work (design.md: "TCP/UDP/SSL server+client,
/// IPv6"). IP literals never touch the resolver; names go through
/// `to_socket_addrs` (bracketed by this function — a bare `::1`-style literal is
/// handled above, so the resolver only ever sees valid `host:port` shapes).
pub fn parse_endpoint(host: &str, port: u16) -> io::Result<SocketAddr> {
    let trimmed = host.trim();
    // Unbracketed IPv6 literal (or any IP literal): construct directly.
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let stripped = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(ip) = stripped.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    // Name (or something we did not recognise): let the system resolver see a
    // well-formed `host:port`. Bracket IPv6 literals that also carry a zone
    // (`fe80::1%en0`) stay bracketed for the resolver.
    let formatted = if stripped != trimmed {
        format!("[{stripped}]:{port}")
    } else if trimmed.contains(':') {
        format!("[{trimmed}]:{port}")
    } else {
        format!("{trimmed}:{port}")
    };
    formatted
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no address for {host}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IPv6 literals parse to their socket form — bracketed and bare (the AC:
    /// IPv6 addresses must work).
    #[test]
    fn parses_ipv6_literals() {
        assert_eq!(parse_endpoint("::1", 80).unwrap(), "[::1]:80".parse().unwrap());
        assert_eq!(parse_endpoint("[::1]", 80).unwrap(), "[::1]:80".parse().unwrap());
        assert_eq!(
            parse_endpoint("2001:db8::1", 8080).unwrap(),
            "[2001:db8::1]:8080".parse().unwrap()
        );
    }

    /// IPv4 literals and bracketed IPv4 (tolerated) parse without the resolver.
    #[test]
    fn parses_ipv4_literals() {
        assert_eq!(
            parse_endpoint("127.0.0.1", 9000).unwrap(),
            "127.0.0.1:9000".parse().unwrap()
        );
        assert_eq!(
            parse_endpoint("[127.0.0.1]", 9000).unwrap(),
            "127.0.0.1:9000".parse().unwrap()
        );
    }

    /// Garbage input is an error, not a panic.
    #[test]
    fn rejects_garbage_hosts() {
        assert!(parse_endpoint("", 80).is_err());
        assert!(parse_endpoint("not a host!!", 80).is_err());
    }

    /// Port 0 stays representable (servers use it for ephemeral binds; the
    /// `Listening` event then carries the real port).
    #[test]
    fn parses_ephemeral_port() {
        assert_eq!(parse_endpoint("::1", 0).unwrap().port(), 0);
    }
}
