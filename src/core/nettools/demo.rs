//! The `RCOM_NETDEMO` autodemo: the net tools proving themselves on loopback,
//! end to end through the real hub, with the evidence in the trace log and as
//! `##` rows in the session log view.
//!
//! - `RCOM_NETDEMO=1` — TCP client ↔ TCP echo server on `127.0.0.1`, `ping`
//!   round trip.
//! - `RCOM_NETDEMO=udp` — UDP client ↔ UDP responder, `pong` round trip.
//! - `RCOM_NETDEMO=6` — the TCP round trip over IPv6 (`::1`).
//! - `RCOM_NETDEMO=tls` — TLS client ↔ TLS server (the on-demand self-signed
//!   certificate, accept-any on the client) through hub channels, `ping` round
//!   trip.
//! - `RCOM_NETDEMO=bridge` — TCP loopback plus the net⇄serial bridge through
//!   `MOCK0`: a byte written to the port travels serial RX → net → echo → net
//!   RX → port TX (both directions proven), then the bridge detaches.
//!
//! Everything runs on localhost; no external network anywhere.

use std::sync::Arc;
use std::time::Duration;

use crate::core::bus::{Direction, Event, EventBus, LogEvent, LogLevel};
use crate::core::port::manager::PortManager;
use crate::core::port::mock::MOCK_PORT_NAME;

use super::hub::NetHub;
use super::{ChannelConfig, NetDirection, NetEventKind, NetMode, NetProtocol};

/// How long any demo phase may take before the run is declared failed.
const DEMO_TIMEOUT: Duration = Duration::from_secs(20);

/// The demo mode requested by `RCOM_NETDEMO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetDemoMode {
    Off,
    Basic,
    Udp,
    V6,
    Tls,
    Bridge,
}

impl NetDemoMode {
    pub fn from_env() -> Self {
        match std::env::var("RCOM_NETDEMO").as_deref() {
            Ok("1") => Self::Basic,
            Ok("udp") => Self::Udp,
            Ok("6") => Self::V6,
            Ok("tls") => Self::Tls,
            Ok("bridge") => Self::Bridge,
            _ => Self::Off,
        }
    }
}

/// Runs the requested demo (spawn me on the core runtime). `manager` is needed
/// only by the bridge mode; other modes run without a port.
pub async fn run(bus: EventBus, hub: Arc<NetHub>, manager: Option<Arc<PortManager>>) {
    let mode = NetDemoMode::from_env();
    if mode == NetDemoMode::Off {
        return;
    }
    let outcome = tokio::time::timeout(DEMO_TIMEOUT, async {
        match mode {
            NetDemoMode::Off => Ok(()),
            NetDemoMode::Basic => tcp_loopback(&bus, &hub, "127.0.0.1").await,
            NetDemoMode::Udp => udp_loopback(&bus, &hub).await,
            NetDemoMode::V6 => tcp_loopback(&bus, &hub, "::1").await,
            NetDemoMode::Tls => tls_loopback(&bus, &hub).await,
            NetDemoMode::Bridge => match manager {
                Some(manager) => bridge_loopback(&bus, &hub, &manager).await,
                None => Err("bridge demo needs the port manager".to_string()),
            },
        }
    })
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => note(&bus, format!("AUTODEMO NET FAILED: {error}"), LogLevel::Error),
        Err(_) => note(&bus, "AUTODEMO NET FAILED: timed out".into(), LogLevel::Error),
    }
}

/// The UDP demo: a responder plus a hub client channel, one `pong` round trip.
async fn udp_loopback(bus: &EventBus, hub: &NetHub) -> Result<(), String> {
    let addr = super::channel::spawn_udp_echo("127.0.0.1:0")
        .await
        .map_err(|error| format!("UDP responder: {error}"))?;
    let channel = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Udp,
            mode: NetMode::Client,
            host: "127.0.0.1".into(),
            port: addr.port(),
            accept_any_cert: false,
        })
        .map_err(|error| format!("start UDP channel: {error}"))?;
    note(
        bus,
        format!("AUTODEMO NET: UDP client #{channel} → responder at {addr}"),
        LogLevel::Info,
    );
    // UDP has no connect event: send straight away; the responder's echo is
    // the proof.
    let mut events = bus.subscribe();
    hub.send(channel, b"pong".to_vec());
    wait_net_data(&mut events, channel, NetDirection::Rx, b"pong").await?;
    note(
        bus,
        format!("AUTODEMO NET OK: 'pong' travelled client → responder → client over {addr} (UDP loopback, channel #{channel})"),
        LogLevel::Info,
    );
    hub.stop_channel(channel);
    Ok(())
}

/// The TCP/UDP-free core of the basic and v6 demos: an echo server on `host`
/// plus a hub client channel, one `ping` round trip.
async fn tcp_loopback(bus: &EventBus, hub: &NetHub, host: &str) -> Result<(), String> {
    let addr = super::channel::spawn_tcp_echo(&format!("{host}:0"))
        .await
        .map_err(|error| format!("echo server on {host}: {error}"))?;

    let channel = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            host: host.to_string(),
            port: addr.port(),
            accept_any_cert: false,
        })
        .map_err(|error| format!("start client channel: {error}"))?;
    note(bus, format!("AUTODEMO NET: TCP client #{channel} → echo server at {addr}"), LogLevel::Info);

    // Send as soon as the channel reports connected, then expect the echo.
    let mut events = bus.subscribe();
    wait_connected(&mut events, channel).await?;
    hub.send(channel, b"ping".to_vec());
    wait_net_data(&mut events, channel, NetDirection::Rx, b"ping").await?;
    note(
        bus,
        format!("AUTODEMO NET OK: 'ping' travelled client → echo server → client over {addr} (TCP{v6} loopback, channel #{channel})",
            v6 = if host.contains(':') { " over ::1" } else { "" }),
        LogLevel::Info,
    );
    hub.stop_channel(channel);
    Ok(())
}

/// The TLS demo: a hub *server* channel with the self-signed certificate and a
/// hub client channel in accept-any mode, one `ping` round trip between them.
async fn tls_loopback(bus: &EventBus, hub: &NetHub) -> Result<(), String> {
    let server = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tls,
            mode: NetMode::Server,
            host: "127.0.0.1".into(),
            port: 0, // ephemeral; the Listening event carries the real port
            accept_any_cert: false,
        })
        .map_err(|error| format!("start TLS server channel: {error}"))?;

    let mut events = bus.subscribe();
    let local = wait_listening(&mut events, server).await?;
    note(
        bus,
        format!("AUTODEMO NET: TLS server #{server} listening on {local} (self-signed cert)"),
        LogLevel::Info,
    );

    let client = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tls,
            mode: NetMode::Client,
            host: local.ip().to_string(),
            port: local.port(),
            accept_any_cert: true,
        })
        .map_err(|error| format!("start TLS client channel: {error}"))?;
    wait_connected(&mut events, client).await?;
    hub.send(client, b"ping".to_vec());
    wait_net_data(&mut events, server, NetDirection::Rx, b"ping").await?;
    note(
        bus,
        format!(
            "AUTODEMO NET OK: 'ping' over TLS {local} — handshake + data with the self-signed \
             certificate (server #{server}, accept-any client #{client})"
        ),
        LogLevel::Info,
    );
    hub.stop_channel(client);
    hub.stop_channel(server);
    Ok(())
}

/// The bridge demo: TCP loopback + the net⇄serial bridge on `MOCK0`. `B` goes
/// out the port, comes back as serial RX, crosses the bridge to the echo
/// server, returns as net RX and is written to the port again: both bridge
/// directions proven, then the bridge detaches (before the echo loop repeats).
async fn bridge_loopback(
    bus: &EventBus,
    hub: &Arc<NetHub>,
    manager: &Arc<PortManager>,
) -> Result<(), String> {
    let addr = super::channel::spawn_tcp_echo("127.0.0.1:0")
        .await
        .map_err(|error| format!("echo server: {error}"))?;
    let session = manager.open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200)).await
        .map_err(|error| format!("open {MOCK_PORT_NAME}: {error}"))?;
    let channel = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            host: "127.0.0.1".into(),
            port: addr.port(),
            accept_any_cert: false,
        })
        .map_err(|error| format!("start client channel: {error}"))?;
    let mut events = bus.subscribe();
    wait_connected(&mut events, channel).await?;

    let bridged = hub.attach_bridge(channel).map_err(|error| format!("attach bridge: {error}"))?;
    note(
        bus,
        format!("AUTODEMO NET: bridge attached — channel #{channel} ⇄ session {bridged} ({MOCK_PORT_NAME})"),
        LogLevel::Info,
    );

    manager.write(b"B".to_vec()).await.map_err(|error| format!("serial write: {error}"))?;

    // Direction 1 (serial → net): the mock echoes B as RX, the bridge carries
    // it over TCP, the echo server sends it back — net RX "B".
    wait_net_data(&mut events, channel, NetDirection::Rx, b"B").await?;
    // Direction 2 (net → serial): that net RX went out the port as TX "B"
    // (the second Tx of "B" in this session).
    wait_second_tx(bus, session, b"B").await?;
    note(
        bus,
        format!(
            "AUTODEMO NET OK: bridge round trip 'B' proven both directions — serial RX → net → \
             echo → net RX → port TX (channel #{channel} ⇄ session {bridged}); detaching"
        ),
        LogLevel::Info,
    );
    hub.detach_bridge();
    hub.stop_channel(channel);
    manager.close().await;
    Ok(())
}

// ---- bus wait helpers ------------------------------------------------------

async fn wait_connected(events: &mut tokio::sync::broadcast::Receiver<Event>, channel: u64) -> Result<(), String> {
    wait_event(events, |event| match event {
        Event::Net(net) if net.channel_id == channel => match &net.kind {
            NetEventKind::Connected { .. } | NetEventKind::Listening { .. } => Some(Ok(())),
            NetEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
}

async fn wait_listening(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    channel: u64,
) -> Result<std::net::SocketAddr, String> {
    wait_event(events, |event| match event {
        Event::Net(net) if net.channel_id == channel => match &net.kind {
            NetEventKind::Listening { local } => Some(Ok(*local)),
            NetEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
}

async fn wait_net_data(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    channel: u64,
    direction: NetDirection,
    expected: &[u8],
) -> Result<(), String> {
    wait_event(events, |event| match event {
        Event::Net(net) if net.channel_id == channel => match &net.kind {
            NetEventKind::Data { direction: dir, bytes, .. }
                if *dir == direction && bytes.as_slice() == expected =>
            {
                Some(Ok(()))
            }
            NetEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
}

/// Waits until the *second* TX of `expected` in `session` (the first is the
/// demo's own write; the second is the bridge writing the echoed byte back).
async fn wait_second_tx(bus: &EventBus, session: u64, expected: &[u8]) -> Result<(), String> {
    let mut events = bus.subscribe();
    let mut seen = 0usize;
    wait_event(&mut events, |event| match event {
        Event::Data(data)
            if data.session_id == session
                && data.direction == Direction::Tx
                && data.bytes.as_slice() == expected =>
        {
            seen += 1;
            (seen >= 2).then_some(Ok(()))
        }
        _ => None,
    })
    .await
}

/// Drives `events` until `predicate` returns `Some`; `Some(Err)` fails the demo.
async fn wait_event<T>(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    mut predicate: impl FnMut(&Event) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let wait = async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(outcome) = predicate(&event) {
                        return outcome;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err("bus closed while waiting".to_string())
                }
            }
        }
    };
    tokio::time::timeout(DEMO_TIMEOUT, wait).await.unwrap_or_else(|_| {
        Err("the expected net event never arrived".to_string())
    })
}

/// One `##` row on the log view + the trace evidence.
fn note(bus: &EventBus, message: String, level: LogLevel) {
    tracing::info!("{message}");
    bus.publish(Event::Log(LogEvent { level, timestamp: chrono::Local::now(), message, session_id: None }));
}
