//! A minimal MQTT 3.1.1 broker over a `TcpListener` — the loopback counterpart
//! of the tool (unit tests and `RCOM_MQTTDEMO`); no external broker is ever
//! required (mosquitto is NOT assumed present).
//!
//! Covers exactly the packet dance a test tool needs: CONNECT→CONNACK,
//! SUBSCRIBE→SUBACK (wildcard filters `+`/`#` honoured on delivery),
//! PUBLISH→forward-to-matching-subscribers (broker echoes the publisher too,
//! per spec), PINGREQ→PINGRESP, DISCONNECT (recorded as the clean-disconnect
//! proof). Publisher-supplied QoS is granted verbatim on SUBACK; the broker
//! itself publishes at QoS 0.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// One broker-side connection: its writer channel and its subscriptions.
struct Conn {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    filters: Vec<(String, u8)>,
}

/// Shared broker state.
struct Inner {
    conns: Mutex<HashMap<SocketAddr, Conn>>,
    /// Every publish the broker received from clients, in order (the tests and
    /// the demo poll this log — robust against subscribing late).
    received: Mutex<Vec<(String, Vec<u8>)>>,
    clean_disconnect: std::sync::atomic::AtomicBool,
    connect_count: AtomicU32,
    publish_count: AtomicU32,
}

/// The test/demo handle of a running mock broker.
#[derive(Clone)]
pub struct MockBroker {
    addr: SocketAddr,
    inner: Arc<Inner>,
}

impl MockBroker {
    /// Spawns a plain-TCP broker on `127.0.0.1:0`.
    pub async fn spawn() -> std::io::Result<Self> {
        spawn_on("127.0.0.1:0").await
    }

    /// Spawns a TLS broker (the nettools self-signed certificate) on
    /// `127.0.0.1:0` — the counterpart for the tool's TLS mode.
    pub async fn spawn_tls() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind mock broker: {error}"))?;
        let addr = listener
            .local_addr()
            .map_err(|error| format!("mock broker address: {error}"))?;
        let acceptor = crate::core::nettools::tls::server_acceptor()?;
        let broker = MockBroker {
            addr,
            inner: Arc::new(Inner {
                conns: Mutex::new(HashMap::new()),
                received: Mutex::new(Vec::new()),
                clean_disconnect: std::sync::atomic::AtomicBool::new(false),
                connect_count: AtomicU32::new(0),
                publish_count: AtomicU32::new(0),
            }),
        };
        let inner = Arc::clone(&broker.inner);
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else { return };
                let acceptor = acceptor.clone();
                let inner = Arc::clone(&inner);
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        serve(tls, peer, inner).await;
                    }
                });
            }
        });
        Ok(broker)
    }

    /// The address clients connect to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The broker port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Whether the last disconnect on record was a graceful MQTT DISCONNECT
    /// (the clean-lifecycle proof).
    pub fn clean_disconnect(&self) -> bool {
        self.inner.clean_disconnect.load(Ordering::Relaxed)
    }

    /// How many CONNECTs arrived (reconnect attempts included).
    pub fn connect_count(&self) -> u32 {
        self.inner.connect_count.load(Ordering::Relaxed)
    }

    /// How many PUBLISHes the broker received from clients.
    pub fn publish_count(&self) -> u32 {
        self.inner.publish_count.load(Ordering::Relaxed)
    }

    /// Subscribes the caller to every message the broker receives (live-only,
    /// like the app bus — subscribe BEFORE traffic starts).
    pub fn received_log(&self) -> Vec<(String, Vec<u8>)> {
        self.inner.received.lock().expect("broker received mutex").clone()
    }

    /// Broker-originated publish: delivered (QoS 0) to every current
    /// subscription matching `topic`.
    pub fn publish(&self, topic: &str, payload: &[u8]) {
        let packet = encode_publish(topic, payload, 0, 0);
        let conns = self.inner.conns.lock().expect("broker conns mutex");
        for conn in conns.values() {
            let matches = conn
                .filters
                .iter()
                .any(|(filter, _)| topic_matches(filter, topic));
            if matches {
                let _ = conn.writer.send(packet.clone());
            }
        }
    }

    /// Waits (bounded) until the broker has received a publish on `topic` with
    /// exactly `payload`; `false` on timeout. Polls the received log, so the
    /// call is safe before or after the traffic.
    pub async fn wait_message(&self, topic: &str, payload: &[u8], timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let hit = self
                .inner
                .received
                .lock()
                .expect("broker received mutex")
                .iter()
                .any(|(seen_topic, seen_payload)| {
                    seen_topic == topic && seen_payload.as_slice() == payload
                });
            if hit {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn spawn_on(bind: &str) -> std::io::Result<MockBroker> {
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let broker = MockBroker {
        addr,
        inner: Arc::new(Inner {
            conns: Mutex::new(HashMap::new()),
            received: Mutex::new(Vec::new()),
            clean_disconnect: std::sync::atomic::AtomicBool::new(false),
            connect_count: AtomicU32::new(0),
            publish_count: AtomicU32::new(0),
        }),
    };
    let inner = Arc::clone(&broker.inner);
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { return };
            let inner = Arc::clone(&inner);
            tokio::spawn(async move {
                serve(stream, peer, inner).await;
            });
        }
    });
    Ok(broker)
}

/// Serves one connection: the packet state machine until EOF, error, or
/// DISCONNECT.
async fn serve<S>(stream: S, peer: SocketAddr, inner: Arc<Inner>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut read, mut write) = tokio::io::split(stream);
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    inner
        .conns
        .lock()
        .expect("broker conns mutex")
        .insert(peer, Conn { writer: writer_tx.clone(), filters: Vec::new() });
    // The writer pump: every queued packet goes out in order.
    let writer_task = tokio::spawn(async move {
        while let Some(packet) = writer_rx.recv().await {
            if write.write_all(&packet).await.is_err() || write.flush().await.is_err() {
                return;
            }
        }
    });

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    'serve: loop {
        // A packet may straddle reads: parse from the buffer, refill as needed.
        while let Some((first, body)) = parse_packet(&buffer) {
            buffer.drain(..body.len() + 1 + remaining_length_size(&body));
            match first >> 4 {
                1 => {
                    // CONNECT — acknowledge unconditionally (a test-tool mock).
                    inner.connect_count.fetch_add(1, Ordering::Relaxed);
                    if writer_tx.send(vec![0x20, 0x02, 0x00, 0x00]).is_err() {
                        break 'serve;
                    }
                }
                8 => {
                    // SUBSCRIBE — record filters, grant the requested QoS.
                    if let Some((pkid, filters)) = parse_subscribe(&body) {
                        if let Some(conn) =
                            inner.conns.lock().expect("broker conns mutex").get_mut(&peer)
                        {
                            conn.filters = filters.clone();
                        }
                        let mut suback = vec![0x90];
                        let remaining = 2 + filters.len();
                        suback.extend(encode_remaining_length(remaining as u32));
                        suback.extend(pkid.to_be_bytes());
                        for (_, qos) in &filters {
                            suback.push(*qos);
                        }
                        if writer_tx.send(suback).is_err() {
                            break 'serve;
                        }
                    }
                }
                3 => {
                    // PUBLISH — record, then forward to every matching
                    // subscriber (the spec: the publisher subscribed to its own
                    // topic receives it back).
                    inner.publish_count.fetch_add(1, Ordering::Relaxed);
                    if let Some((topic, payload, qos, pkid)) = parse_publish(&body, first) {
                        inner
                            .received
                            .lock()
                            .expect("broker received mutex")
                            .push((topic.clone(), payload.clone()));
                        let packet = encode_publish(&topic, &payload, qos, pkid);
                        let conns = inner.conns.lock().expect("broker conns mutex");
                        for conn in conns.values() {
                            if conn
                                .filters
                                .iter()
                                .any(|(filter, _)| topic_matches(filter, &topic))
                            {
                                let _ = conn.writer.send(packet.clone());
                            }
                        }
                    }
                }
                12 => {
                    // PINGREQ → PINGRESP.
                    if writer_tx.send(vec![0xD0, 0x00]).is_err() {
                        break 'serve;
                    }
                }
                14 => {
                    // DISCONNECT — the graceful close.
                    inner.clean_disconnect.store(true, Ordering::Relaxed);
                    break 'serve;
                }
                _ => {}
            }
        }
        match read.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    inner.conns.lock().expect("broker conns mutex").remove(&peer);
    writer_task.abort();
}

// ---- packet framing ---------------------------------------------------------

/// Parses one packet from the front of `buffer`: `(first byte, body)` when a
/// complete one is present.
fn parse_packet(buffer: &[u8]) -> Option<(u8, Vec<u8>)> {
    let (&first, rest) = buffer.split_first()?;
    let (length, size) = decode_remaining_length(rest)?;
    let total = 1 + size + length as usize;
    if buffer.len() < total {
        return None;
    }
    Some((first, buffer[1 + size..total].to_vec()))
}

/// Decodes the MQTT variable-length remaining-length field: `(value, bytes used)`.
fn decode_remaining_length(bytes: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut multiplier: u32 = 1;
    for (index, &byte) in bytes.iter().enumerate().take(4) {
        value += u32::from(byte & 0x7F) * multiplier;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
        multiplier *= 128;
    }
    None
}

/// How many bytes the length prefix of a body uses (for `drain` bookkeeping).
fn remaining_length_size(body: &[u8]) -> usize {
    // Re-encode: 1 byte up to 127, 2 up to 16383, … (test bodies stay small).
    let length = body.len();
    if length < 128 {
        1
    } else if length < 16_384 {
        2
    } else if length < 2_097_152 {
        3
    } else {
        4
    }
}

fn encode_remaining_length(length: u32) -> Vec<u8> {
    let mut encoded = Vec::new();
    let mut value = length;
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        encoded.push(byte);
        if value == 0 {
            return encoded;
        }
    }
}

/// Parses a SUBSCRIBE body: `(pkid, filters)` — topic filter + requested QoS
/// pairs.
fn parse_subscribe(body: &[u8]) -> Option<(u16, Vec<(String, u8)>)> {
    if body.len() < 2 {
        return None;
    }
    let pkid = u16::from_be_bytes([body[0], body[1]]);
    let mut filters = Vec::new();
    let mut rest = &body[2..];
    while rest.len() >= 3 {
        let length = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        if rest.len() < 2 + length + 1 {
            break;
        }
        let topic = String::from_utf8_lossy(&rest[2..2 + length]).to_string();
        let qos = rest[2 + length] & 0b11;
        filters.push((topic, qos));
        rest = &rest[2 + length + 1..];
    }
    Some((pkid, filters))
}

/// Parses a PUBLISH body: `(topic, payload, qos, pkid)`.
fn parse_publish(body: &[u8], first: u8) -> Option<(String, Vec<u8>, u8, u16)> {
    let qos = (first >> 1) & 0b11;
    if body.len() < 2 {
        return None;
    }
    let length = u16::from_be_bytes([body[0], body[1]]) as usize;
    if body.len() < 2 + length {
        return None;
    }
    let topic = String::from_utf8_lossy(&body[2..2 + length]).to_string();
    let mut rest = &body[2 + length..];
    let pkid = if qos > 0 {
        if rest.len() < 2 {
            return None;
        }
        let pkid = u16::from_be_bytes([rest[0], rest[1]]);
        rest = &rest[2..];
        pkid
    } else {
        0
    };
    Some((topic, rest.to_vec(), qos, pkid))
}

/// Encodes a broker→client PUBLISH packet.
fn encode_publish(topic: &str, payload: &[u8], qos: u8, pkid: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend((topic.len() as u16).to_be_bytes());
    body.extend_from_slice(topic.as_bytes());
    if qos > 0 {
        body.extend(pkid.to_be_bytes());
    }
    body.extend_from_slice(payload);
    let mut packet = vec![0x30 | (qos << 1)];
    packet.extend(encode_remaining_length(body.len() as u32));
    packet.extend(body);
    packet
}

/// MQTT topic-filter matching, restricted to what the tool exercises: `+` one
/// level, `#` the remaining levels (last only).
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut filter_levels = filter.split('/');
    let mut topic_levels = topic.split('/');
    loop {
        match (filter_levels.next(), topic_levels.next()) {
            (Some("#"), _) => return true, // matches the parent level too (spec)
            (Some("+"), Some(_)) => {}
            (Some("+"), None) => return false,
            (Some(f), Some(t)) if f == t => {}
            (Some(_), Some(_)) => return false,
            (Some(_), None) => return false,
            (None, Some(_)) => return false,
            (None, None) => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    /// Wildcard semantics the delivery path relies on.
    #[test]
    fn topic_filters_match_per_spec() {
        assert!(topic_matches("demo/up", "demo/up"));
        assert!(!topic_matches("demo/up", "demo/down"));
        assert!(topic_matches("demo/+", "demo/up"));
        assert!(!topic_matches("demo/+", "demo/up/extra"));
        assert!(topic_matches("demo/#", "demo/up/extra"));
        assert!(topic_matches("#", "a/b/c"));
        assert!(topic_matches("+", "a"));
        assert!(!topic_matches("+", "a/b"));
    }

    /// Remaining-length round trip across the one-, two- and three-byte ranges.
    #[test]
    fn remaining_length_round_trips() {
        for length in [0u32, 1, 127, 128, 255, 16_383, 16_384, 100_000] {
            let encoded = encode_remaining_length(length);
            assert!(encoded.len() <= 4);
            let (decoded, used) = decode_remaining_length(&encoded).expect("decodable");
            assert_eq!(decoded, length);
            assert_eq!(used, encoded.len());
        }
        assert!(decode_remaining_length(&[0x80, 0x80, 0x80, 0x80]).is_none());
    }

    /// A publish packet survives its own encoding (the broker→client path).
    #[tokio::test]
    async fn publish_packets_round_trip() {
        let packet = encode_publish("demo/up", b"ping", 0, 0);
        let (first, body) = parse_packet(&packet).expect("complete packet");
        assert_eq!(first >> 4, 3);
        let (topic, payload, qos, pkid) = parse_publish(&body, first).expect("publish");
        assert_eq!((topic.as_str(), payload.as_slice(), qos, pkid), ("demo/up", b"ping".as_slice(), 0, 0));

        let packet = encode_publish("a/b", &[1, 2, 3], 1, 7);
        let (first, body) = parse_packet(&packet).expect("complete packet");
        let (topic, payload, qos, pkid) = parse_publish(&body, first).expect("publish");
        assert_eq!((topic.as_str(), qos, pkid), ("a/b", 1, 7));
        assert_eq!(payload, vec![1, 2, 3]);
    }

    /// THE broker proof: CONNECT→CONNACK, SUBSCRIBE→SUBACK, a client publish
    /// comes back to the (subscribed) publisher, a broker publish is delivered,
    /// and DISCONNECT marks the clean close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_broker_serves_connect_subscribe_publish_disconnect() {
        let broker = MockBroker::spawn().await.expect("broker binds");
        let mut stream = TcpStream::connect(broker.addr()).await.expect("client connects");

        // CONNECT → CONNACK.
        let connect = {
            let mut body = vec![];
            body.extend((4u16).to_be_bytes());
            body.extend_from_slice(b"MQTT");
            body.push(4); // level 3.1.1
            body.push(0); // flags
            body.extend(60u16.to_be_bytes());
            body.extend((4u16).to_be_bytes());
            body.extend_from_slice(b"test");
            let mut packet = vec![0x10];
            packet.extend(encode_remaining_length(body.len() as u32));
            packet.extend(body);
            packet
        };
        stream.write_all(&connect).await.unwrap();
        let mut ack = [0u8; 4];
        stream.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack, [0x20, 0x02, 0x00, 0x00], "CONNACK success");
        assert_eq!(broker.connect_count(), 1);

        // SUBSCRIBE demo/up (QoS 0) → SUBACK granting QoS 0.
        let subscribe = {
            let mut body = vec![0u8, 1]; // pkid 1
            body.extend((7u16).to_be_bytes());
            body.extend_from_slice(b"demo/up");
            body.push(0);
            let mut packet = vec![0x82];
            packet.extend(encode_remaining_length(body.len() as u32));
            packet.extend(body);
            packet
        };
        stream.write_all(&subscribe).await.unwrap();
        let mut suback = [0u8; 5];
        stream.read_exact(&mut suback).await.unwrap();
        assert_eq!(suback[0], 0x90);
        assert_eq!(suback[1], 3, "remaining length: pkid + one return code");
        assert_eq!(u16::from_be_bytes([suback[2], suback[3]]), 1, "pkid echoed");
        assert_eq!(suback[4], 0, "granted QoS 0");

        // A PUBLISH from this client is forwarded back to it (subscribed).
        stream
            .write_all(&encode_publish("demo/up", b"ping", 0, 0))
            .await
            .unwrap();
        assert!(broker.wait_message("demo/up", b"ping", Duration::from_secs(5)).await);
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0] >> 4, 3, "the echoed publish");
        let length = header[1] as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[2..9], b"demo/up");
        assert_eq!(&body[9..], b"ping");

        // The broker's own publish lands too.
        broker.publish("demo/up", b"push");
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        let length = header[1] as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[2..9], b"demo/up");
        assert_eq!(&body[9..], b"push");

        // PINGREQ → PINGRESP.
        stream.write_all(&[0xC0, 0x00]).await.unwrap();
        let mut pong = [0u8; 2];
        stream.read_exact(&mut pong).await.unwrap();
        assert_eq!(pong, [0xD0, 0x00]);

        // DISCONNECT records the graceful close.
        assert!(!broker.clean_disconnect());
        stream.write_all(&[0xE0, 0x00]).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !broker.clean_disconnect() {
            assert!(tokio::time::Instant::now() < deadline, "DISCONNECT never recorded");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(broker.publish_count(), 1);
    }

    /// Wildcards are honoured on delivery: a `demo/#` subscription receives
    /// publishes to `demo/any/level`, and an unrelated topic stays out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_broker_delivers_wildcard_subscriptions() {
        let broker = MockBroker::spawn().await.expect("broker binds");

        let mut stream = TcpStream::connect(broker.addr()).await.unwrap();
        let connect = {
            let mut body = vec![];
            body.extend((4u16).to_be_bytes());
            body.extend_from_slice(b"MQTT");
            body.push(4);
            body.push(0);
            body.extend(60u16.to_be_bytes());
            body.extend((0u16).to_be_bytes()); // empty client id
            let mut packet = vec![0x10];
            packet.extend(encode_remaining_length(body.len() as u32));
            packet.extend(body);
            packet
        };
        stream.write_all(&connect).await.unwrap();
        let mut ack = [0u8; 4];
        stream.read_exact(&mut ack).await.unwrap();

        // Subscribe demo/# (QoS 1 — the SUBACK grants it back).
        let subscribe = {
            let mut body = vec![0u8, 2];
            body.extend((6u16).to_be_bytes());
            body.extend_from_slice(b"demo/#");
            body.push(1);
            let mut packet = vec![0x82];
            packet.extend(encode_remaining_length(body.len() as u32));
            packet.extend(body);
            packet
        };
        stream.write_all(&subscribe).await.unwrap();
        let mut suback = [0u8; 5];
        stream.read_exact(&mut suback).await.unwrap();
        assert_eq!(suback[4], 1, "granted QoS 1");

        // Another connection publishes to demo/any/level; the wildcard
        // subscription must receive it.
        let mut publisher = TcpStream::connect(broker.addr()).await.unwrap();
        publisher.write_all(&connect).await.unwrap();
        let mut ack = [0u8; 4];
        publisher.read_exact(&mut ack).await.unwrap();
        publisher
            .write_all(&encode_publish("demo/any/level", b"wild", 0, 0))
            .await
            .unwrap();

        // The received log proves what the broker saw; the socket proves the
        // wildcard delivery to the subscriber (forwarded at the publisher's
        // QoS — 0 here, so no packet id sits between topic and payload).
        assert!(
            broker
                .wait_message("demo/any/level", b"wild", Duration::from_secs(5))
                .await,
            "the broker must record the publish"
        );
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0] >> 4, 3);
        assert_eq!(header[0] & 0b1100, 0, "forwarded at the publisher's QoS 0");
        let length = header[1] as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[2..16], b"demo/any/level");
        assert_eq!(&body[16..], b"wild");
    }
}
