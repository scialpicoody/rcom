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
//! - The engine's always-on services start beside the driver: the log store (ring
//!   buffer + auto-save file sink), the hotplug poller (1 s) and the reconnect state
//!   machine (docs/design.md, "Serial layer").
//! - `RCOM_AUTODEMO=1` drives the basic path headlessly: open MOCK0, send `ping`,
//!   watch the echo arrive. `RCOM_AUTODEMO=reconnect` additionally injects an
//!   unplug/replug cycle on the mock: session → WatchDesired → auto-reopen → data
//!   flows again — the hardware-free proof of the reconnect machine (contract §5d).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;

use rcom::core::bus::{
    DataEvent, Direction, Event, LogEvent, LogLevel, PortEvent, PortState,
};
use rcom::core::codec::{hex_spaced, lossy_utf8};
use rcom::core::logstore::{default_log_dir, LogStore};
use rcom::core::port::manager::PortManager;
use rcom::core::port::mock::{MockBackend, MockPlug, MOCK_PORT_NAME};
use rcom::core::port::reconnect;
use rcom::core::port::serial::SerialPortBackend;
use rcom::core::port::{hotplug, OpenPort, PortConfig, PortInfo, SerialBackend};
use rcom::core::{EventBus, Result};

/// A command from the UI thread to the core driver.
#[derive(Debug)]
pub enum Command {
    RefreshPorts,
    Open { name: String, baud: u32 },
    Close,
    SendText(String),
}

/// A message from the core driver to the UI thread.
#[derive(Debug)]
pub enum UiMsg {
    /// A fresh port listing.
    Ports(Vec<PortInfo>),
    /// A session's port state (drives the Open/Close button).
    PortState(PortState),
    /// A pre-formatted log row (timestamp, direction/origin, hex, text). `direction`
    /// tags data rows (`None` for status/diagnostic lines) — the hide-sent filter
    /// works on this, view-side only.
    Row {
        text: String,
        direction: Option<Direction>,
    },
}

/// A rendered bus event: the row text plus the data direction it shows (if any).
pub struct RenderedRow {
    pub text: String,
    pub direction: Option<Direction>,
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
    let composite = CompositeBackend::new();
    let mock_plug = composite.mock_plug();
    let backend: Arc<dyn SerialBackend> = Arc::new(composite);
    let manager = Arc::new(PortManager::new(Arc::clone(&backend), bus.clone()));
    let (commands_tx, commands_rx) = mpsc::unbounded_channel::<Command>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiMsg>();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for the core");

    let driver_bus = bus;
    let demo_commands = commands_tx.clone();
    std::thread::Builder::new()
        .name("rcom-core".into())
        .spawn(move || {
            // Everything tokio-task-spawning runs inside the runtime context: the
            // engine's always-on services (log store, hotplug poller, reconnect
            // machine) start beside the driver loop, on the same runtime.
            runtime.block_on(async move {
                let logstore = LogStore::start(default_log_dir(), &driver_bus);
                hotplug::spawn(Arc::clone(&backend), driver_bus.clone());
                reconnect::spawn(Arc::clone(&manager), driver_bus.clone());
                driver(
                    manager,
                    driver_bus,
                    commands_rx,
                    demo_commands,
                    ui_tx,
                    logstore,
                    mock_plug,
                )
                .await;
            });
        })
        .expect("spawn rcom-core thread");

    tracing::debug!("core engine started");
    CoreHost {
        commands: commands_tx,
        ui_rx: std::sync::Mutex::new(ui_rx),
    }
}

/// The autodemo scenario selected by `RCOM_AUTODEMO` (`1` = basic echo demo,
/// `reconnect` = unplug/replug demo; unset or anything else = no demo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoDemo {
    Basic,
    Reconnect,
}

fn autodemo_mode() -> Option<AutoDemo> {
    match std::env::var("RCOM_AUTODEMO").ok().as_deref() {
        Some("1") => Some(AutoDemo::Basic),
        Some("reconnect") => Some(AutoDemo::Reconnect),
        _ => None,
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
    logstore: Arc<LogStore>,
    mock_plug: MockPlug,
) {
    let mode = autodemo_mode();
    // The basic demo path: open MOCK0 and send `ping`; the mock echoes it back as RX.
    let mut awaiting_echo = false;
    // The reconnect demo: set once the machine reports Reopened, so the next RX
    // `ping` proves data flows across the unplug/replug.
    let mut reconnected = false;

    if let Some(mode) = mode {
        let demo = commands_tx.clone();
        let plug = mock_plug.clone();
        let store = Arc::clone(&logstore);
        tokio::spawn(async move {
            match mode {
                AutoDemo::Basic => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = demo.send(Command::SendText("ping".into()));
                }
                AutoDemo::Reconnect => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let _ = demo.send(Command::SendText("hello".into()));
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    tracing::info!("AUTODEMO: injecting MOCK0 unplug");
                    plug.unplug();
                    tokio::time::sleep(Duration::from_millis(2000)).await;
                    tracing::info!("AUTODEMO: injecting MOCK0 replug");
                    plug.replug();
                    // The hotplug poller announces the return within its 1 s
                    // cadence; the machine re-opens; give it both windows.
                    tokio::time::sleep(Duration::from_millis(1400)).await;
                    let _ = demo.send(Command::SendText("ping".into()));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    match store.file_path().await {
                        Some(path) => {
                            tracing::info!("AUTODEMO: session log file: {}", path.display())
                        }
                        None => tracing::warn!("AUTODEMO: no session log file was created"),
                    }
                }
            }
        });
    }

    let mut events = bus.subscribe();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break, // the host is gone: app shutting down
                Some(command) => {
                    if mode == Some(AutoDemo::Basic) {
                        if let Command::SendText(text) = &command {
                            if text == "ping" {
                                awaiting_echo = true;
                            }
                        }
                    }
                    handle_command(&manager, &ui, command).await;
                }
            },
            event = events.recv() => match event {
                Ok(event) => {
                    // Autodemo evidence in the trace log. Basic: the sent `ping`
                    // came back over the mock as RX — send→TX and RX→bus→log proven
                    // in one run. Reconnect: the same after an injected unplug and
                    // the machine's auto-reopen.
                    if let Event::Data(data) = &event {
                        let is_ping = data.direction == Direction::Rx
                            && data.bytes.as_slice() == b"ping";
                        if awaiting_echo && is_ping {
                            awaiting_echo = false;
                            tracing::info!(
                                "AUTODEMO OK: 'ping' sent as TX and echoed as RX \
                                 (send→port→bus→log path proven)"
                            );
                        }
                        if reconnected && is_ping {
                            reconnected = false;
                            tracing::info!(
                                "AUTODEMO OK: auto-reconnect complete — data flows again \
                                 after unplug/replug (WatchDesired→Open proven)"
                            );
                        }
                    }
                    if let Event::Port(port) = &event {
                        match port.state {
                            PortState::WatchDesired => tracing::info!(
                                "AUTODEMO: unplug detected → WatchDesired (waiting for {})",
                                port.port
                            ),
                            PortState::Reopened => {
                                reconnected = true;
                                tracing::info!(
                                    "AUTODEMO: port returned → auto-reopened {}",
                                    port.port
                                );
                            }
                            _ => {}
                        }
                        // The state reaches the button, the row the log view.
                        if ui.send(UiMsg::PortState(port.state)).is_err() {
                            break; // UI gone
                        }
                    }
                    if let Some(row) = render_event(&event) {
                        tracing::debug!(target: "rcom::rows", "{}", row.text);
                        if ui
                            .send(UiMsg::Row { text: row.text, direction: row.direction })
                            .is_err()
                        {
                            break; // UI gone
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    let _ = ui.send(UiMsg::Row {
                        text: format!("!! {missed} events missed (bus lag)"),
                        direction: None,
                    });
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
                let _ = ui.send(UiMsg::Row {
                    text: format!("!! port list failed: {error}"),
                    direction: None,
                });
            }
        },
        Command::Open { name, baud } => {
            if let Err(error) = manager.open(&name, PortConfig::new(baud)).await {
                let _ = ui.send(UiMsg::Row {
                    text: format!("!! open {name} failed: {error}"),
                    direction: None,
                });
            }
        }
        Command::Close => manager.close().await,
        Command::SendText(text) => {
            if let Err(error) = manager.write(text.into_bytes()).await {
                let _ = ui.send(UiMsg::Row {
                    text: format!("!! send failed: {error}"),
                    direction: None,
                });
            }
        }
    }
}

/// Turns a bus event into the row the log view shows: timestamp + direction/origin
/// + hex + decoded text for data; state and diagnostics lines for the rest.
pub fn render_event(event: &Event) -> Option<RenderedRow> {
    match event {
        Event::Data(data) => Some(RenderedRow {
            text: format_data_row(data),
            direction: Some(data.direction),
        }),
        Event::Port(port) => Some(RenderedRow {
            text: format_port_row(port),
            direction: None,
        }),
        Event::Log(log) => Some(RenderedRow {
            text: format_log_row(log),
            direction: None,
        }),
    }
}

/// How much of one chunk one row shows before it is abbreviated.
const ROW_HEX_BYTES: usize = 96;
const ROW_TEXT_CHARS: usize = 128;

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
        PortState::Open => format!("{stamp}  ==  {} opened", port.port),
        PortState::Closed => format!("{stamp}  ==  {} closed", port.port),
        PortState::Error => format!(
            "{stamp}  !!  {} error: {}",
            port.port,
            port.detail.as_deref().unwrap_or("unknown")
        ),
        PortState::WatchDesired => format!(
            "{stamp}  ==  {} — {}",
            port.port,
            port.detail.as_deref().unwrap_or("waiting for the port to reappear")
        ),
        PortState::Reopened => format!(
            "{stamp}  ==  {} — {} ({} baud)",
            port.port,
            port.detail.as_deref().unwrap_or("reconnected"),
            port.baud.unwrap_or(0)
        ),
        PortState::Arrived => format!("{stamp}  --  {} arrived", port.port),
        PortState::Removed => format!("{stamp}  --  {} removed", port.port),
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
    use rcom::core::bus::{Payload, GLOBAL_SESSION_ID};

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

    /// Port and log events render their state lines — including the reconnect
    /// machine's WatchDesired/Reopened and the hotplug poller's global events.
    #[test]
    fn port_and_log_rows_render() {
        let open = format_port_row(&PortEvent {
            session_id: 1,
            port: "MOCK0".into(),
            state: PortState::Open,
            detail: None,
            timestamp: stamp(),
            baud: Some(115_200),
        });
        assert_eq!(open, "03:04:05.123  ==  MOCK0 opened");

        let error = format_port_row(&PortEvent {
            session_id: 1,
            port: "COM3".into(),
            state: PortState::Error,
            detail: Some("device gone".into()),
            timestamp: stamp(),
            baud: None,
        });
        assert_eq!(error, "03:04:05.123  !!  COM3 error: device gone");

        let watching = format_port_row(&PortEvent {
            session_id: 1,
            port: "COM3".into(),
            state: PortState::WatchDesired,
            detail: Some("waiting for the port to reappear (auto-reconnect)".into()),
            timestamp: stamp(),
            baud: None,
        });
        assert_eq!(
            watching,
            "03:04:05.123  ==  COM3 — waiting for the port to reappear (auto-reconnect)"
        );

        let reopened = format_port_row(&PortEvent {
            session_id: 2,
            port: "COM3".into(),
            state: PortState::Reopened,
            detail: Some("auto-reconnected".into()),
            timestamp: stamp(),
            baud: Some(115_200),
        });
        assert_eq!(
            reopened,
            "03:04:05.123  ==  COM3 — auto-reconnected (115200 baud)"
        );

        let arrived = format_port_row(&PortEvent {
            session_id: GLOBAL_SESSION_ID,
            port: "COM3".into(),
            state: PortState::Arrived,
            detail: None,
            timestamp: stamp(),
            baud: None,
        });
        assert_eq!(arrived, "03:04:05.123  --  COM3 arrived");

        let removed = format_port_row(&PortEvent {
            session_id: GLOBAL_SESSION_ID,
            port: "COM3".into(),
            state: PortState::Removed,
            detail: None,
            timestamp: stamp(),
            baud: None,
        });
        assert_eq!(removed, "03:04:05.123  --  COM3 removed");

        let log = format_log_row(&LogEvent {
            level: LogLevel::Warn,
            timestamp: stamp(),
            message: "careful".into(),
        });
        assert_eq!(log, "03:04:05.123  !!  careful");
    }

    /// The rendered row tags data events with their direction (the hide-sent
    /// filter's input) and status lines with none.
    #[test]
    fn rendered_rows_carry_direction_only_for_data() {
        let data = render_event(&Event::Data(DataEvent {
            session_id: 1,
            direction: Direction::Tx,
            timestamp: stamp(),
            bytes: Payload::from_vec(b"hi".to_vec()),
        }))
        .unwrap();
        assert_eq!(data.direction, Some(Direction::Tx));

        let port = render_event(&Event::Port(PortEvent {
            session_id: 1,
            port: "COM3".into(),
            state: PortState::Closed,
            detail: None,
            timestamp: stamp(),
            baud: None,
        }))
        .unwrap();
        assert_eq!(port.direction, None);
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

    /// The unplug/replug control of the mock backend (the autodemo drives it).
    fn mock_plug(&self) -> MockPlug {
        self.mock.plug()
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
