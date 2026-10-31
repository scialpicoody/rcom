//! The log store (docs/design.md, "Architecture": `logstore.rs` — "ring buffer +
//! timestamped file sink (auto-save)"; parity row: "auto-save logs with time").
//!
//! One task subscribes to the bus and does two things off the UI thread:
//!
//! * keeps the **ring buffer** of raw records (`{timestamp, direction, bytes}` —
//!   contract §1.3: the raw bytes are authoritative; the view text is a render) for
//!   snapshots and later re-renders (e.g. encoding switches);
//! * auto-saves every record to a **timestamped session file**
//!   (`<data>/rcom/logs/<YYYYmmdd-HHMMSS>[_PORT].log`), created lazily on the first
//!   record of the run. The file stores the truth: one line per record with the
//!   raw bytes losslessly hex-encoded, so the log round-trips (the lossy text
//!   render stays a view concern and never reaches the file).

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Local};
use tokio::sync::Mutex;

use crate::core::bus::{Direction, Event, EventBus, LogLevel, PortState, ScriptOutKind};

/// How many raw records the ring buffer keeps (llcom-class scrollback; the file
/// sink is the unlimited durable copy).
pub const RING_CAP: usize = 10_000;

/// One raw record: what moved, in which direction, when (contract §1.3 — the store
/// keeps the raw bytes, never the render).
#[derive(Debug, Clone)]
pub struct RawRecord {
    pub timestamp: DateTime<Local>,
    pub direction: Direction,
    pub bytes: Vec<u8>,
}

/// The auto-save sink lifecycle: not needed yet, open, or given up on (a broken log
/// directory disables the file after the first failed attempt — the store never
/// retries per record, the ring keeps working).
enum SinkState {
    Pending,
    Open(Sink),
    Disabled,
}

struct Sink {
    file: std::fs::File,
    path: PathBuf,
}

/// The log store: ring buffer + file sink. Shared as `Arc`; the bus task is its
/// only writer, `snapshot`/`file_path` read from anywhere.
pub struct LogStore {
    dir: PathBuf,
    ring: Mutex<VecDeque<RawRecord>>,
    sink: Mutex<SinkState>,
    /// session_id → port name, learned from `Open` events (names the session file).
    ports: Mutex<HashMap<u64, String>>,
}

impl LogStore {
    /// Starts the log store task: subscribes to `bus` and records until the bus
    /// closes. `dir` is created on demand; when it cannot be created the store
    /// degrades to ring-buffer-only (the app never blocks on a bad log path).
    pub fn start(dir: PathBuf, bus: &EventBus) -> Arc<Self> {
        let store = Arc::new(Self {
            dir,
            ring: Mutex::new(VecDeque::with_capacity(1024)),
            sink: Mutex::new(SinkState::Pending),
            ports: Mutex::new(HashMap::new()),
        });
        let task = Arc::clone(&store);
        let mut events = bus.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                task.ingest(&event);
            }
        });
        store
    }

    /// The most recent `limit` raw records, oldest first (the ring-buffer read seam
    /// that future re-renders — encoding switches, filters — will use).
    pub async fn snapshot(&self, limit: usize) -> Vec<RawRecord> {
        let ring = self.ring.lock().await;
        let skip = ring.len().saturating_sub(limit);
        ring.iter().skip(skip).cloned().collect()
    }

    /// The auto-save file of this run, once the first record created it.
    pub async fn file_path(&self) -> Option<PathBuf> {
        match &*self.sink.lock().await {
            SinkState::Open(sink) => Some(sink.path.clone()),
            _ => None,
        }
    }

    /// Synchronous path the bus task uses. Also the direct entry point for tests.
    pub fn ingest(&self, event: &Event) {
        match event {
            Event::Data(data) => {
                let tag = match data.direction {
                    Direction::Rx => "RX",
                    Direction::Tx => "TX",
                };
                self.push(RawRecord {
                    timestamp: data.timestamp,
                    direction: data.direction,
                    bytes: data.bytes.as_slice().to_vec(),
                });
                self.write_line(format!(
                    "{}  {tag}  {}",
                    data.timestamp.format("%Y-%m-%d %H:%M:%S%.3f"),
                    crate::core::codec::hex_spaced(data.bytes.as_slice()),
                ));
            }
            Event::Port(port) => {
                if port.state == PortState::Open {
                    if let Ok(mut ports) = self.ports.try_lock() {
                        ports.insert(port.session_id, port.port.clone());
                    }
                }
                let note = match port.state {
                    PortState::Open => {
                        format!("==  {} opened at {} baud", port.port, port.baud.unwrap_or(0))
                    }
                    PortState::Closed => format!("==  {} closed", port.port),
                    PortState::Error => format!(
                        "!!  {} error: {}",
                        port.port,
                        port.detail.as_deref().unwrap_or("unknown")
                    ),
                    PortState::WatchDesired => format!(
                        "==  {} — {}",
                        port.port,
                        port.detail.as_deref().unwrap_or("watching")
                    ),
                    PortState::Reopened => format!(
                        "==  {} — {} ({} baud)",
                        port.port,
                        port.detail.as_deref().unwrap_or("reopened"),
                        port.baud.unwrap_or(0)
                    ),
                    PortState::Arrived => format!("--  {} arrived", port.port),
                    PortState::Removed => format!("--  {} removed", port.port),
                };
                self.write_line(format!(
                    "{}  {note}",
                    port.timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
                ));
            }
            Event::Log(log) => {
                let level = match log.level {
                    LogLevel::Info => "##",
                    LogLevel::Warn | LogLevel::Error => "!!",
                };
                self.write_line(format!(
                    "{}  {level}  {}",
                    log.timestamp.format("%Y-%m-%d %H:%M:%S%.3f"),
                    log.message
                ));
            }
            Event::ScriptOut(out) => {
                // Script output joins the session file with the UI's `~~` marker
                // (prints verbatim, errors flagged) — it is part of the session's
                // record, though it carries no wire bytes for the ring buffer.
                let line = match &out.kind {
                    ScriptOutKind::Print { line } => line.clone(),
                    ScriptOutKind::Error { message } => format!("!! script error: {message}"),
                };
                self.write_line(format!(
                    "{}  ~~  {line}",
                    out.timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
                ));
            }
            Event::Net(net) => {
                // Net tools traffic joins the session file with the tools' own
                // marker, so the durable record covers the tools page's work
                // (issue #20). Data chunks carry no serial-direction bytes, so
                // the ring buffer stays serial-only.
                let line = match &net.kind {
                    crate::core::nettools::NetEventKind::Listening { local } => {
                        format!("==  [net#{}] listening on {local}", net.channel_id)
                    }
                    crate::core::nettools::NetEventKind::Connected { remote } => {
                        format!("==  [net#{}] connected to {remote}", net.channel_id)
                    }
                    crate::core::nettools::NetEventKind::Disconnected { .. } => {
                        format!("==  [net#{}] closed", net.channel_id)
                    }
                    crate::core::nettools::NetEventKind::Error { message } => {
                        format!("!!  [net#{}] error: {message}", net.channel_id)
                    }
                    crate::core::nettools::NetEventKind::Data { direction, remote, bytes } => {
                        let tag = match direction {
                            crate::core::nettools::NetDirection::Rx => "RX",
                            crate::core::nettools::NetDirection::Tx => "TX",
                        };
                        format!("{tag}  [net#{}] {remote}  {}", net.channel_id,
                            crate::core::codec::hex_spaced(bytes.as_slice()))
                    }
                };
                self.write_line(format!(
                    "{}  ~~  {line}",
                    net.timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
                ));
            }
            Event::Mqtt(mqtt) => {
                // MQTT session traffic joins the session file with the tool's
                // own marker (issue #21). Publish payloads are lossy text; the
                // ring buffer stays serial-only.
                let line = match &mqtt.kind {
                    crate::core::mqtt::MqttEventKind::Connected { server } => {
                        format!("==  [mqtt#{}] connected to {server}", mqtt.session_id)
                    }
                    crate::core::mqtt::MqttEventKind::Disconnected { .. } => {
                        format!("==  [mqtt#{}] closed", mqtt.session_id)
                    }
                    crate::core::mqtt::MqttEventKind::Error { message } => {
                        format!("!!  [mqtt#{}] error: {message}", mqtt.session_id)
                    }
                    crate::core::mqtt::MqttEventKind::Subscribed { topic, qos } => {
                        format!("==  [mqtt#{}] subscribed {topic} (qos {})", mqtt.session_id, qos.wire())
                    }
                    crate::core::mqtt::MqttEventKind::Unsubscribed { topic } => {
                        format!("==  [mqtt#{}] unsubscribed {topic}", mqtt.session_id)
                    }
                    crate::core::mqtt::MqttEventKind::Message { direction, topic, payload, .. } => {
                        let tag = match direction {
                            crate::core::mqtt::MqttDirection::Rx => "RX",
                            crate::core::mqtt::MqttDirection::Tx => "TX",
                        };
                        format!(
                            "{tag}  [mqtt#{}] {topic}  {:?}",
                            mqtt.session_id,
                            String::from_utf8_lossy(payload.as_slice())
                        )
                    }
                };
                self.write_line(format!(
                    "{}  ~~  {line}",
                    mqtt.timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
                ));
            }
            Event::Plot(plot) => {
                // Plot decoder lifecycle joins the durable record with the
                // tools' `~~` marker (issue #24); per-point data never reaches
                // the bus, so there is nothing else to log here.
                use crate::core::bus::PlotEventKind;
                let line = match &plot.kind {
                    PlotEventKind::DecoderStarted { name } => {
                        format!("==  [plot] decoder started: {name}")
                    }
                    PlotEventKind::DecoderStopped { name } => {
                        format!("==  [plot] decoder stopped: {name}")
                    }
                    PlotEventKind::DecoderFailed { name, message } => {
                        format!("!!  [plot] decoder failed: {name}: {message}")
                    }
                };
                self.write_line(format!(
                    "{}  ~~  {line}",
                    plot.timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
                ));
            }
        }
    }

    fn push(&self, record: RawRecord) {
        if let Ok(mut ring) = self.ring.try_lock() {
            if ring.len() >= RING_CAP {
                ring.pop_front(); // rotate: drop the oldest
            }
            ring.push_back(record);
        }
    }

    /// Writes one line to the auto-save file, creating it on the first record of
    /// the run. Every failure disables the file sink (warn once), never panics.
    fn write_line(&self, line: String) {
        let mut sink = match self.sink.try_lock() {
            Ok(sink) => sink,
            Err(_) => return, // contended: skip, the next record retries
        };
        if let SinkState::Pending = *sink {
            *sink = match self.create_sink() {
                Ok(open) => SinkState::Open(open),
                Err(error) => {
                    tracing::warn!("log auto-save disabled (cannot create file): {error}");
                    SinkState::Disabled
                }
            };
        }
        if let SinkState::Open(open) = &mut *sink {
            let _ = writeln!(open.file, "{line}");
            let _ = open.file.flush();
        }
    }

    fn create_sink(&self) -> std::io::Result<Sink> {
        std::fs::create_dir_all(&self.dir)?;
        let port = self
            .ports
            .try_lock()
            .ok()
            .and_then(|ports| {
                // Single-session runs name the file after their port; multi-session
                // runs stay on the plain timestamp (per-session files come with the
                // session tabs ticket).
                let mut names = ports.values();
                let only = names.next().filter(|_| names.next().is_none());
                only.cloned()
            })
            .map(|name| format!("_{}", sanitize_port_name(&name)))
            .unwrap_or_default();
        let stamp = Local::now().format("%Y%m%d-%H%M%S");
        let path = self.dir.join(format!("{stamp}{port}.log"));
        let file = std::fs::File::create(&path)?;
        tracing::info!(path = %path.display(), "session log auto-save");
        Ok(Sink { file, path })
    }
}

/// The port name as a file-name component: everything outside letters, digits and
/// `-_.` becomes `_` (`/dev/ttyUSB0` → `_dev_ttyUSB0`).
fn sanitize_port_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The default auto-save directory: `<data>/rcom/logs` (design.md "Stack": `dirs`
/// for platform data dirs), falling back to the temp dir.
pub fn default_log_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rcom")
        .join("logs")
}

/// A log directory under `base` (tests and callers that already resolved a root).
pub fn log_dir_under(base: &Path) -> PathBuf {
    base.join("rcom").join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::{DataEvent, Payload, PortEvent};

    fn data(direction: Direction, bytes: &[u8]) -> Event {
        Event::Data(DataEvent {
            session_id: 1,
            direction,
            timestamp: Local::now(),
            bytes: Payload::from_vec(bytes.to_vec()),
        })
    }

    fn open_event() -> Event {
        Event::Port(PortEvent {
            session_id: 1,
            port: "COM3".into(),
            state: PortState::Open,
            detail: None,
            timestamp: Local::now(),
            baud: Some(115_200),
        })
    }

    /// A store whose file sink cannot be created (parent is a regular file): the
    /// ring must keep working with the sink disabled after the first failed attempt.
    fn ring_only_store(tag: &str) -> LogStore {
        let blocker = std::env::temp_dir().join(format!("rcom-logstore-{tag}-blocker"));
        std::fs::write(&blocker, b"").ok();
        LogStore {
            dir: blocker.join("logs"), // create_dir_all fails: parent is a file
            ring: Mutex::new(VecDeque::new()),
            sink: Mutex::new(SinkState::Pending),
            ports: Mutex::new(HashMap::new()),
        }
    }

    /// The ring rotates: past `RING_CAP` the oldest records drop and `snapshot`
    /// returns the newest window, oldest first, bytes intact.
    #[tokio::test]
    async fn ring_rotates_and_snapshots_newest() {
        let store = ring_only_store("ring");
        for i in 0..(RING_CAP + 10) {
            store.ingest(&data(Direction::Rx, format!("record {i}").as_bytes()));
        }

        let snap = store.snapshot(3).await;
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].bytes, format!("record {}", RING_CAP + 7).into_bytes());
        assert_eq!(snap[1].bytes, format!("record {}", RING_CAP + 8).into_bytes());
        assert_eq!(snap[2].bytes, format!("record {}", RING_CAP + 9).into_bytes());

        // The full ring holds exactly the cap (rotation, not growth).
        assert_eq!(store.snapshot(usize::MAX).await.len(), RING_CAP);
    }

    /// The auto-save file: created on the first record, named after the run's port,
    /// carrying the raw bytes (hex) plus the open note — the durable copy of truth.
    #[tokio::test]
    async fn file_sink_writes_raw_records_and_notes() {
        let base = std::env::temp_dir().join(format!("rcom-logstore-sink-{}", std::process::id()));
        let dir = log_dir_under(&base);
        let store = LogStore {
            dir: dir.clone(),
            ring: Mutex::new(VecDeque::new()),
            sink: Mutex::new(SinkState::Pending),
            ports: Mutex::new(HashMap::new()),
        };

        store.ingest(&open_event());
        store.ingest(&data(Direction::Tx, &[0x68, 0x69]));
        store.ingest(&data(Direction::Rx, &[0x00, 0xff]));

        let path = store.file_path().await.expect("sink file created");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(|c: char| c.is_ascii_digit()), "{name}");
        assert!(name.contains("COM3"), "{name}");
        assert!(name.ends_with(".log"), "{name}");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("COM3 opened at 115200 baud"), "{content}");
        assert!(content.contains("TX  68 69"), "{content}");
        assert!(content.contains("RX  00 ff"), "{content}");

        std::fs::remove_dir_all(&base).ok();
    }

    /// A hostile port name is sanitized into a safe file-name component (dots stay,
    /// but the sink always prefixes a timestamp, so no traversal is possible).
    #[test]
    fn port_names_are_sanitized() {
        assert_eq!(sanitize_port_name("/dev/ttyUSB0"), "_dev_ttyUSB0");
        assert_eq!(sanitize_port_name("COM3"), "COM3");
        assert_eq!(sanitize_port_name("../../etc"), ".._.._etc");
    }
}
