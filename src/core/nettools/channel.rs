//! One net channel worker per instance: the protocol × mode state machines that
//! move bytes and report progress as [`Event::Net`] bus events.
//!
//! Every channel is one supervisor task (plus one pump task per accepted
//! connection on servers) ended through a `watch`-based cancel flag — no task
//! outlives its channel (the session layer's cancel-by-drop rule, applied to
//! sockets). Errors and EOF are terminal and published as
//! [`NetEventKind::Disconnected`]; the user-stop path cancels first and then
//! aborts as a fallback, so a stuck worker cannot survive its channel.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};

use crate::core::bus::{Event, EventBus, Payload};

use super::{ChannelConfig, NetDirection, NetEvent, NetEventKind, NetMode, NetProtocol, parse_endpoint};

/// How many bytes one read step picks up (the chunk granularity of the
/// [`NetEventKind::Data`] events).
const READ_BUF: usize = 8 * 1024;

/// The cancel flag of one channel: `true` ends every task it reached.
#[derive(Clone)]
pub(crate) struct Cancel(Arc<watch::Sender<bool>>);

impl Cancel {
    pub(crate) fn new() -> (Self, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        (Self(Arc::new(tx)), rx)
    }

    pub(crate) fn cancel(&self) {
        let _ = self.0.send(true);
    }

    /// Whether the flag was raised (the supervisor uses this to leave the
    /// terminal `Disconnected` event to whoever stopped the channel).
    pub(crate) fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// A fresh receiver of the cancel flag (every task holds its own).
    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// Resolves once the flag flips to `true` (or the sender goes away).
async fn cancelled(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// The command half of a channel: sends user bytes into the worker. Kept by the
/// hub; a closed channel means the worker is gone.
pub(crate) type CmdTx = mpsc::UnboundedSender<Vec<u8>>;

/// Starts a channel worker: spawns the supervisor through `runtime` (the hub
/// calls this from the UI thread, outside any runtime context) and returns its
/// command sender and cancel flag. Progress lands on the bus as [`Event::Net`]
/// with `channel_id`; the caller publishes [`NetEventKind::Disconnected`] when
/// it stops the channel itself.
pub(crate) fn start(
    bus: EventBus,
    channel_id: u64,
    config: ChannelConfig,
    runtime: &tokio::runtime::Handle,
) -> (CmdTx, Cancel) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (cancel, cancel_rx) = Cancel::new();
    runtime.spawn(supervise(bus, channel_id, config, cmd_rx, cancel.clone(), cancel_rx));
    (cmd_tx, cancel)
}

/// The per-channel supervisor: connection setup, then the data pumps.
async fn supervise(
    bus: EventBus,
    channel_id: u64,
    config: ChannelConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: Cancel,
    cancel_rx: watch::Receiver<bool>,
) {
    let label = config.label();
    let result = run(&bus, channel_id, &config, &mut cmd_rx, cancel.clone(), cancel_rx).await;
    cancel.cancel();
    // A channel stopped by its owner already carries the owner's terminal
    // `Disconnected` event; the supervisor publishes only the endings the
    // owner did not cause (EOF, error, dropped command sender).
    if cancel.is_cancelled() {
        return;
    }
    match result {
        Ok(()) => {}
        Err(error) => {
            bus.publish(Event::Net(NetEvent {
                channel_id,
                protocol: config.protocol,
                mode: config.mode,
                timestamp: chrono::Local::now(),
                kind: NetEventKind::Error { message: error.clone() },
            }));
            tracing::warn!(channel = channel_id, "{label}: {error}");
        }
    }
    bus.publish(Event::Net(NetEvent {
        channel_id,
        protocol: config.protocol,
        mode: config.mode,
        timestamp: chrono::Local::now(),
        kind: NetEventKind::Disconnected { reason: None },
    }));
    tracing::info!(channel = channel_id, "{label}: channel closed");
}

type RunResult = Result<(), String>;

async fn run(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: Cancel,
    cancel_rx: watch::Receiver<bool>,
) -> RunResult {
    match (config.mode, config.protocol) {
        (NetMode::Client, NetProtocol::Tcp) => tcp_client(bus, channel_id, config, cmd_rx, cancel_rx).await,
        (NetMode::Client, NetProtocol::Tls) => tls_client(bus, channel_id, config, cmd_rx, cancel_rx).await,
        (NetMode::Client, NetProtocol::Udp) => udp_client(bus, channel_id, config, cmd_rx, cancel).await,
        (NetMode::Server, NetProtocol::Tcp) => tcp_server(bus, channel_id, config, cmd_rx, cancel).await,
        (NetMode::Server, NetProtocol::Tls) => tls_server(bus, channel_id, config, cmd_rx, cancel).await,
        (NetMode::Server, NetProtocol::Udp) => udp_server(bus, channel_id, config, cmd_rx, cancel).await,
    }
}

// ---- clients ---------------------------------------------------------------

async fn tcp_client(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel_rx: watch::Receiver<bool>,
) -> RunResult {
    let addr = parse_endpoint(&config.host, config.port).map_err(|error| error.to_string())?;
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|error| format!("connect {addr} failed: {error}"))?;
    let remote = stream.peer_addr().unwrap_or(addr);
    publish_connected(bus, channel_id, config, remote);
    let (read, write) = tokio::io::split(stream);
    pump(bus, channel_id, config, remote, read, write, cmd_rx, cancel_rx).await
}

async fn tls_client(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel_rx: watch::Receiver<bool>,
) -> RunResult {
    let addr = parse_endpoint(&config.host, config.port).map_err(|error| error.to_string())?;
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|error| format!("connect {addr} failed: {error}"))?;
    // rustls wants the bare name/IP (no brackets); the endpoint parser accepts
    // both shapes, so normalise here.
    let server_name = config.host.trim().trim_matches(['[', ']']).to_string();
    let tls = super::tls::connect_client(stream, &server_name, config.accept_any_cert).await?;
    let remote = tls.get_ref().0.peer_addr().unwrap_or(addr);
    publish_connected(bus, channel_id, config, remote);
    let (read, write) = tokio::io::split(tls);
    pump(bus, channel_id, config, remote, read, write, cmd_rx, cancel_rx).await
}

async fn udp_client(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: Cancel,
) -> RunResult {
    let addr = parse_endpoint(&config.host, config.port).map_err(|error| error.to_string())?;
    // `connect` pins the peer: replies from anyone else are filtered by the OS.
    let socket = Arc::new(
        UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
            .await
            .map_err(|error| format!("bind local UDP failed: {error}"))?,
    );
    socket.connect(addr).await.map_err(|error| format!("connect {addr} failed: {error}"))?;
    publish_connected(bus, channel_id, config, addr);
    let mut buf = vec![0u8; READ_BUF];
    loop {
        tokio::select! {
            _ = cancelled(cancel.subscribe()) => return Ok(()),
            received = socket.recv(&mut buf) => {
                match received {
                    Ok(n) => publish_data(bus, channel_id, config, addr, NetDirection::Rx, &buf[..n]),
                    Err(error) => return Err(format!("UDP receive failed: {error}")),
                }
            }
            command = cmd_rx.recv() => match command {
                Some(bytes) => {
                    if let Err(error) = socket.send(&bytes).await {
                        return Err(format!("UDP send failed: {error}"));
                    }
                    publish_data(bus, channel_id, config, addr, NetDirection::Tx, &bytes);
                }
                None => return Ok(()),
            },
        }
    }
}

// ---- servers ---------------------------------------------------------------

async fn tcp_server(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: Cancel,
) -> RunResult {
    let addr = parse_endpoint(&config.host, config.port).map_err(|error| error.to_string())?;
    let listener =
        TcpListener::bind(addr).await.map_err(|error| format!("bind {addr} failed: {error}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    publish_listening(bus, channel_id, config, local);

    // Every accepted client gets its own pump; the fan-out writes to all of
    // them (the debug-tool semantics: one channel, many clients).
    let clients: Arc<std::sync::Mutex<Vec<(SocketAddr, mpsc::UnboundedSender<Vec<u8>>)>> > =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut client_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let result = loop {
        tokio::select! {
            _ = cancelled(cancel.subscribe()) => break Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let (client_tx, client_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    clients.lock().expect("clients mutex").push((peer, client_tx));
                    publish_connected(bus, channel_id, config, peer);
                    let task = spawn_client_pump(
                        bus.clone(),
                        channel_id,
                        config.clone(),
                        peer,
                        stream,
                        client_rx,
                        Arc::clone(&clients),
                    );
                    client_tasks.push(task);
                }
                Err(error) => break Err(format!("accept failed: {error}")),
            },
            command = cmd_rx.recv() => match command {
                Some(bytes) => {
                    let snapshot: Vec<mpsc::UnboundedSender<Vec<u8>>> = clients
                        .lock()
                        .expect("clients mutex")
                        .iter()
                        .map(|(_, tx)| tx.clone())
                        .collect();
                    if snapshot.is_empty() {
                        tracing::debug!(channel = channel_id, "send with no connected client, dropped");
                    }
                    for tx in snapshot {
                        let _ = tx.send(bytes.clone());
                    }
                    publish_data(bus, channel_id, config, local, NetDirection::Tx, &bytes);
                }
                None => break Ok(()),
            },
        }
    };
    cancel.cancel();
    for task in client_tasks {
        task.abort();
    }
    result
}

async fn tls_server(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: Cancel,
) -> RunResult {
    let addr = parse_endpoint(&config.host, config.port).map_err(|error| error.to_string())?;
    let listener =
        TcpListener::bind(addr).await.map_err(|error| format!("bind {addr} failed: {error}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    let acceptor = super::tls::server_acceptor()?;
    publish_listening(bus, channel_id, config, local);

    let clients: Arc<std::sync::Mutex<Vec<(SocketAddr, mpsc::UnboundedSender<Vec<u8>>)>> > =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut client_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let result = loop {
        tokio::select! {
            _ = cancelled(cancel.subscribe()) => break Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let acceptor = acceptor.clone();
                    let bus = bus.clone();
                    let config = config.clone();
                    let clients = Arc::clone(&clients);
                    let (client_tx, client_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    clients.lock().expect("clients mutex").push((peer, client_tx));
                    publish_connected(&bus, channel_id, &config, peer);
                    let task = tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls) => {
                                pump_conn(&bus, channel_id, &config, peer, tls, client_rx).await;
                            }
                            Err(error) => {
                                tracing::debug!(channel = channel_id, "TLS accept from {peer} failed: {error}");
                            }
                        }
                        clients.lock().expect("clients mutex").retain(|(addr, _)| *addr != peer);
                    });
                    client_tasks.push(task);
                }
                Err(error) => break Err(format!("accept failed: {error}")),
            },
            command = cmd_rx.recv() => match command {
                Some(bytes) => {
                    let snapshot: Vec<mpsc::UnboundedSender<Vec<u8>>> = clients
                        .lock()
                        .expect("clients mutex")
                        .iter()
                        .map(|(_, tx)| tx.clone())
                        .collect();
                    for tx in snapshot {
                        let _ = tx.send(bytes.clone());
                    }
                    publish_data(bus, channel_id, config, local, NetDirection::Tx, &bytes);
                }
                None => break Ok(()),
            },
        }
    };
    cancel.cancel();
    for task in client_tasks {
        task.abort();
    }
    result
}

async fn udp_server(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: Cancel,
) -> RunResult {
    let addr = parse_endpoint(&config.host, config.port).map_err(|error| error.to_string())?;
    let socket = Arc::new(
        UdpSocket::bind(addr).await.map_err(|error| format!("bind {addr} failed: {error}"))?,
    );
    let local = socket.local_addr().unwrap_or(addr);
    publish_listening(bus, channel_id, config, local);

    // The peers seen so far (bounded, pruned by cap); sends fan out to all of
    // them — llcom's "server replies to the last remote" is the single-peer
    // special case.
    let peers: Arc<std::sync::Mutex<Vec<SocketAddr>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    const PEER_CAP: usize = 64;
    let mut buf = vec![0u8; READ_BUF];

    let result = loop {
        tokio::select! {
            _ = cancelled(cancel.subscribe()) => break Ok(()),
            received = socket.recv_from(&mut buf) => match received {
                Ok((n, peer)) => {
                    {
                        let mut peers = peers.lock().expect("peers mutex");
                        if !peers.contains(&peer) {
                            peers.push(peer);
                            if peers.len() > PEER_CAP {
                                peers.remove(0);
                            }
                        }
                    }
                    publish_data(bus, channel_id, config, peer, NetDirection::Rx, &buf[..n]);
                }
                Err(error) => break Err(format!("UDP receive failed: {error}")),
            },
            command = cmd_rx.recv() => match command {
                Some(bytes) => {
                    let targets = peers.lock().expect("peers mutex").clone();
                    if targets.is_empty() {
                        tracing::debug!(channel = channel_id, "UDP send with no known peer, dropped");
                    }
                    for peer in targets {
                        if let Err(error) = socket.send_to(&bytes, peer).await {
                            tracing::debug!(channel = channel_id, "UDP send to {peer} failed: {error}");
                        }
                    }
                    publish_data(bus, channel_id, config, local, NetDirection::Tx, &bytes);
                }
                None => break Ok(()),
            },
        }
    };
    cancel.cancel();
    result
}

// ---- shared plumbing -------------------------------------------------------

/// One connected stream's pump: remote → bus and commands → remote, until EOF,
/// error or cancel. Used by the client channels directly and by the TCP server
/// per client (through [`spawn_client_pump`]).
async fn pump<R, W>(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    remote: SocketAddr,
    mut read: R,
    mut write: W,
    cmd_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel_rx: watch::Receiver<bool>,
) -> RunResult
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; READ_BUF];
    let result = loop {
        tokio::select! {
            _ = cancelled(cancel_rx.clone()) => break Ok(()),
            received = read.read(&mut buf) => match received {
                Ok(0) => break Ok(()),
                Ok(n) => publish_data(bus, channel_id, config, remote, NetDirection::Rx, &buf[..n]),
                Err(error) => break Err(format!("receive from {remote} failed: {error}")),
            },
            command = cmd_rx.recv() => match command {
                Some(bytes) => {
                    if let Err(error) = write.write_all(&bytes).await {
                        break Err(format!("send to {remote} failed: {error}"));
                    }
                    let _ = write.flush().await;
                    publish_data(bus, channel_id, config, remote, NetDirection::Tx, &bytes);
                }
                None => break Ok(()),
            },
        }
    };
    result
}

/// The TCP server's per-client pump (the TLS server wraps its accept in the
/// spawned task and calls [`pump_conn`] directly).
#[allow(clippy::too_many_arguments)]
fn spawn_client_pump(
    bus: EventBus,
    channel_id: u64,
    config: ChannelConfig,
    peer: SocketAddr,
    stream: TcpStream,
    client_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    clients: Arc<std::sync::Mutex<Vec<(SocketAddr, mpsc::UnboundedSender<Vec<u8>>)>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        pump_conn(&bus, channel_id, &config, peer, stream, client_rx).await;
        clients.lock().expect("clients mutex").retain(|(addr, _)| *addr != peer);
    })
}

/// The pump for one already-established connection of any stream type.
async fn pump_conn<S>(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    peer: SocketAddr,
    stream: S,
    mut client_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut read, mut write) = tokio::io::split(stream);
    let mut buf = vec![0u8; READ_BUF];
    loop {
        tokio::select! {
            received = read.read(&mut buf) => match received {
                Ok(0) | Err(_) => break,
                Ok(n) => publish_data(bus, channel_id, config, peer, NetDirection::Rx, &buf[..n]),
            },
            command = client_rx.recv() => match command {
                Some(bytes) => {
                    if write.write_all(&bytes).await.is_err() {
                        break;
                    }
                    let _ = write.flush().await;
                    publish_data(bus, channel_id, config, peer, NetDirection::Tx, &bytes);
                }
                None => break,
            },
        }
    }
}

fn publish_connected(bus: &EventBus, channel_id: u64, config: &ChannelConfig, remote: SocketAddr) {
    bus.publish(Event::Net(NetEvent {
        channel_id,
        protocol: config.protocol,
        mode: config.mode,
        timestamp: chrono::Local::now(),
        kind: NetEventKind::Connected { remote },
    }));
}

fn publish_listening(bus: &EventBus, channel_id: u64, config: &ChannelConfig, local: SocketAddr) {
    bus.publish(Event::Net(NetEvent {
        channel_id,
        protocol: config.protocol,
        mode: config.mode,
        timestamp: chrono::Local::now(),
        kind: NetEventKind::Listening { local },
    }));
}

fn publish_data(
    bus: &EventBus,
    channel_id: u64,
    config: &ChannelConfig,
    remote: SocketAddr,
    direction: NetDirection,
    bytes: &[u8],
) {
    bus.publish(Event::Net(NetEvent {
        channel_id,
        protocol: config.protocol,
        mode: config.mode,
        timestamp: chrono::Local::now(),
        kind: NetEventKind::Data {
            direction,
            remote,
            bytes: Payload::from_vec(bytes.to_vec()),
        },
    }));
}

/// Tests and the demo need echo servers; a plain TCP echo is the workhorse.
/// Returns the bound address.
pub(crate) async fn spawn_tcp_echo(bind: &str) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    Ok(addr)
}

/// A UDP responder: echoes every datagram back to its sender.
pub(crate) async fn spawn_udp_echo(bind: &str) -> std::io::Result<SocketAddr> {
    let socket = UdpSocket::bind(bind).await?;
    let addr = socket.local_addr()?;
    tokio::spawn(async move {
        let mut buf = vec![0u8; READ_BUF];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((n, peer)) => {
                    let _ = socket.send_to(&buf[..n], peer).await;
                }
                Err(_) => return,
            }
        }
    });
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::nettools::NetDirection;

    /// Collects the net events of one channel until `predicate` is satisfied or
    /// the timeout passes. Takes a receiver the caller created BEFORE the
    /// events were published — the bus is live-only (no replay).
    async fn wait_for(
        events: &mut tokio::sync::broadcast::Receiver<Event>,
        channel: u64,
        mut predicate: impl FnMut(&NetEvent) -> bool,
    ) -> Option<NetEvent> {
        let wait = async {
            while let Ok(event) = events.recv().await {
                if let Event::Net(net) = event {
                    if net.channel_id == channel && predicate(&net) {
                        return Some(net);
                    }
                }
            }
            None
        };
        tokio::time::timeout(Duration::from_secs(5), wait).await.ok().flatten()
    }

    fn tcp_client(host: &str, port: u16) -> ChannelConfig {
        ChannelConfig {
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            host: host.into(),
            port,
            accept_any_cert: false,
        }
    }

    /// THE basic channel proof: a TCP client channel against a real echo
    /// server on 127.0.0.1 — connect, send `ping`, receive it back as net RX.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_client_channel_roundtrips_on_localhost() {
        let addr = spawn_tcp_echo("127.0.0.1:0").await.unwrap();
        let bus = EventBus::new();
        let (cmd, cancel) = start(bus.clone(), 1, tcp_client("127.0.0.1", addr.port()), &tokio::runtime::Handle::current());
        let mut events = bus.subscribe();
        wait_for(&mut events, 1, |net| matches!(net.kind, NetEventKind::Connected { .. })).await
            .expect("client must connect");
        cmd.send(b"ping".to_vec()).unwrap();
        // The sent chunk appears on the bus as net TX (the tools' traffic
        // record) and comes back from the server as net RX — one pass over the
        // same stream collects both, in either publish order.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let (mut seen_tx, mut seen_rx) = (false, false);
        let mut echo_remote = None;
        while !(seen_tx && seen_rx) {
            assert!(tokio::time::Instant::now() < deadline, "round trip incomplete: tx={seen_tx} rx={seen_rx}");
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("no event within 2s")
                .expect("bus closed");
            let Event::Net(net) = event else { continue };
            if net.channel_id != 1 {
                continue;
            }
            if let NetEventKind::Data { direction, bytes, remote } = net.kind {
                if bytes.as_slice() == b"ping" {
                    match direction {
                        NetDirection::Tx => seen_tx = true,
                        NetDirection::Rx => {
                            seen_rx = true;
                            echo_remote = Some(remote);
                        }
                    }
                }
            }
        }
        assert_eq!(echo_remote, Some(addr), "the echo carries the server's address");
        cancel.cancel();
    }

    /// The UDP client channel against a real responder: datagram out, echo back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn udp_client_channel_roundtrips_on_localhost() {
        let addr = spawn_udp_echo("127.0.0.1:0").await.unwrap();
        let bus = EventBus::new();
        let (cmd, cancel) = start(
            bus.clone(),
            2,
            ChannelConfig {
                protocol: NetProtocol::Udp,
                mode: NetMode::Client,
                host: "127.0.0.1".into(),
                port: addr.port(),
                accept_any_cert: false,
            },
            &tokio::runtime::Handle::current(),
        );
        let mut events = bus.subscribe();
        cmd.send(b"pong".to_vec()).unwrap();
        wait_for(&mut events, 2, |net| matches!(
            &net.kind,
            NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"pong"
        ))
        .await
        .expect("UDP echo must arrive as net RX");
        cancel.cancel();
    }

    /// Server mode on port 0: the `Listening` event carries the real address,
    /// a plain TCP client connects, both directions move bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_server_channel_serves_a_client() {
        let bus = EventBus::new();
        let (cmd, cancel) = start(
            bus.clone(),
            3,
            ChannelConfig {
                protocol: NetProtocol::Tcp,
                mode: NetMode::Server,
                host: "127.0.0.1".into(),
                port: 0,
                accept_any_cert: false,
            },
            &tokio::runtime::Handle::current(),
        );
        let mut events = bus.subscribe();
        let listening = wait_for(&mut events, 3, |net| matches!(net.kind, NetEventKind::Listening { .. }))
            .await
            .expect("server must listen");
        let local = match listening.kind {
            NetEventKind::Listening { local } => local,
            _ => unreachable!(),
        };
        assert_ne!(local.port(), 0, "the Listening event carries the real port");

        let mut client = TcpStream::connect(local).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        wait_for(&mut events, 3, |net| matches!(
            &net.kind,
            NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"hello"
        ))
        .await
        .expect("the server channel receives the client's bytes");

        cmd.send(b"ahoy".to_vec()).unwrap();
        let mut buf = [0u8; 16];
        client.read_exact(&mut buf[..4]).await.unwrap();
        assert_eq!(&buf[..4], b"ahoy", "the channel's send reaches the plain client");
        cancel.cancel();
    }

    /// The TLS channel: client mode with the accept-any toggle against a
    /// rustls echo server presenting the self-signed certificate — handshake
    /// and data over the channel path (the AC's SSL tool slice).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_client_channel_roundtrips_with_self_signed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = super::super::tls::server_acceptor().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = acceptor.accept(stream).await {
                let mut buf = [0u8; 64];
                if let Ok(n) = tls.read(&mut buf).await {
                    let _ = tls.write_all(&buf[..n]).await;
                    let _ = tls.flush().await;
                }
            }
        });

        let bus = EventBus::new();
        let (cmd, cancel) = start(
            bus.clone(),
            4,
            ChannelConfig {
                protocol: NetProtocol::Tls,
                mode: NetMode::Client,
                host: addr.ip().to_string(),
                port: addr.port(),
                accept_any_cert: true,
            },
            &tokio::runtime::Handle::current(),
        );
        let mut events = bus.subscribe();
        wait_for(&mut events, 4, |net| matches!(net.kind, NetEventKind::Connected { .. })).await
            .expect("TLS client must complete the handshake");
        cmd.send(b"secure".to_vec()).unwrap();
        wait_for(&mut events, 4, |net| matches!(
            &net.kind,
            NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"secure"
        ))
        .await
        .expect("TLS echo must arrive as net RX");
        cancel.cancel();
    }

    /// Stopping a channel is terminal and prompt: cancel → the worker's pump
    /// ends; a send afterwards goes nowhere (the command sender survives but
    /// the channel is logically closed from the hub side, which drops it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_channel_stops_without_leaks() {
        let addr = spawn_tcp_echo("127.0.0.1:0").await.unwrap();
        let bus = EventBus::new();
        let (_cmd, cancel) = start(bus.clone(), 5, tcp_client("127.0.0.1", addr.port()), &tokio::runtime::Handle::current());
        let mut events = bus.subscribe();
        wait_for(&mut events, 5, |net| matches!(net.kind, NetEventKind::Connected { .. })).await
            .expect("client must connect");
        cancel.cancel();
        // The supervisor leaves the terminal event to the stopper (the hub);
        // nothing publishes after cancel — give it a beat and assert quiet.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut events = bus.subscribe();
        let quiet = tokio::time::timeout(Duration::from_millis(200), events.recv()).await;
        assert!(quiet.is_err(), "no further events after cancel: {quiet:?}");
    }

    /// A channel whose target refuses the connection reports the error on the
    /// bus (nothing hangs, nothing panics).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_connection_reports_error() {
        let bus = EventBus::new();
        // Port 1 on loopback: nothing listens there.
        let (_cmd, cancel) = start(bus.clone(), 6, tcp_client("127.0.0.1", 1), &tokio::runtime::Handle::current());
        let mut events = bus.subscribe();
        let error = wait_for(&mut events, 6, |net| matches!(net.kind, NetEventKind::Error { .. }))
            .await
            .expect("the failed connect must surface as an Error event");
        let NetEventKind::Error { message } = error.kind else { unreachable!() };
        assert!(message.contains("connect"), "{message}");
        cancel.cancel();
    }
}
