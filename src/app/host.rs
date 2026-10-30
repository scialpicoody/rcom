//! The bridge between the UI and the core (docs/design.md: "Core must remain
//! UI-agnostic; the app bridges").
//!
//! - The tokio runtime runs on a dedicated background thread; the core's driver task
//!   executes UI commands (refresh/open/close/send) and renders every bus event into
//!   a pre-formatted log row.
//! - Rows cross to the UI thread over a std channel; a [`ferroui_base::threading::DispatcherTimer`]
//!   on the UI thread drains it (the pinned FerroUI object handles are not `Send`, so
//!   the timer poll is the sanctioned hand-off — verified against the 0.4.0-preview.1
//!   sources).
//! - `RCOM_AUTODEMO=1` drives the whole path headlessly: open MOCK0, send `ping`,
//!   watch the echo arrive — the smoke-run proof of RX→bus→log and send→TX.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;

use rcom::core::bus::{DataEvent, Direction, Event, LogEvent, LogLevel, PortEvent, PortState};
use rcom::core::codec::{hex_spaced, lossy_utf8, TextEncoding};
use rcom::core::port::manager::PortManager;
use rcom::core::port::mock::{MockBackend, MOCK_PORT_NAME};
use rcom::core::port::serial::SerialPortBackend;
use rcom::core::port::{OpenPort, PortConfig, PortInfo, SerialBackend};
use rcom::core::{EventBus, Result};

/// A command from the UI thread to the core driver.
#[derive(Debug)]
pub enum Command {
    RefreshPorts,
    Open { name: String, baud: u32 },
    Close,
    /// Text from the send box, already encoded to wire bytes by the UI (the
    /// selector's encoding applies there, so the core stays encoding-agnostic).
    SendBytes(Vec<u8>),
}

/// One data chunk for the UI's log model: the raw bytes stay authoritative so an
/// encoding switch re-renders history (docs/ui-drafts/05-contract.md §1.3).
#[derive(Debug, Clone)]
pub struct DataRowMsg {
    pub direction: Direction,
    /// Pre-formatted local timestamp (`%H:%M:%S%.3f`).
    pub stamp: String,
    pub bytes: Vec<u8>,
}

/// A message from the core driver to the UI thread.
#[derive(Debug)]
pub enum UiMsg {
    /// A fresh port listing.
    Ports(Vec<PortInfo>),
    /// A session's port state (drives the Open/Close button).
    PortState(PortState),
    /// A pre-formatted log row (port state lines, diagnostics).
    Row(String),
    /// A data chunk; the UI renders it with the current encoding.
    Data(DataRowMsg),
    /// Autodemo drives the encoding switch (the same code path as the selector).
    SetEncoding(TextEncoding),
}

/// The process-wide handle the UI thread uses to reach the core. `ui_rx` is drained
/// by the UI's dispatcher-timer pump (std `Receiver::try_recv` takes `&self`).
pub struct CoreHost {
    pub commands: mpsc::UnboundedSender<Command>,
    /// Drained by the UI pump; the Mutex is for `Sync` (a static host), the pump
    /// is its only user.
    pub ui_rx: std::sync::Mutex<std::sync::mpsc::Receiver<UiMsg>>,
}

static HOST: OnceLock<CoreHost> = OnceLock::new();

/// The core host, starting the engine on first use.
pub fn core_host() -> &'static CoreHost {
    HOST.get_or_init(start_engine)
}

fn start_engine() -> CoreHost {
    let bus = EventBus::new();
    let manager = Arc::new(PortManager::new(
        Arc::new(CompositeBackend::new()),
        bus.clone(),
    ));
    let (commands_tx, commands_rx) = mpsc::unbounded_channel::<Command>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiMsg>();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for the core");

    let driver_manager = Arc::clone(&manager);
    let demo_commands = commands_tx.clone();
    std::thread::Builder::new()
        .name("rcom-core".into())
        .spawn(move || {
            runtime.block_on(driver(driver_manager, bus, commands_rx, demo_commands, ui_tx));
        })
        .expect("spawn rcom-core thread");

    tracing::debug!("core engine started");
    CoreHost {
        commands: commands_tx,
        ui_rx: std::sync::Mutex::new(ui_rx),
    }
}

/// The core driver loop: one task multiplexing UI commands and bus events until the
/// UI side goes away.
async fn driver(
    manager: Arc<PortManager>,
    bus: EventBus,
    mut commands: mpsc::UnboundedReceiver<Command>,
    commands_tx: mpsc::UnboundedSender<Command>,
    ui: std::sync::mpsc::Sender<UiMsg>,
) {
    // The demo paths: `RCOM_AUTODEMO=1` opens MOCK0 and sends `ping` (the mock
    // echoes it back as RX); `RCOM_AUTODEMO=encoding` sends 你好 as GBK bytes and
    // then drives the encoding switch — the re-render proof of ticket #17.
    // (`=persist` is handled entirely on the UI side, see the controller.)
    let mut awaiting_echo: Option<Vec<u8>> = None;
    match AutodemoMode::from_env() {
        AutodemoMode::Off | AutodemoMode::Persist => {}
        AutodemoMode::Basic => {
            tracing::info!("RCOM_AUTODEMO=1: driving the MOCK0 demo path");
            let demo = commands_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = demo.send(Command::SendBytes(b"ping".to_vec()));
            });
            awaiting_echo = Some(b"ping".to_vec());
        }
        AutodemoMode::Encoding => {
            tracing::info!("RCOM_AUTODEMO=encoding: driving the GBK encoding demo path");
            let demo = commands_tx.clone();
            let ui = ui.clone();
            let payload = TextEncoding::Gbk.encode("你好，rcom\r\n");
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                tokio::time::sleep(Duration::from_millis(500)).await;
                // TX in GBK while the display still decodes UTF-8: garbled rows.
                let _ = demo.send(Command::SendBytes(payload));
                // Then the switch: the UI re-renders the retained raw bytes.
                tokio::time::sleep(Duration::from_millis(700)).await;
                let _ = ui.send(UiMsg::SetEncoding(TextEncoding::Gbk));
            });
            awaiting_echo = Some(TextEncoding::Gbk.encode("你好，rcom\r\n"));
        }
    }

    let mut events = bus.subscribe();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break, // the host is gone: app shutting down
                Some(command) => handle_command(&manager, &ui, command).await,
            },
            event = events.recv() => match event {
                Ok(event) => {
                    // Autodemo evidence in the trace log: the sent bytes came back
                    // over the mock as RX — send→port→bus proven for this run.
                    if let Event::Data(data) = &event {
                        if let Some(expected) = &awaiting_echo {
                            if data.direction == Direction::Rx
                                && data.bytes.as_slice() == expected.as_slice()
                            {
                                awaiting_echo = None;
                                tracing::info!(
                                    "AUTODEMO OK: payload sent as TX and echoed as RX \
                                     byte-exact (send→port→bus→log path proven)"
                                );
                            }
                        }
                    }
                    if let Event::Port(port) = &event {
                        // The state reaches the button, the row the log view.
                        if ui.send(UiMsg::PortState(port.state)).is_err() {
                            break; // UI gone
                        }
                    }
                    match &event {
                        Event::Data(data) => {
                            let row = format_data_row(data);
                            tracing::debug!(target: "rcom::rows", "{row}");
                            let message = UiMsg::Data(DataRowMsg {
                                direction: data.direction,
                                stamp: data.timestamp.format("%H:%M:%S%.3f").to_string(),
                                bytes: data.bytes.as_slice().to_vec(),
                            });
                            if ui.send(message).is_err() {
                                break; // UI gone
                            }
                        }
                        Event::Port(_) | Event::Log(_) => {
                            if let Some(row) = render_event(&event) {
                                tracing::debug!(target: "rcom::rows", "{row}");
                                if ui.send(UiMsg::Row(row)).is_err() {
                                    break; // UI gone
                                }
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    let _ = ui.send(UiMsg::Row(rust_i18n::t!("bus_lag", missed = missed).to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

async fn handle_command(manager: &PortManager, ui: &std::sync::mpsc::Sender<UiMsg>, command: Command) {
    match command {
        Command::RefreshPorts => match manager.list_ports().await {
            Ok(ports) => {
                let _ = ui.send(UiMsg::Ports(ports));
            }
            Err(error) => {
                let _ = ui.send(UiMsg::Row(rust_i18n::t!("port_list_failed", error = &error).to_string()));
            }
        },
        Command::Open { name, baud } => {
            if let Err(error) = manager.open(&name, PortConfig::new(baud)).await {
                let _ = ui.send(UiMsg::Row(
                    rust_i18n::t!("open_failed", name = &name, error = &error).to_string(),
                ));
            }
        }
        Command::Close => manager.close().await,
        Command::SendBytes(bytes) => {
            if let Err(error) = manager.write(bytes).await {
                let _ = ui.send(UiMsg::Row(rust_i18n::t!("send_failed", error = &error).to_string()));
            }
        }
    }
}

/// Turns a non-data bus event into the row the log view shows (data chunks travel
/// to the UI as raw [`DataRowMsg`]s and are rendered there, per encoding).
pub fn render_event(event: &Event) -> Option<String> {
    match event {
        Event::Data(_) => None,
        Event::Port(port) => Some(format_port_row(port)),
        Event::Log(log) => Some(format_log_row(log)),
    }
}

/// How much of one chunk one row shows before it is abbreviated.
pub const ROW_HEX_BYTES: usize = 96;
pub const ROW_TEXT_CHARS: usize = 128;

pub fn format_data_row(data: &DataEvent) -> String {
    let direction = match data.direction {
        Direction::Rx => "RX",
        Direction::Tx => "TX",
    };
    let bytes = data.bytes.as_slice();
    let hex = if bytes.len() > ROW_HEX_BYTES {
        format!("{} …(+{} bytes)", hex_spaced(&bytes[..ROW_HEX_BYTES]), bytes.len() - ROW_HEX_BYTES)
    } else {
        hex_spaced(bytes)
    };
    let text_full = lossy_utf8(bytes);
    let text = if text_full.chars().count() > ROW_TEXT_CHARS {
        format!("{} …", text_full.chars().take(ROW_TEXT_CHARS).collect::<String>())
    } else {
        text_full
    };
    format!(
        "{}  {:>2}  {}  \"{}\"",
        data.timestamp.format("%H:%M:%S%.3f"),
        direction,
        hex,
        text.replace('"', "'")
    )
}

pub fn format_port_row(port: &PortEvent) -> String {
    let stamp = port.timestamp.format("%H:%M:%S%.3f");
    match port.state {
        PortState::Open => format!("{stamp}  ==  {}", rust_i18n::t!("opened", name = &port.port)),
        PortState::Closed => format!("{stamp}  ==  {}", rust_i18n::t!("closed", name = &port.port)),
        PortState::Error => format!(
            "{stamp}  !!  {}",
            rust_i18n::t!(
                "port_error",
                name = &port.port,
                detail = port.detail.as_deref().unwrap_or("unknown")
            )
        ),
    }
}

pub fn format_log_row(log: &LogEvent) -> String {
    let level = match log.level {
        LogLevel::Info => "##",
        LogLevel::Warn => "!!",
        LogLevel::Error => "!!",
    };
    format!(
        "{}  {}  {}",
        log.timestamp.format("%H:%M:%S%.3f"),
        level,
        log.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rcom::core::bus::Payload;

    fn stamp() -> chrono::DateTime<chrono::Local> {
        use chrono::Timelike;
        chrono::Local
            .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
            .unwrap()
            .with_nanosecond(123_000_000)
            .unwrap()
    }

    /// A data row shows the direction, the hex and the decoded text — the AC's
    /// "HEX + decoded text" per row, direction visible.
    #[test]
    fn data_row_shows_direction_hex_and_text() {
        let row = format_data_row(&DataEvent {
            session_id: 1,
            direction: Direction::Tx,
            timestamp: stamp(),
            bytes: Payload::from_vec("hi".as_bytes().to_vec()),
        });
        assert_eq!(row, "03:04:05.123  TX  68 69  \"hi\"");
    }

    /// Invalid UTF-8 degrades to the replacement character instead of failing the
    /// row, and embedded quotes survive readable.
    #[test]
    fn data_row_is_lossy_and_readable() {
        let row = format_data_row(&DataEvent {
            session_id: 1,
            direction: Direction::Rx,
            timestamp: stamp(),
            bytes: Payload::from_vec(vec![0x68, 0xff, 0x22]),
        });
        assert!(row.contains("68 ff 22"), "{row}");
        assert!(row.contains("h\u{fffd}'"), "{row}");
    }

    /// Oversized chunks abbreviate instead of flooding the log view.
    #[test]
    fn data_row_abbreviates_large_chunks() {
        let row = format_data_row(&DataEvent {
            session_id: 1,
            direction: Direction::Rx,
            timestamp: stamp(),
            bytes: Payload::from_vec(vec![0x61; ROW_HEX_BYTES + 10]),
        });
        assert!(row.contains("…(+10 bytes)"), "{row}");
    }

    /// Port and log events render their state lines.
    #[test]
    fn port_and_log_rows_render() {
        let open = format_port_row(&PortEvent {
            session_id: 1,
            port: "MOCK0".into(),
            state: PortState::Open,
            detail: None,
            timestamp: stamp(),
        });
        assert_eq!(open, "03:04:05.123  ==  MOCK0 opened");

        let error = format_port_row(&PortEvent {
            session_id: 1,
            port: "COM3".into(),
            state: PortState::Error,
            detail: Some("device gone".into()),
            timestamp: stamp(),
        });
        assert_eq!(error, "03:04:05.123  !!  COM3 error: device gone");

        let log = format_log_row(&LogEvent {
            level: LogLevel::Warn,
            timestamp: stamp(),
            message: "careful".into(),
        });
        assert_eq!(log, "03:04:05.123  !!  careful");

        assert!(render_event(&Event::Log(LogEvent {
            level: LogLevel::Info,
            timestamp: stamp(),
            message: "x".into(),
        }))
        .is_some());
    }
}

/// What `RCOM_AUTODEMO` asks for: the basic `ping` path, the GBK encoding path,
/// the quick-send persistence path, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutodemoMode {
    Off,
    Basic,
    Encoding,
    Persist,
}

impl AutodemoMode {
    /// Reads `RCOM_AUTODEMO`: `1` = basic, `encoding` = the GBK path,
    /// `persist` = the quick-send persistence path, else off.
    pub fn from_env() -> Self {
        match std::env::var("RCOM_AUTODEMO").as_deref() {
            Ok("1") => Self::Basic,
            Ok("encoding") => Self::Encoding,
            Ok("persist") => Self::Persist,
            _ => Self::Off,
        }
    }
}

/// Both backends, behind one list: the real ports of the machine plus the mock's
/// `MOCK0` (the demo seam). Opening `MOCK0` routes to the mock; anything else to
/// the real driver.
struct CompositeBackend {
    real: SerialPortBackend,
    mock: MockBackend,
}

impl CompositeBackend {
    fn new() -> Self {
        Self {
            real: SerialPortBackend::new(),
            mock: MockBackend::new(),
        }
    }
}

impl SerialBackend for CompositeBackend {
    fn enumerate(&self) -> Result<Vec<PortInfo>> {
        let mut ports = match self.real.enumerate() {
            Ok(ports) => ports,
            Err(error) => {
                tracing::warn!("real port enumeration failed: {error}");
                Vec::new()
            }
        };
        ports.extend(self.mock.enumerate()?);
        Ok(ports)
    }

    fn open(&self, name: &str, config: &PortConfig) -> Result<OpenPort> {
        if name == MOCK_PORT_NAME {
            self.mock.open(name, config)
        } else {
            self.real.open(name, config)
        }
    }
}
