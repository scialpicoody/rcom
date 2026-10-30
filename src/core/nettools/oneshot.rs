//! The blocking one-shot requests behind the `rcom.net` script API: connect,
//! send, read everything until the remote closes or the timeout expires.
//!
//! Deliberately `std`-network (not tokio): the calls run inside Lua callbacks,
//! which are synchronous; a bounded timeout keeps the worst case short. The
//! documented contract (also shipped as the API doc in `lua_api`):
//!
//! - `rcom.net.tcp_send(host, port, data[, timeout_ms])` → `response | nil, err`
//! - `rcom.net.udp_send(host, port, data[, timeout_ms])` → `response | nil, err`
//! - `rcom.net.tls_send(host, port, data[, timeout_ms])` → `response | nil, err`
//!   (TLS one-shots accept self-signed certificates — debugging tool parity;
//!   there is no verify mode on this one-shot).
//!
//! `timeout_ms` defaults to 2000; the response is the raw bytes as a Lua
//! string (no decoding — scripts own their codecs).

use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;

use super::parse_endpoint;

/// The default one-shot timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;

/// The whole TCP exchange (connect → send → read-until-quiet).
pub fn tcp_send(host: &str, port: u16, data: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    let addr = parse_endpoint(host, port).map_err(|error| format!("resolve {host}:{port}: {error}"))?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|error| format!("connect {addr}: {error}"))?;
    exchange(stream, data, timeout)
}

/// The whole UDP exchange (send one datagram → read the responses until quiet).
pub fn udp_send(host: &str, port: u16, data: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    let addr = parse_endpoint(host, port).map_err(|error| format!("resolve {host}:{port}: {error}"))?;
    let local = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket =
        UdpSocket::bind(local).map_err(|error| format!("bind local UDP: {error}"))?;
    socket.connect(addr).map_err(|error| format!("connect {addr}: {error}"))?;
    socket.set_read_timeout(Some(timeout)).map_err(|error| format!("set timeout: {error}"))?;
    socket.send(data).map_err(|error| format!("send: {error}"))?;
    read_until_quiet(
        |buf| socket.recv(buf).map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error)),
        timeout,
    )
}

/// The whole TLS exchange over a blocking rustls stream (self-signed accepted).
pub fn tls_send(host: &str, port: u16, data: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    let addr = parse_endpoint(host, port).map_err(|error| format!("resolve {host}:{port}: {error}"))?;
    let name = ServerName::try_from(host.trim().trim_matches(['[', ']']).to_string())
        .map_err(|error| format!("invalid TLS server name {host:?}: {error}"))?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|error| format!("connect {addr}: {error}"))?;
    let config = super::tls::client_config_any()?;
    let connection = rustls::ClientConnection::new(config, name)
        .map_err(|error| format!("TLS setup: {error}"))?;
    let tls = rustls::StreamOwned::new(connection, stream);
    exchange(tls, data, timeout)
}

/// What the exchange loop needs from a stream: read, write, and adjustable
/// timeouts (implemented for the TCP stream and the rustls-wrapped one).
trait NetStream: std::io::Read + std::io::Write {
    fn set_read_deadline(&mut self, timeout: Duration) -> std::io::Result<()>;
    fn set_write_deadline(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl NetStream for TcpStream {
    fn set_read_deadline(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(timeout))
    }

    fn set_write_deadline(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.set_write_timeout(Some(timeout))
    }
}

impl NetStream for rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    fn set_read_deadline(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.sock.set_read_timeout(Some(timeout))
    }

    fn set_write_deadline(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.sock.set_write_timeout(Some(timeout))
    }
}

/// Writes `data`, then reads until EOF or until `timeout` passes without new
/// bytes; everything received is concatenated.
fn exchange<S: NetStream>(mut stream: S, data: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    stream.set_read_deadline(timeout).map_err(|error| format!("set timeout: {error}"))?;
    stream.set_write_deadline(timeout).map_err(|error| format!("set timeout: {error}"))?;
    stream.write_all(data).map_err(|error| format!("send: {error}"))?;
    stream.flush().map_err(|error| format!("flush: {error}"))?;
    let mut received = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if let Err(error) = stream.set_read_deadline(remaining.max(Duration::from_millis(1))) {
            return Err(format!("set timeout: {error}"));
        }
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF: the remote is done
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                break; // quiet until the deadline: that is the response
            }
            Err(error) => {
                if received.is_empty() {
                    return Err(format!("receive: {error}"));
                }
                break;
            }
        }
    }
    Ok(received)
}

/// UDP has no EOF; read until the quiet window is the timeout itself.
fn read_until_quiet(
    mut recv: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let mut received = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match recv(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                break;
            }
            Err(error) => {
                if received.is_empty() {
                    return Err(format!("receive: {error}"));
                }
                break;
            }
        }
    }
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// A one-shot against a real TCP server that replies and closes: fast
    /// (EOF ends the read) and byte-exact.
    #[test]
    fn tcp_send_roundtrip_with_reply_and_close() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(&buf[..n]).unwrap();
            // Drop = close = EOF for the one-shot.
        });
        let response = tcp_send("127.0.0.1", addr.port(), b"ping", Duration::from_secs(2))
            .expect("one-shot must succeed");
        assert_eq!(response, b"ping");
    }

    /// A server that answers without closing: the one-shot waits out the
    /// (short) quiet window and still returns the reply.
    #[test]
    fn tcp_send_returns_after_quiet_window() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let _n = stream.read(&mut buf).unwrap();
            stream.write_all(b"ack").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let started = Instant::now();
        let response = tcp_send("127.0.0.1", addr.port(), b"ping", Duration::from_millis(150))
            .expect("one-shot must succeed");
        assert_eq!(response, b"ack");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Unreachable ports error, they do not hang for the full timeout.
    #[test]
    fn tcp_send_errors_on_refused() {
        // Port 1 on loopback: nothing listens there in the test env.
        let result = tcp_send("127.0.0.1", 1, b"x", Duration::from_millis(500));
        assert!(result.is_err());
    }

    /// The UDP one-shot gets its datagram echoed.
    #[test]
    fn udp_send_roundtrip() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (n, peer) = socket.recv_from(&mut buf).unwrap();
            socket.send_to(&buf[..n], peer).unwrap();
        });
        let response = udp_send("127.0.0.1", addr.port(), b"pong", Duration::from_secs(2))
            .expect("udp one-shot must succeed");
        assert_eq!(response, b"pong");
    }

    /// IPv6 literals work end to end on the one-shot path (the AC: IPv6 must
    /// work — this exercises resolve + connect + exchange over ::1).
    #[test]
    fn tcp_send_over_ipv6_loopback() {
        let listener = match TcpListener::bind("[::1]:0") {
            Ok(listener) => listener,
            Err(_) => return, // no IPv6 in this environment: skip quietly
        };
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(&buf[..n]).unwrap();
        });
        let response = tcp_send("::1", addr.port(), b"v6", Duration::from_secs(2))
            .expect("ipv6 one-shot must succeed");
        assert_eq!(response, b"v6");
    }

    /// The TLS one-shot talks to a rustls server presenting the self-signed
    /// certificate (accept-any is the one-shot's documented mode). The server
    /// side is the tokio acceptor; the one-shot itself blocks a worker thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_send_roundtrip_with_self_signed_server() {
        super::super::tls::ensure_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = super::super::tls::server_acceptor().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((stream, _)) = listener.accept().await else { return };
            if let Ok(mut tls) = acceptor.accept(stream).await {
                let mut buf = [0u8; 64];
                if let Ok(n) = tls.read(&mut buf).await {
                    let _ = tls.write_all(&buf[..n]).await;
                    let _ = tls.flush().await;
                }
            }
        });
        let response = tokio::task::spawn_blocking(move || {
            tls_send("127.0.0.1", addr.port(), b"secure", Duration::from_secs(3))
        })
        .await
        .unwrap()
        .expect("tls one-shot must succeed");
        assert_eq!(response, b"secure");
    }
}
