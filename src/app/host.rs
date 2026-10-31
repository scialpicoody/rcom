//! The bridge between the UI and the core (docs/design.md: "Core must remain
//! UI-agnostic; the app bridges").
//!
//! - The tokio runtime runs on a dedicated background thread; the core's driver task
//!   executes UI commands (refresh/open/close/send/mock-playback) and renders every
//!   bus event into a pre-formatted log row tagged with its session (the session
//!   tabs route by it — design-ui §3.6: filter by `session_id` everywhere).
//! - Rows cross to the UI thread over a std channel; a [`ferroui_base::threading::DispatcherTimer`]
//!   on the UI thread drains it (the pinned FerroUI object handles are not `Send`, so
//!   the timer poll is the sanctioned hand-off — verified against the 0.4.0-preview.1
//!   sources).
//! - The engine's always-on services start beside the driver: the log store (ring
//!   buffer + auto-save file sink), the hotplug poller (1 s) and the reconnect state
//!   machine (docs/design.md, "Serial layer").
//! - **The mock seam (design-ui §5.7):** the engine's backend is the core's
//!   [`BackendFactory`]; this file only *selects the mode* — `RCOM_MOCK` when set,
//!   else the legacy demo vars pin their port (`RCOM_AUTODEMO=reconnect` ⇒ the
//!   unplug scenario, the other mock-driven demos ⇒ the echo scenario), else the
//!   core's profile default. Zero `RCOM_MOCK` parsing below beyond that one
//!   selection; the UI never references it (AC9).
//! - `RCOM_AUTODEMO` drives a scenario headlessly: `1` opens MOCK0 and sends `ping`
//!   (the echo is the smoke-run evidence); `reconnect` drives the unplug scenario's
//!   MockControl trigger through the new seam; `encoding` sends 你好 as GBK and
//!   drives the encoding switch; `persist` runs entirely on the UI side (see the
//!   controller). Beside these, `RCOM_CODECDEMO=1` runs the codec toolbox's pure
//!   conversion/recovery proofs (issue #22; see `core::codec::demo`).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;

use rcom::core::bus::{
    DataEvent, Direction, Event, LogEvent, LogLevel, PlotEvent, PlotEventKind, PortEvent, PortState,
    ScriptOutEvent, ScriptOutKind,
};
use rcom::core::codec::{hex_spaced, lossy_utf8, TextEncoding};
use rcom::core::logstore::{default_log_dir, LogStore};
use rcom::core::lua::{LuaRuntime, UartSink};
use rcom::core::plot::{demo, DecoderHost};
use rcom::core::port::factory::{mock_mode_default_for_profile, mock_mode_from_env, BackendFactory, MockMode};
use rcom::core::port::manager::PortManager;
use rcom::core::port::mock::{MockScenario, MockTrigger, MOCK_PORT_NAME};
use rcom::core::port::reconnect;
use rcom::core::port::{MockControl, PortInfo};
use rcom::core::EventBus;

/// A command from the UI thread to the core driver.
#[derive(Debug)]
pub enum Command {
    RefreshPorts,
    /// Opens `name` as a new session (the multi-session manager adds it; the
    /// tab that sent the command adopts the session id from the Open event).
    Open { name: String, baud: u32 },
    /// Closes the session (the tab's close / the Open-Close button).
    CloseSession { session_id: u64 },
    /// Closes every session (the quit guard's "Close all").
    CloseAll,
    /// Text from the send bar / quick-send, already encoded to wire bytes by
    /// the UI (the selector's encoding applies there, so the core stays
    /// encoding-agnostic). `session_id: None` = the legacy current-session
    /// path (the demo harness).
    SendBytes { session_id: Option<u64>, bytes: Vec<u8> },
    /// The script panel's Run: (re)boots the standalone script VM with this code
    /// (ticket #19). Output flows back as `~~` log rows.
    RunScript { code: String },
    /// The script panel's Stop: stops the standalone VM (instruction-count hook
    /// unwinds even a hard loop).
    StopScript,
    /// Installs or clears the pre-send hook (`None` = disabled). With a
    /// session id: that session's hook (the Script page's path); without: the
    /// manager-wide slot (the demo harness).
    SetHook { session_id: Option<u64>, script: Option<String> },
    /// The plot page's decoder Start (issue #24): boots a decoder VM with this
    /// script on the core runtime (a previous one is stopped first). Output
    /// flows back as [`UiMsg::Plot`] lifecycle events.
    PlotDecoderStart { name: String, code: String },
    /// The plot page's decoder Stop.
    PlotDecoderStop,
    /// A mock-drawer action (design-ui §6.1) against session `id`'s playback
    /// controls. Resolved core-side: a no-op when the session is not a mock.
    MockAction { session_id: u64, action: MockAction },
}

/// The mock drawer's vocabulary (design-ui §6.1 `MockControl`), app-side so
/// the UI types no core mock internals beyond the handle's API.
#[derive(Debug, Clone)]
pub enum MockAction {
    Pause(bool),
    Speed(f64),
    Step,
    InjectRx(Vec<u8>),
    Trigger(MockTrigger),
    Restart,
}

impl MockAction {
    /// Applies itself to a resolved [`MockControl`].
    fn apply(self, control: &MockControl) {
        match self {
            MockAction::Pause(on) => control.pause(on),
            MockAction::Speed(speed) => control.set_speed(speed),
            MockAction::Step => control.step(),
            MockAction::InjectRx(bytes) => control.inject_rx(bytes),
            MockAction::Trigger(trigger) => control.trigger(trigger),
            MockAction::Restart => control.restart(),
        }
    }
}

/// One data chunk for the UI's log model: the raw bytes stay authoritative so an
/// encoding switch re-renders history (docs/ui-drafts/05-contract.md §1.3).
/// `session_id` routes the row to its tab; `hooked` renders the ` ⌁hook`
/// marker on post-hook TX rows (design-ui §3.3/§5.2).
#[derive(Debug, Clone)]
pub struct DataRowMsg {
    pub session_id: u64,
    pub direction: Direction,
    /// Pre-formatted local timestamp (`%H:%M:%S%.3f`).
    pub stamp: String,
    pub bytes: Vec<u8>,
    pub hooked: bool,
}

/// A message from the core driver to the UI thread.
#[derive(Debug)]
pub enum UiMsg {
    /// A fresh port listing.
    Ports(Vec<PortInfo>),
    /// A session's port lifecycle (drives the tab's chip/banner; `row` is the
    /// formatted note row for the session's log).
    PortState { session_id: u64, port: String, state: PortState, detail: Option<String>, row: String },
    /// A pre-formatted log row (port notes, diagnostics, tool echoes).
    /// `session_id: Some` routes to that session's tab, `None` to the active
    /// tab (process-global rows: tool stories, standalone script VM output).
    /// `direction` tags the data a row shows (`None` for status/diagnostic
    /// lines) — the hide-sent filter works on this, view-side only; data
    /// chunks travel as [`UiMsg::Data`].
    Row { session_id: Option<u64>, text: String, direction: Option<Direction> },
    /// A data chunk; the UI renders it with the session's current encoding.
    Data(DataRowMsg),
    /// Autodemo drives the encoding switch (the same code path as the selector).
    SetEncoding(TextEncoding),
    /// A net-tools event (issue #20); the tools page consumes it and the row
    /// text also lands in the active tab's log view.
    Net(rcom::core::nettools::NetEvent),
    /// An MQTT-tool event (issue #21); the tools page consumes it and the row
    /// text also lands in the active tab's log view.
    Mqtt(rcom::core::mqtt::MqttEvent),
    /// A plot-pipeline event (issue #24); the plot page consumes it and the
    /// row also lands in the active tab's log view.
    Plot(PlotEvent),
    /// A serial-monitor event (issue #23); the monitor page consumes it and
    /// the row also lands in the active tab's log view.
    Monitor(rcom::core::monitor::MonitorEvent),
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

/// The backend-factory mode the engine starts with: `RCOM_MOCK` when set; else
/// the legacy demo vars pin their port (the demo evidence paths must work in
/// every build profile); else the core's profile default. The selection is
/// the *only* mock-env reading outside the core's factory (AC9: none of it in
/// the UI layer — this is the driver's startup, the seam's selection site).
fn selected_mock_mode() -> MockMode {
    if let Some(mode) = mock_mode_from_env() {
        return mode;
    }
    match AutodemoMode::from_env() {
        AutodemoMode::Reconnect => MockMode::Single(MockScenario::Unplug),
        AutodemoMode::Off => {
            if luademo_enabled() || plotdemo_enabled() {
                MockMode::Single(MockScenario::Echo)
            } else {
                mock_mode_default_for_profile()
            }
        }
        _ => MockMode::Single(MockScenario::Echo),
    }
}

fn start_engine() -> CoreHost {
    let bus = EventBus::new();
    let factory = BackendFactory::with_mode(selected_mock_mode());
    let backend: Arc<dyn rcom::core::port::SerialBackend> = Arc::new(factory);
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
                // The log directory: the settings override when persisted
                // (ticket #25 — the settings load runs before the engine
                // starts), else the platform default.
                let log_dir = super::settings::global()
                    .map(|settings| settings.resolved_log_dir())
                    .unwrap_or_else(default_log_dir);
                let logstore = LogStore::start(log_dir, &driver_bus);
                rcom::core::port::hotplug::spawn(Arc::clone(&backend), driver_bus.clone());
                reconnect::spawn(Arc::clone(&manager), driver_bus.clone());
                // ISSUE #20: the process-global net hub (channels + bridge) and,
                // when requested, the net autodemo beside the driver.
                let net_hub = rcom::core::nettools::hub::init_global(
                    driver_bus.clone(),
                    Arc::clone(&manager),
                );
                if rcom::core::nettools::demo::NetDemoMode::from_env() != rcom::core::nettools::demo::NetDemoMode::Off
                {
                    tokio::spawn(rcom::core::nettools::demo::run(
                        driver_bus.clone(),
                        net_hub,
                        Some(Arc::clone(&manager)),
                    ));
                }
                // ISSUE #21: the process-global MQTT hub (sessions + bridge)
                // and, when requested, the MQTT autodemo (in-process mock
                // broker — no external broker needed).
                let mqtt_hub = rcom::core::mqtt::hub::init_global(
                    driver_bus.clone(),
                    Arc::clone(&manager),
                );
                if rcom::core::mqtt::demo::MqttDemoMode::from_env() != rcom::core::mqtt::demo::MqttDemoMode::Off
                {
                    tokio::spawn(rcom::core::mqtt::demo::run(
                        driver_bus.clone(),
                        mqtt_hub,
                        Some(Arc::clone(&manager)),
                    ));
                }
                // ISSUE #22: the codec autodemo — pure conversion/recovery
                // proofs over known mojibake (no port, no network); its
                // evidence rows ride the bus's log events like the others.
                rcom::core::codec::demo::run(&driver_bus);
                // ISSUE #23: the process-global monitor hub (the pty-pair proxy)
                // and, when requested, the monitor autodemo (loopback, plain
                // File I/O on the pty slaves).
                let monitor_hub = rcom::core::monitor::hub::init_global(
                    driver_bus.clone(),
                    Arc::clone(&backend),
                );
                if rcom::core::monitor::demo::MonitorDemoMode::from_env()
                    != rcom::core::monitor::demo::MonitorDemoMode::Off
                {
                    tokio::spawn(rcom::core::monitor::demo::run(
                        driver_bus.clone(),
                        monitor_hub,
                    ));
                }
                driver(
                    manager,
                    driver_bus,
                    commands_rx,
                    demo_commands,
                    ui_tx,
                    logstore,
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

/// What `RCOM_AUTODEMO` asks for: the basic `ping` path, the unplug/replug
/// reconnect path, the GBK encoding path, the quick-send persistence path, the
/// pre-send-hook path, the script-panel Run/Stop path, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutodemoMode {
    Off,
    Basic,
    Encoding,
    Persist,
    Reconnect,
    Hook,
    Panel,
}

impl AutodemoMode {
    /// Reads `RCOM_AUTODEMO`: `1` = basic, `reconnect` = the unplug/replug path,
    /// `encoding` = the GBK path, `persist` = the quick-send persistence path,
    /// `hook` = the pre-send-hook path, `panel` = the script-panel Run/Stop path,
    /// else off.
    pub fn from_env() -> Self {
        match std::env::var("RCOM_AUTODEMO").as_deref() {
            Ok("1") => Self::Basic,
            Ok("reconnect") => Self::Reconnect,
            Ok("encoding") => Self::Encoding,
            Ok("persist") => Self::Persist,
            Ok("hook") => Self::Hook,
            Ok("panel") => Self::Panel,
            _ => Self::Off,
        }
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
) {
    // The demo paths: `RCOM_AUTODEMO=1` opens MOCK0 and sends `ping` (the mock
    // echoes it back as RX); `RCOM_AUTODEMO=reconnect` opens the unplug-scenario
    // MOCK0 and drives its MockControl trigger — session → WatchDesired →
    // auto-reopen → data flows again (contract §5d; the new seam's path).
    // `RCOM_AUTODEMO=encoding` sends 你好 as GBK bytes and then drives the
    // encoding switch — the re-render proof of ticket #17. (`=persist` is
    // handled entirely on the UI side, see the controller.) `RCOM_AUTODEMO=hook`
    // installs the `fromHex` pre-send hook and sends the hex *text* `414243` —
    // the wire must see `41 42 43`; `=panel` runs the script-panel Run/Stop
    // path headlessly. `RCOM_LUADEMO=1` implies the basic path — the script VM
    // boots on the MOCK0 open and feeds on its first RX chunk (ticket #18).
    let mut awaiting_echo: Option<Vec<u8>> = None;
    // The reconnect demo: set once the machine reports Reopened, so the next RX
    // `ping` proves data flows across the unplug/replug.
    let mut reconnected = false;
    let luademo = luademo_enabled();
    let plotdemo = plotdemo_enabled();
    // The reconnect demo's trigger task learns its session id from the Open
    // event (published by the driver's Port arm below).
    let (demo_id_tx, demo_id_rx) = mpsc::unbounded_channel::<u64>();
    let demo_id_tx = if AutodemoMode::from_env() == AutodemoMode::Reconnect {
        Some(demo_id_tx)
    } else {
        None
    };
    let requested = AutodemoMode::from_env();
    let mode = if requested == AutodemoMode::Off && (luademo || plotdemo) {
        AutodemoMode::Basic // both demos run against the MOCK0 open
    } else {
        requested
    };
    if luademo {
        tracing::info!("RCOM_LUADEMO=1: driving the Lua demo (llcom sample on MOCK0)");
    }
    if plotdemo {
        tracing::info!(
            "RCOM_PLOTDEMO: driving the plot demo (bundled framed-protocol decoder, \
             generator on MOCK0 unless the mock already feeds frames; mode {:?})",
            demo::PlotDemoMode::from_env()
        );
    }
    match mode {
        AutodemoMode::Off | AutodemoMode::Persist => {}
        AutodemoMode::Basic => {
            if requested == AutodemoMode::Basic {
                tracing::info!("RCOM_AUTODEMO=1: driving the MOCK0 demo path");
            }
            let demo = commands_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = demo.send(Command::SendBytes { session_id: None, bytes: b"ping".to_vec() });
            });
            awaiting_echo = Some(b"ping".to_vec());
        }
        AutodemoMode::Reconnect => {
            tracing::info!(
                "RCOM_AUTODEMO=reconnect: driving the unplug/replug demo path \
                 (unplug scenario MOCK0, MockControl triggers)"
            );
            let demo = commands_tx.clone();
            let store = Arc::clone(&logstore);
            let mut id_rx = demo_id_rx;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                let session_id = match id_rx.recv().await {
                    Some(session_id) => session_id,
                    None => return,
                };
                tokio::time::sleep(Duration::from_millis(600)).await;
                let _ = demo.send(Command::SendBytes {
                    session_id: Some(session_id),
                    bytes: b"hello".to_vec(),
                });
                tokio::time::sleep(Duration::from_millis(600)).await;
                tracing::info!("AUTODEMO: triggering MOCK0 unplug (MockControl)");
                let _ = demo.send(Command::MockAction {
                    session_id,
                    action: MockAction::Trigger(MockTrigger::Unplug),
                });
                // The scripted reappear (5 s / speed) opens the reconnect
                // window; give the machine its poll cadence, then ping.
                tokio::time::sleep(Duration::from_millis(6500)).await;
                let _ = demo.send(Command::SendBytes {
                    session_id: Some(session_id),
                    bytes: b"ping".to_vec(),
                });
                tokio::time::sleep(Duration::from_millis(300)).await;
                match store.file_path().await {
                    Some(path) => {
                        tracing::info!("AUTODEMO: session log file: {}", path.display())
                    }
                    None => tracing::warn!("AUTODEMO: no session log file was created"),
                }
            });
        }
        AutodemoMode::Hook => {
            tracing::info!(
                "RCOM_AUTODEMO=hook: driving the pre-send-hook demo path \
                 ('414243' must reach the wire as 41 42 43)"
            );
            let demo = commands_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = demo.send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 });
                tokio::time::sleep(Duration::from_millis(500)).await;
                // The llcom README hook, through the real UI command path.
                let _ = demo.send(Command::SetHook {
                    session_id: None,
                    script: Some(rcom::core::lua::scripts::SEND_CONVERT_HEX.into()),
                });
                tokio::time::sleep(Duration::from_millis(200)).await;
                // Hex *text* in — the hook must convert before the writer.
                let _ = demo.send(Command::SendBytes { session_id: None, bytes: b"414243".to_vec() });
            });
            // The echo comes back as the *converted* bytes "ABC".
            awaiting_echo = Some(b"ABC".to_vec());
        }
        AutodemoMode::Panel => {
            tracing::info!(
                "RCOM_AUTODEMO=panel: driving the script-panel demo path \
                 (Run boots the VM; Stop unwinds a hard loop)"
            );
            let demo = commands_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                // The editor content, through the real Run command: prints once,
                // then spins forever at the top level — the Stop button's
                // instruction-count hook is the only way out.
                let _ = demo.send(Command::RunScript {
                    code: "print(\"panel demo running\")\nwhile true do end\n".into(),
                });
                // Give the print time to surface, then cut the loop down.
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let _ = demo.send(Command::StopScript);
            });
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
                let _ = demo.send(Command::SendBytes { session_id: None, bytes: payload });
                // Then the switch: the UI re-renders the retained raw bytes.
                tokio::time::sleep(Duration::from_millis(700)).await;
                let _ = ui.send(UiMsg::SetEncoding(TextEncoding::Gbk));
            });
            awaiting_echo = Some(TextEncoding::Gbk.encode("你好，rcom\r\n"));
        }
    }

    // RCOM_LUADEMO: one script VM on the MOCK0 session, running llcom's README
    // sample (uartReceive → publish → task replies "ok!", plus a 1s loop timer).
    let mut script_vm: Option<LuaRuntime> = None;
    let mut script_vm_fed = false;
    // The script panel's standalone VM (ticket #19): Run replaces it, Stop stops
    // it. Its output flows to the log view as `~~` rows; its uart writes go to
    // the open session raw (the pre-send hook does not apply to script sends,
    // llcom parity). VMs under construction report back through `panel_ready`;
    // the generation counter rejects superseded runs.
    let mut panel_vm: Option<LuaRuntime> = None;
    let mut panel_generation: u64 = 0;
    let (panel_ready_tx, mut panel_ready_rx) =
        tokio::sync::mpsc::unbounded_channel::<PanelVmReady>();
    // The panel demo's two proofs: the Run print surfaced, the Stop unwound.
    let mut panel_demo_ran = false;
    let mut panel_stop_ok = false;
    // The plot decoder (issue #24): Start/Stop from the plot page (and the
    // PLOTDEMO's bundled decoder) boot it like the script-panel VM — on a
    // blocking task, adopted through a generation counter. The feed itself
    // happens in the event arm below: every RX bus chunk goes into `decode`
    // (the bus tap), and the decoder writes the plot store directly.
    let mut plot_decoder: Option<DecoderHost> = None;
    let mut plot_generation: u64 = 0;
    let (plot_ready_tx, mut plot_ready_rx) =
        tokio::sync::mpsc::unbounded_channel::<PlotDecoderReady>();
    let mut plotdemo_started = false;

    let mut events = bus.subscribe();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break, // the host is gone: app shutting down
                Some(command) => {
                    handle_command(
                        &manager,
                        &ui,
                        &bus,
                        &mut panel_vm,
                        &mut panel_generation,
                        panel_ready_tx.clone(),
                        &mut plot_decoder,
                        &mut plot_generation,
                        plot_ready_tx.clone(),
                        command,
                    )
                    .await
                }
            },
            ready = panel_ready_rx.recv() => match ready {
                None => break, // the sender is gone: app shutting down
                Some(arriving) => {
                    if arriving.generation == panel_generation {
                        match arriving.vm {
                            Ok(vm) if !vm.is_stopped() => {
                                tracing::info!(
                                    generation = arriving.generation,
                                    "script panel: VM started"
                                );
                                panel_vm = Some(vm);
                            }
                            Ok(vm) => {
                                // A Stop outran this VM's start: keep it stopped.
                                vm.stop();
                            }
                            Err(error) => {
                                let _ = ui.send(UiMsg::Row {
                                    session_id: None,
                                    text: rust_i18n::t!("script_run_failed", error = &error).to_string(),
                                    direction: None,
                                });
                            }
                        }
                    } else if let Ok(vm) = arriving.vm {
                        // Superseded while loading: stop it before it runs.
                        vm.stop();
                    }
                }
            },
            ready = plot_ready_rx.recv() => match ready {
                None => break, // the sender is gone: app shutting down
                Some(arriving) => {
                    if arriving.generation == plot_generation {
                        match arriving.result {
                            Ok(decoder) => {
                                tracing::info!(name = decoder.name(), "plot decoder started");
                                bus.publish(Event::Plot(PlotEvent {
                                    timestamp: chrono::Local::now(),
                                    kind: PlotEventKind::DecoderStarted {
                                        name: decoder.name().to_string(),
                                    },
                                }));
                                plot_decoder = Some(decoder);
                            }
                            Err(error) => {
                                bus.publish(Event::Plot(PlotEvent {
                                    timestamp: chrono::Local::now(),
                                    kind: PlotEventKind::DecoderFailed {
                                        name: arriving.name.clone(),
                                        message: error.to_string(),
                                    },
                                }));
                            }
                        }
                    } else if let Ok(decoder) = arriving.result {
                        // Superseded by a newer Start/Stop while loading.
                        decoder.stop();
                    }
                }
            },
            event = events.recv() => match event {
                Ok(event) => {
                    // Script-panel demo evidence: the Run command booted the
                    // standalone VM (its print surfaced), and the Stop command
                    // unwound the spinning script — the only writer of the stop
                    // flag is `Command::StopScript`, so a "script stopped" error
                    // is proof of the whole Stop path.
                    if mode == AutodemoMode::Panel {
                        if let Event::ScriptOut(out) = &event {
                            match &out.kind {
                                ScriptOutKind::Print { line }
                                    if !panel_demo_ran && line.contains("panel demo running") =>
                                {
                                    panel_demo_ran = true;
                                    tracing::info!(
                                        "AUTODEMO PANEL RUN OK: the standalone VM adopted the \
                                         Run command and executed the script (print surfaced as \
                                         a `~~` row)"
                                    );
                                }
                                ScriptOutKind::Error { message }
                                    if message.contains("script stopped") && !panel_stop_ok =>
                                {
                                    panel_stop_ok = true;
                                    tracing::info!(
                                        "AUTODEMO PANEL STOP OK: the spinning script unwound \
                                         through the instruction-count hook ({message})"
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                    // Autodemo evidence in the trace log: the sent bytes came back
                    // over the mock as RX — send→port→bus proven for this run.
                    if let Event::Data(data) = &event {
                        // Hook demo, TX side: the hex *text* `414243` hit the
                        // wire as the converted bytes 41 42 43 — the Lua
                        // pre-send hook transformed the TX before the writer.
                        if mode == AutodemoMode::Hook
                            && data.direction == Direction::Tx
                            && data.bytes.as_slice() == b"ABC"
                        {
                            tracing::info!(
                                "AUTODEMO HOOK OK: TX '414243' reached the wire as 41 42 43 \
                                 ('ABC') — the fromHex pre-send hook transformed the send"
                            );
                        }
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
                        // Reconnect evidence: the post-replug `ping` came back, so
                        // the new session moves bytes (WatchDesired→Open proven).
                        if reconnected
                            && data.direction == Direction::Rx
                            && data.bytes.as_slice() == b"ping"
                        {
                            reconnected = false;
                            tracing::info!(
                                "AUTODEMO OK: auto-reconnect complete — data flows again \
                                 after unplug/replug (WatchDesired→Open proven)"
                            );
                        }
                    }
                    // Lua demo: boot the sample VM as soon as MOCK0 is open; feed
                    // it exactly one RX chunk (the `ping` echo) — its own `ok!`
                    // reply comes back as RX too, and feeding that would loop.
                    if luademo {
                        if script_vm.is_none() {
                            if let Event::Port(port) = &event {
                                if port.state == PortState::Open && port.port == MOCK_PORT_NAME {
                                    script_vm = start_demo_vm(&manager, bus.clone(), port.session_id, &ui);

                                }
                            }
                        }
                        if !script_vm_fed {
                            if let (Some(vm), Event::Data(data)) = (&script_vm, &event) {
                                if data.direction == Direction::Rx {
                                    script_vm_fed = true;
                                    let vm = vm.clone();
                                    let bytes = data.bytes.as_slice().to_vec();
                                    tokio::spawn(async move {
                                        vm.feed_uart(&bytes);
                                    });
                                }
                            }
                        }
                    }
                    // Plot demo (issue #24): on the MOCK0 open, boot the bundled
                    // framed-protocol decoder. The frame *feed* comes from the
                    // mock itself when the opened port is the framed scenario
                    // (RCOM_MOCK=framed via the new seam); otherwise the demo
                    // generator writes frames through the real TX path as before.
                    if plotdemo && !plotdemo_started {
                        if let Event::Port(port) = &event {
                            if port.state == PortState::Open && port.port == MOCK_PORT_NAME {
                                plotdemo_started = true;
                                plot_generation += 1;
                                spawn_plot_decoder(
                                    bus.clone(),
                                    "bundled: framed protocol".into(),
                                    rcom::core::lua::scripts::FRAMED_DECODER_SAMPLE.into(),
                                    plot_generation,
                                    plot_ready_tx.clone(),
                                );
                                let framed_mock = manager
                                    .mock_controls(port.session_id)
                                    .is_some_and(|control| control.scenario() == MockScenario::Framed);
                                if !framed_mock {
                                    let config = match demo::PlotDemoMode::from_env() {
                                        demo::PlotDemoMode::Fast => demo::DemoConfig::fast(),
                                        _ => demo::DemoConfig::basic(),
                                    };
                                    demo::spawn_generator(Arc::clone(&manager), config);
                                } else {
                                    tracing::info!(
                                        "PLOTDEMO: the framed mock feeds the decoder directly \
                                         (no demo generator)"
                                    );
                                }
                            }
                        }
                    }
                    // The plot decoder's bus tap: every RX chunk feeds
                    // `decode(bytes, ts)`; a script error fails the decoder once
                    // (one Plot event, the tap unarms), never per chunk.
                    if let Event::Data(data) = &event {
                        if data.direction == Direction::Rx && plot_decoder.is_some() {
                            let mut failure: Option<String> = None;
                            if let Some(decoder) = &plot_decoder {
                                if let Err(error) = decoder.feed(
                                    data.bytes.as_slice(),
                                    data.timestamp.timestamp_millis() as f64,
                                ) {
                                    failure = Some(error.to_string());
                                }
                            }
                            if let Some(message) = failure {
                                let name = plot_decoder
                                    .as_ref()
                                    .map(|decoder| decoder.name().to_string())
                                    .unwrap_or_default();
                                if let Some(decoder) = plot_decoder.take() {
                                    decoder.stop();
                                }
                                bus.publish(Event::Plot(PlotEvent {
                                    timestamp: chrono::Local::now(),
                                    kind: PlotEventKind::DecoderFailed { name, message },
                                }));
                            }
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
                        // The reconnect demo's trigger task needs the session id.
                        if let Some(demo_id_tx) = &demo_id_tx {
                            if port.state == PortState::Open {
                                let _ = demo_id_tx.send(port.session_id);
                            }
                        }
                        // The state reaches the tab's chip and its note row.
                        let row = format_port_row(port);
                        if ui
                            .send(UiMsg::PortState {
                                session_id: port.session_id,
                                port: port.port.clone(),
                                state: port.state,
                                detail: port.detail.clone(),
                                row,
                            })
                            .is_err()
                        {
                            break; // UI gone
                        }
                    }
                    match &event {
                        Event::Data(data) => {
                            let row = format_data_row(data);
                            tracing::debug!(target: "rcom::rows", "{row}");
                            let message = UiMsg::Data(DataRowMsg {
                                session_id: data.session_id,
                                direction: data.direction,
                                stamp: data.timestamp.format("%H:%M:%S%.3f").to_string(),
                                bytes: data.bytes.as_slice().to_vec(),
                                hooked: data.hooked,
                            });
                            if ui.send(message).is_err() {
                                break; // UI gone
                            }
                        }
                        // ISSUE #20: net events render one row into the main log
                        // view (the session-log echo of the tools' traffic) and
                        // go to the net tools page as their own message.
                        Event::Net(net) => {
                            let row = super::tools_net::format_net_row(net);
                            tracing::debug!(target: "rcom::rows", "{row}");
                            if ui.send(UiMsg::Row { session_id: None, text: row, direction: None }).is_err() {
                                break; // UI gone
                            }
                            if ui.send(UiMsg::Net(net.clone())).is_err() {
                                break; // UI gone
                            }
                        }
                        // ISSUE #21: MQTT events, same treatment as the net
                        // tools — main-log echo row plus the page's own message.
                        Event::Mqtt(mqtt) => {
                            let row = super::tools_mqtt::format_mqtt_row(mqtt);
                            tracing::debug!(target: "rcom::rows", "{row}");
                            if ui.send(UiMsg::Row { session_id: None, text: row, direction: None }).is_err() {
                                break; // UI gone
                            }
                            if ui.send(UiMsg::Mqtt(mqtt.clone())).is_err() {
                                break; // UI gone
                            }
                        }
                        // ISSUE #24: plot events render one row (decoder
                        // lifecycle) and go to the plot page as their own message.
                        Event::Plot(plot) => {
                            let row = format_plot_row(plot);
                            tracing::debug!(target: "rcom::rows", "{row}");
                            if ui.send(UiMsg::Row { session_id: None, text: row, direction: None }).is_err() {
                                break; // UI gone
                            }
                            if ui.send(UiMsg::Plot(plot.clone())).is_err() {
                                break; // UI gone
                            }
                        }
                        // ISSUE #23: monitor events render one row (relay
                        // lifecycle + captured traffic) and go to the monitor
                        // page as their own message.
                        Event::Monitor(monitor) => {
                            let row = super::tools_monitor::format_monitor_row(monitor);
                            tracing::debug!(target: "rcom::rows", "{row}");
                            if ui.send(UiMsg::Row { session_id: None, text: row, direction: None }).is_err() {
                                break; // UI gone
                            }
                            if ui.send(UiMsg::Monitor(monitor.clone())).is_err() {
                                break; // UI gone
                            }
                        }
                        Event::Port(_) => {}
                        Event::Log(_) | Event::ScriptOut(_) => {
                            if let Some((session_id, row)) = render_event(&event) {
                                tracing::debug!(target: "rcom::rows", "{row}");
                                if ui
                                    .send(UiMsg::Row { session_id, text: row, direction: None })
                                    .is_err()
                                {
                                    break; // UI gone
                                }
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    let _ = ui.send(UiMsg::Row {
                        session_id: None,
                        text: rust_i18n::t!("bus_lag", missed = missed).to_string(),
                        direction: None,
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    // The VMs are dropped with the driver; their timers abort and the tokio
    // runtime shuts down with the process.
    if let Some(vm) = script_vm {
        vm.stop();
    }
    if let Some(vm) = panel_vm {
        vm.stop();
    }
    if let Some(decoder) = plot_decoder {
        decoder.stop();
    }
}

/// The sink for script VMs' `apiSendUartData`: raw writes to the open session.
/// Raw on purpose (llcom parity): the pre-send hook transforms *UI* sends; a
/// script's uart writes are already final and must not be converted twice.
struct ScriptSink(Arc<PortManager>);

impl UartSink for ScriptSink {
    fn send_uart(&self, bytes: Vec<u8>) -> bool {
        let manager = Arc::clone(&self.0);
        tokio::spawn(async move {
            if let Err(error) = manager.write_raw(bytes).await {
                tracing::warn!("script uart send failed: {error}");
            }
        });
        true
    }
}

/// The script VM backing the `RCOM_LUADEMO` run: llcom's README sample, with
/// `apiSend("uart")` wired to the port manager's write path. Failures become log
/// rows (never panics).
fn start_demo_vm(
    manager: &Arc<PortManager>,
    bus: EventBus,
    session_id: u64,
    ui: &std::sync::mpsc::Sender<UiMsg>,
) -> Option<LuaRuntime> {
    match LuaRuntime::start(
        bus,
        Some(session_id),
        Some(Arc::new(ScriptSink(Arc::clone(manager)))),
        rcom::core::lua::scripts::LLCOM_README_SAMPLE,
    ) {
        Ok(vm) => Some(vm),
        Err(error) => {
            let _ = ui.send(UiMsg::Row {
                session_id: None,
                text: format!("!! {}", rust_i18n::t!("lua_demo_failed", error = &error)),
                direction: None,
            });
            None
        }
    }
}

/// Stops the script panel's VM, if one is running. Never blocks: the stop flag
/// plus the instruction-count hook unwind even a hard loop, and the dying VM's
/// thread finishes on its own (the handle was cloned into it).
fn stop_panel_vm(panel_vm: &Option<LuaRuntime>) {
    if let Some(vm) = panel_vm {
        tracing::info!("script panel: stopping the running VM");
        vm.stop();
    }
}

/// A freshly created script-panel VM on its way to the driver: the driver adopts
/// it (into `panel_vm`) when the generation is still current, else stops it —
/// a newer Run/Stop superseded this one.
struct PanelVmReady {
    generation: u64,
    vm: mlua::Result<LuaRuntime>,
}

/// Boots the script panel's standalone VM (the Run button): [`LuaRuntime::create`]
/// is bounded setup (sandbox + host API + head.lua, no user code); the handle is
/// reported to the driver *before* the script chunk runs, so the Stop button holds
/// the stop switch even while a script spins at load. Run errors surface as `~~`
/// rows through the bus ([`LuaRuntime::run_script`] reports them); a create
/// failure rides back on the message. Never blocks the driver.
fn spawn_panel_vm(
    manager: &Arc<PortManager>,
    bus: EventBus,
    code: String,
    generation: u64,
    ready: tokio::sync::mpsc::UnboundedSender<PanelVmReady>,
) {
    let sink = Arc::new(ScriptSink(Arc::clone(manager))) as Arc<dyn UartSink>;
    tokio::task::spawn_blocking(move || {
        let vm = LuaRuntime::create(bus, None, Some(sink));
        let Ok(reportable) = vm else {
            // `create` failing is an engine bug (sandbox/head.lua are embedded);
            // surface it and give up on this run.
            let _ = ready.send(PanelVmReady {
                generation,
                vm: Err(mlua::Error::runtime("script VM bootstrap failed")),
            });
            return;
        };
        let _ = ready.send(PanelVmReady {
            generation,
            vm: Ok(reportable.clone()),
        });
        // Run errors are published on the bus by `run_script` itself (`~~` rows);
        // the handle on the driver side keeps the stop switch armed meanwhile.
        let _ = reportable.run_script(&code);
    });
}

/// A freshly started plot decoder on its way to the driver (the panel VM's
/// readiness pattern): adopted when the generation is still current, stopped
/// when a newer Start/Stop superseded it.
struct PlotDecoderReady {
    generation: u64,
    name: String,
    result: mlua::Result<DecoderHost>,
}

/// Boots a plot decoder VM (issue #24) on a blocking task — `run_script` may
/// spin at load, and the driver must never block. The readiness message
/// carries the stop switch either way.
fn spawn_plot_decoder(
    bus: EventBus,
    name: String,
    code: String,
    generation: u64,
    ready: tokio::sync::mpsc::UnboundedSender<PlotDecoderReady>,
) {
    tokio::task::spawn_blocking(move || {
        let result = DecoderHost::start(name.clone(), bus, &code);
        let _ = ready.send(PlotDecoderReady {
            generation,
            name,
            result,
        });
    });
}

async fn handle_command(
    manager: &Arc<PortManager>,
    ui: &std::sync::mpsc::Sender<UiMsg>,
    bus: &EventBus,
    panel_vm: &mut Option<LuaRuntime>,
    panel_generation: &mut u64,
    panel_ready: tokio::sync::mpsc::UnboundedSender<PanelVmReady>,
    plot_decoder: &mut Option<DecoderHost>,
    plot_generation: &mut u64,
    plot_ready: tokio::sync::mpsc::UnboundedSender<PlotDecoderReady>,
    command: Command,
) {
    match command {
        Command::RefreshPorts => match manager.list_ports().await {
            Ok(ports) => {
                let _ = ui.send(UiMsg::Ports(ports));
            }
            Err(error) => {
                let _ = ui.send(UiMsg::Row {
                    session_id: None,
                    text: rust_i18n::t!("port_list_failed", error = &error).to_string(),
                    direction: None,
                });
            }
        },
        Command::Open { name, baud } => {
            if let Err(error) = manager.open(&name, rcom::core::port::PortConfig::new(baud)).await {
                let _ = ui.send(UiMsg::Row {
                    session_id: None,
                    text: rust_i18n::t!("open_failed", name = &name, error = &error).to_string(),
                    direction: None,
                });
            }
        }
        Command::CloseSession { session_id } => manager.close_session(session_id).await,
        Command::CloseAll => manager.close_all().await,
        Command::SendBytes { session_id, bytes } => {
            let outcome = match session_id {
                Some(session_id) => manager.write_for(session_id, bytes).await,
                None => manager.write(bytes).await,
            };
            if let Err(error) = outcome {
                let _ = ui.send(UiMsg::Row {
                    session_id,
                    text: rust_i18n::t!("send_failed", error = &error).to_string(),
                    direction: None,
                });
            }
        }
        Command::RunScript { code } => {
            // A fresh VM per run: stop the old one first, so `taskInit`ed tasks
            // and loop timers never survive a re-run.
            stop_panel_vm(panel_vm);
            *panel_vm = None;
            *panel_generation += 1;
            spawn_panel_vm(manager, bus.clone(), code, *panel_generation, panel_ready);
        }
        Command::StopScript => {
            stop_panel_vm(panel_vm);
            *panel_vm = None;
            *panel_generation += 1; // an in-flight start arrives to be stopped
            let _ = ui.send(UiMsg::Row {
                session_id: None,
                text: rust_i18n::t!("script_stopped").to_string(),
                direction: None,
            });
        }
        Command::SetHook { session_id, script } => {
            let hook = script.clone().map(rcom::core::lua::TxHook::new);
            match session_id {
                Some(session_id) => manager.set_hook_for(session_id, hook),
                None => manager.set_hook(hook),
            }
            let text = match script {
                Some(code) => rust_i18n::t!("tx_hook_on", script = &code).to_string(),
                None => rust_i18n::t!("tx_hook_off").to_string(),
            };
            let _ = ui.send(UiMsg::Row { session_id, text, direction: None });
        }
        Command::PlotDecoderStart { name, code } => {
            // A fresh VM per start: stop the old one first (a Stopped notice
            // only when there was one).
            if let Some(old) = plot_decoder.take() {
                let name = old.name().to_string();
                old.stop();
                bus.publish(Event::Plot(PlotEvent {
                    timestamp: chrono::Local::now(),
                    kind: PlotEventKind::DecoderStopped { name },
                }));
            }
            *plot_generation += 1;
            spawn_plot_decoder(bus.clone(), name, code, *plot_generation, plot_ready);
        }
        Command::PlotDecoderStop => {
            if let Some(old) = plot_decoder.take() {
                let name = old.name().to_string();
                old.stop();
                bus.publish(Event::Plot(PlotEvent {
                    timestamp: chrono::Local::now(),
                    kind: PlotEventKind::DecoderStopped { name },
                }));
            }
        }
        Command::MockAction { session_id, action } => match manager.mock_controls(session_id) {
            Some(control) => action.apply(&control),
            None => tracing::warn!(session = session_id, "mock action on a non-mock session"),
        },
    }
}

/// Turns a non-data bus event into the `(session_id, row)` the log view shows
/// (`None`-session rows land on the active tab; data chunks travel to the UI as
/// raw [`DataRowMsg`]s and are rendered there, per encoding; net and MQTT events
/// are rendered in the driver's event arm — they carry their own row shape and
/// also go to the tools pages).
pub fn render_event(event: &Event) -> Option<(Option<u64>, String)> {
    match event {
        Event::Data(_) => None,
        Event::Port(_) => None, // rendered in the driver's Port arm (chip + row)
        Event::Log(log) => Some((log.session_id, format_log_row(log))),
        Event::ScriptOut(out) => Some((out.session_id, format_script_row(out))),
        Event::Net(_) => None,
        Event::Mqtt(_) => None,
        Event::Plot(_) => None,
        Event::Monitor(_) => None,
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
            rust_i18n::t!("port_error", name = &port.port, detail = localize_port_detail(port.detail.as_deref()))
        ),
        PortState::WatchDesired => format!(
            "{stamp}  ==  {}",
            rust_i18n::t!("port_watching", name = &port.port, detail = localize_port_detail(port.detail.as_deref()))
        ),
        PortState::Reopened => format!(
            "{stamp}  ==  {}",
            rust_i18n::t!(
                "port_reopened",
                name = &port.port,
                detail = localize_port_detail(port.detail.as_deref()),
                baud = port.baud.unwrap_or(0)
            )
        ),
        PortState::Arrived => format!("{stamp}  --  {}", rust_i18n::t!("port_arrived", name = &port.port)),
        PortState::Removed => format!("{stamp}  --  {}", rust_i18n::t!("port_removed", name = &port.port)),
    }
}

/// The sweep (ticket #25): the core publishes English detail strings as data
/// (the reconnect machine's two known phrases); the render boundary translates
/// the known ones and passes everything else through (OS errors etc. read as
/// they arrived). `None` degrades to the localized "unknown".
fn localize_port_detail(detail: Option<&str>) -> String {
    match detail {
        None => rust_i18n::t!("detail_unknown").to_string(),
        Some(known) if known == reconnect::DETAIL_WATCHING => {
            rust_i18n::t!("port_detail_watching").to_string()
        }
        Some(known) if known == reconnect::DETAIL_RECONNECTED => {
            rust_i18n::t!("port_detail_reconnected").to_string()
        }
        Some(other) => other.to_string(),
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

/// Script rows are marked `~~` so they read apart from wire traffic: `print` /
/// `log.*` verbatim (llcom's own "[I]-[tag] message" shape), errors prefixed.
pub fn format_script_row(out: &ScriptOutEvent) -> String {
    let stamp = out.timestamp.format("%H:%M:%S%.3f");
    match &out.kind {
        ScriptOutKind::Print { line } => format!("{stamp}  ~~  {line}"),
        // The sweep (ticket #25): the error prefix translates; the `~~`/`!!`
        // row-kind markers are log syntax, not prose.
        ScriptOutKind::Error { message } => {
            format!("{stamp}  ~~  !! {}", rust_i18n::t!("script_error", message = message))
        }
    }
}

/// Plot rows (issue #24) read like the net rows: a `[plot]` tag, lifecycle
/// notes as `==`, failures flagged.
pub fn format_plot_row(plot: &PlotEvent) -> String {
    let stamp = plot.timestamp.format("%H:%M:%S%.3f");
    match &plot.kind {
        PlotEventKind::DecoderStarted { name } => {
            format!("{stamp}  ==  [plot] {}", rust_i18n::t!("plot_row_started", name = name))
        }
        PlotEventKind::DecoderStopped { name } => {
            format!("{stamp}  ==  [plot] {}", rust_i18n::t!("plot_row_stopped", name = name))
        }
        PlotEventKind::DecoderFailed { name, message } => format!(
            "{stamp}  ~~  !! [plot] {}",
            rust_i18n::t!("plot_row_failed", name = name, reason = message)
        ),
    }
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
            hooked: false,
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
            hooked: false,
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
            hooked: false,
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
            session_id: None,
        });
        assert_eq!(log, "03:04:05.123  !!  careful");

        assert!(render_event(&Event::Log(LogEvent {
            level: LogLevel::Info,
            timestamp: stamp(),
            message: "x".into(),
            session_id: Some(3),
        }))
        .is_some_and(|(session_id, _)| session_id == Some(3)));
    }
    /// Script output renders as its own row kind: prints verbatim (llcom's
    /// "[I]-[tag] message" shape survives), errors flagged — the AC's "script
    /// errors surface in the log".
    #[test]
    fn script_rows_render_print_and_error() {
        let print = format_script_row(&ScriptOutEvent {
            session_id: Some(1),
            timestamp: stamp(),
            kind: ScriptOutKind::Print {
                line: "[I]-[uartReceive]\thello".into(),
            },
        });
        assert_eq!(print, "03:04:05.123  ~~  [I]-[uartReceive]\thello");

        let error = format_script_row(&ScriptOutEvent {
            session_id: None,
            timestamp: stamp(),
            kind: ScriptOutKind::Error {
                message: "script error: boom".into(),
            },
        });
        assert_eq!(error, "03:04:05.123  ~~  !! script error: script error: boom");

        assert!(render_event(&Event::ScriptOut(ScriptOutEvent {
            session_id: None,
            timestamp: stamp(),
            kind: ScriptOutKind::Print { line: "x".into() },
        }))
        .is_some());
    }

    /// Data events do not render through `render_event` — they travel to the UI
    /// as raw [`DataRowMsg`]s (the encoding switch re-renders them there); port
    /// events render in the driver's Port arm (chip + row).
    #[test]
    fn render_event_skips_data_and_renders_log() {
        assert!(render_event(&Event::Data(DataEvent {
            session_id: 1,
            direction: Direction::Tx,
            timestamp: stamp(),
            bytes: Payload::from_vec(b"hi".to_vec()),
            hooked: false,
        }))
        .is_none());

        let (session_id, row) = render_event(&Event::Log(LogEvent {
            level: LogLevel::Info,
            timestamp: stamp(),
            message: "note".into(),
            session_id: Some(2),
        }))
        .expect("log renders");
        assert_eq!(session_id, Some(2));
        assert!(row.contains("note"), "{row}");
    }

    /// The plot lifecycle rows (issue #24): a `[plot]` tag, `==` for the
    /// lifecycle notes, `~~ !!` flagging a failure. The driver arm renders the
    /// row (like the MQTT tool's), so `render_event` skips them to avoid double
    /// logging; `format_plot_row` is the row source.
    #[test]
    fn plot_rows_render_lifecycle() {
        let event = |kind| Event::Plot(PlotEvent { timestamp: stamp(), kind });
        assert!(render_event(&event(PlotEventKind::DecoderStarted { name: "d".into() })).is_none());
        let started = format_plot_row(&PlotEvent {
            timestamp: stamp(),
            kind: PlotEventKind::DecoderStarted { name: "d".into() },
        });
        assert!(started.contains("==  [plot] decoder started: d"), "{started}");
        let stopped = format_plot_row(&PlotEvent {
            timestamp: stamp(),
            kind: PlotEventKind::DecoderStopped { name: "d".into() },
        });
        assert!(stopped.contains("==  [plot] decoder stopped: d"), "{stopped}");
        let failed = format_plot_row(&PlotEvent {
            timestamp: stamp(),
            kind: PlotEventKind::DecoderFailed { name: "d".into(), message: "boom".into() },
        });
        assert!(failed.contains("~~") && failed.contains("!! [plot] decoder failed: d: boom"), "{failed}");
    }

    /// The render-boundary detail localization (ticket #25's sweep): the
    /// reconnect machine's two known phrases translate, unknown details and OS
    /// errors pass through, `None` degrades to the localized "unknown".
    #[test]
    fn port_details_localize_at_the_render_boundary() {
        use rcom::core::port::reconnect;
        // The known core phrases: the English dictionary keeps their wording,
        // so the en rows read as before.
        assert_eq!(
            localize_port_detail(Some(reconnect::DETAIL_WATCHING)),
            "waiting for the port to reappear (auto-reconnect)"
        );
        assert_eq!(localize_port_detail(Some(reconnect::DETAIL_RECONNECTED)), "auto-reconnected");
        // Everything else passes through verbatim (OS errors, driver texts).
        assert_eq!(localize_port_detail(Some("Device not found")), "Device not found");
        assert_eq!(localize_port_detail(None), "unknown");
    }

    /// The mock-drawer action vocabulary applies to a control (the AC9 seam:
    /// the app-side type maps onto the core handle's API).
    #[test]
    fn mock_actions_apply_to_the_control() {
        let factory = BackendFactory::with_mode(MockMode::Single(MockScenario::Echo));
        let control = factory.mock_control(MOCK_PORT_NAME).expect("control");
        MockAction::Speed(99.0).apply(&control);
        assert_eq!(control.speed(), 8.0, "clamped to the documented window");
        MockAction::Pause(true).apply(&control);
        assert!(control.paused());
        MockAction::Pause(false).apply(&control);
        assert!(!control.paused());
        MockAction::InjectRx(b"x".to_vec()).apply(&control);
        MockAction::Step.apply(&control);
        MockAction::Trigger(MockTrigger::CorruptFrame).apply(&control);
        MockAction::Restart.apply(&control);
    }
}

/// `RCOM_LUADEMO=1`: run llcom's README sample in a script VM against MOCK0 —
/// its log/timer output shows up as `~~` rows in the log view (ticket #18 demo).
fn luademo_enabled() -> bool {
    std::env::var("RCOM_LUADEMO").is_ok_and(|value| value == "1")
}

/// `RCOM_PLOTDEMO=1|fast`: drive the plot pipeline (issue #24) — the bundled
/// framed-protocol decoder plus a frame feed on MOCK0 (the demo generator, or
/// the framed scenario's own timeline when `RCOM_MOCK=framed`). `fast` raises
/// the generator's rate to fill the 10k-point rings for the render-budget proof.
fn plotdemo_enabled() -> bool {
    demo::PlotDemoMode::from_env() != demo::PlotDemoMode::Off
}
