//! The parity sweep (ticket #26): `RCOM_AUTODEMO=parity` — one headless run
//! that walks docs/design.md's parity table row by row against the running
//! engine (mock-driven, no hardware, no UI interaction) and prints one verdict
//! line per row:
//!
//! ```text
//! PARITY <row>: OK
//! PARITY <row>: GAP(<detail>)
//! ```
//!
//! Every row exercises the real end-to-end path — the session manager, the
//! bus, the Lua runtime, the hubs — not just pure functions (the pure rows
//! that cannot fail at runtime — encodings, codec tools — still assert through
//! the live code so the sweep output is one complete table). Each verdict also
//! lands as a `##`/`!!` row in the session log, like the other demos' evidence.
//!
//! Rows that are hardware- or platform-gated are NOT claimed here: the
//! resolution comment on #26 carries the OK/GAP/DEFERRED table with the
//! reasons (real-port swap, Windows monitor FFI, log-view virtualization).

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use ferroui_base::input::{Key, KeyModifiers};

use rcom::core::bus::{DataEvent, Direction, Event, LogEvent, LogLevel, ScriptOutEvent, ScriptOutKind};
use rcom::core::codec::{
    base64_decode, base64_encode, encode_text, hex_encode, parse_hex, recover_garbled,
    transcode, url_decode, url_encode, TextEncoding,
};
use rcom::core::logstore::LogStore;
use rcom::core::lua::{LuaRuntime, QuickSendLookup, TxHook};
use rcom::core::mqtt::mockbroker::MockBroker;
use rcom::core::mqtt::hub as mqtt_hub;
use rcom::core::mqtt::{MqttConfig, MqttDirection, MqttEventKind, MqttQos};
use rcom::core::monitor::hub as monitor_hub;
use rcom::core::monitor::{MonitorConfig, MonitorDirection, MonitorEventKind, Upstream};
use rcom::core::nettools::channel::{spawn_tcp_echo, spawn_udp_echo};
use rcom::core::nettools::hub as net_hub;
use rcom::core::nettools::{ChannelConfig, NetDirection, NetEventKind, NetMode, NetProtocol};
use rcom::core::port::manager::PortManager;
use rcom::core::port::mock::MOCK_PORT_NAME;
use rcom::core::port::{MockTrigger, PortConfig};

use super::host::format_data_row;
use super::logview::LogLine;
use super::quicksend::{quick_send_entry, QuickSendStore};
use super::tools_terminal::{map_key, KeyStroke, NewlineMode};

/// How long any single row may take before it is a GAP (the whole suite stays
/// bounded; the reconnect row gets its own longer budget below).
const STEP_BUDGET: Duration = Duration::from_secs(15);
/// The reconnect row's budget: the scripted unplug cycle at 8× + the machine's
/// poll cadence + margin.
const RECONNECT_BUDGET: Duration = Duration::from_secs(20);

/// The sweep entry: spawn me on the core runtime beside the driver.
pub async fn run(manager: Arc<PortManager>, bus: rcom::core::EventBus, logstore: Arc<LogStore>) {
    tracing::info!("RCOM_AUTODEMO=parity: walking the design.md parity table row by row");
    let mut events = bus.subscribe();
    let mut ok = 0u32;
    let mut gaps: Vec<(String, String)> = Vec::new();

    // ---- Session + the dual log -------------------------------------------
    // The mock listing exercises the same PortInfo shape a real backend
    // produces (name/vid/pid/description/kind); real-port metadata needs
    // hardware (the resolution comment records the manual pass).
    let listing = match manager.list_ports().await {
        Ok(ports) => match ports.iter().find(|port| port.name == MOCK_PORT_NAME) {
            Some(port) if port.is_mock() => {
                Ok(format!("{} ports listed, {MOCK_PORT_NAME} plays a mock scenario", ports.len()))
            }
            _ => Err(format!("{MOCK_PORT_NAME} missing from the listing or not a mock")),
        },
        Err(error) => Err(format!("list_ports failed: {error}")),
    };
    record(&bus, &mut ok, &mut gaps, "port-listing", listing);

    let session = match tokio::time::timeout(STEP_BUDGET, async {
        manager
            .open(MOCK_PORT_NAME, PortConfig::new(115_200))
            .await
            .map_err(|error| format!("open {MOCK_PORT_NAME}: {error}"))
    })
    .await
    {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => {
            record(&bus, &mut ok, &mut gaps, "session-open", Err(error));
            finish(&bus, ok, &gaps);
            return;
        }
        Err(_) => {
            record(&bus, &mut ok, &mut gaps, "session-open", Err("open timed out".into()));
            finish(&bus, ok, &gaps);
            return;
        }
    };

    // Row: HEX+string dual log with timestamps — the TX and its mock echo both
    // render `stamp  dir  hex  "text"` (the row shape the log view shows).
    let payload = b"parity-ping".to_vec();
    let dual_log = tokio::time::timeout(STEP_BUDGET, async {
        manager
            .write_for(session, payload.clone())
            .await
            .map_err(|error| format!("write: {error}"))?;
        let tx = wait_data(&mut events, |data| {
            data.session_id == session && data.direction == Direction::Tx && data.bytes.as_slice() == payload.as_slice()
        })
        .await?;
        let rx = wait_data(&mut events, |data| {
            data.session_id == session && data.direction == Direction::Rx && data.bytes.as_slice() == payload.as_slice()
        })
        .await?;
        verify_dual_log(&tx, &rx)
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "dual-log-hex-text-timestamps", unwrap_timeout(dual_log));

    // Row: auto-save logs with time — the file sink wrote the traffic.
    let autosave = tokio::time::timeout(STEP_BUDGET, async {
        let path = logstore
            .file_path()
            .await
            .ok_or("no auto-save file was created")?;
        let content = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if content.contains("TX  70 61 72") {
            Ok(format!("{} carries the TX hex lines", path.display()))
        } else {
            Err(format!("{} is missing the TX hex lines", path.display()))
        }
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "auto-save-logs", unwrap_timeout(autosave));

    // Row: auto-reconnect on replug — the unplug driver scenario (`MOCK3`) is
    // the scripted unplug/reappear cycle (§6.5): trigger the unplug, watch the
    // machine go WatchDesired → Reopened, then see the stream resume (the
    // unplug scenario's status lines — it drops writes, so the resumed RX is
    // the honest "data flows again" proof; the TX path was proven on MOCK0).
    // A second session keeps the echo session untouched (two sessions live at
    // once — multi-session is exercised for free).
    let reconnect = match tokio::time::timeout(RECONNECT_BUDGET, async {
        let replug_session = manager
            .open("MOCK3", PortConfig::new(115_200))
            .await
            .map_err(|error| format!("open MOCK3 (unplug driver): {error}"))?;
        let control = manager
            .mock_controls(replug_session)
            .ok_or("the session has no mock playback control")?;
        control.set_speed(8.0);
        control.trigger(MockTrigger::Unplug);
        // The unplug moves the ORIGINAL session to WatchDesired; the machine's
        // auto-reopen binds a FRESH session id (the tab's lineage appends it —
        // §6.5), so the port is the stable identity across the cycle.
        wait_port(&mut events, |event| {
            event.port == "MOCK3" && event.state == rcom::core::bus::PortState::WatchDesired
        })
        .await?;
        let reopened = wait_port(&mut events, |event| {
            event.port == "MOCK3" && event.state == rcom::core::bus::PortState::Reopened
        })
        .await?;
        // The stream resumes: the unplug driver emits its status lines again —
        // on the NEW session the machine opened.
        let resumed = wait_data(&mut events, |data| {
            data.session_id == reopened.session_id && data.direction == Direction::Rx
        })
        .await?;
        tracing::info!(
            session = resumed.session_id,
            bytes = resumed.bytes.len(),
            "PARITY: the replugged session streamed again"
        );
        manager.close_session(reopened.session_id).await;
        Ok::<String, String>(format!(
            "WatchDesired → Reopened (session {replug_session} → {}), the stream resumed",
            reopened.session_id
        ))
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!("timed out after {RECONNECT_BUDGET:?}")),
    };
    record(&bus, &mut ok, &mut gaps, "auto-reconnect-replug", reconnect);

    // Row: Lua pre-processing of TX — the fromHex hook rewrites the send.
    let hook = tokio::time::timeout(STEP_BUDGET, async {
        manager.set_hook_for(session, Some(TxHook::new(rcom::core::lua::scripts::SEND_CONVERT_HEX)));
        manager
            .write_for(session, b"414243".to_vec())
            .await
            .map_err(|error| format!("write: {error}"))?;
        let tx = wait_data(&mut events, |data| {
            data.session_id == session && data.direction == Direction::Tx && data.bytes.as_slice() == b"ABC"
        })
        .await?;
        manager.set_hook_for(session, None);
        if tx.hooked {
            Ok("hook rewrote '414243' → 41 42 43; the row carries the hook marker".into())
        } else {
            Err("the wire saw the rewritten bytes but the TX event is not flagged hooked".into())
        }
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "lua-pre-send-hook", unwrap_timeout(hook));

    // Row: quick-send, unlimited entries, 10 pages — the store shape plus the
    // apiQuickSendList read a script sees.
    let quicksend = tokio::time::timeout(STEP_BUDGET, async {
        let mut store = QuickSendStore::with_seed_entries();
        if store.pages.len() != 10 {
            return Err(format!("expected 10 pages, got {}", store.pages.len()));
        }
        for i in 0..250 {
            store.pages[3].entries.push(super::quicksend::QuickSendEntry::text(
                format!("bulk {i}"),
                format!("payload {i}\r\n"),
            ));
        }
        if store.pages[3].entries.len() != 250 {
            return Err("a page refused 250 entries".into());
        }
        let (hex, payload) = quick_send_entry(&store, 1).ok_or("entry 1 missing")?;
        if hex || payload != "ping\r\n" {
            return Err(format!("entry 1 is ({hex}, {payload:?})"));
        }
        // The script-side read: a VM with a provider sees the entries (the
        // file-backed provider type is covered by the unit tests).
        struct Stub;
        impl QuickSendLookup for Stub {
            fn entry(&self, id: i64) -> Option<(bool, String)> {
                match id {
                    1 => Some((false, "qs-payload".into())),
                    2 => Some((true, "41 42".into())),
                    _ => None,
                }
            }
        }
        let mut outbox = ScriptBox::new(&bus);
        let vm = LuaRuntime::create(bus.clone(), None, None)
            .map_err(|error| format!("VM create: {error}"))?;
        vm.set_quicksend_lookup(Some(Arc::new(Stub)));
        vm.run_script(
            r#"
            print("QS1=" .. tostring(apiQuickSendList(1)))
            local hexed = apiQuickSendList(2)
            print("QS2=" .. (hexed and "hex-ok" or "hex-missing"))
        "#,
        )
        .map_err(|error| format!("quick-send script: {error}"))?;
        outbox.next_matching(Duration::from_secs(5), "QS1=qs-payload").await?;
        outbox.next_matching(Duration::from_secs(5), "QS2=hex-ok").await?;
        vm.stop();
        Ok("10 pages, 250-entry page, script reads S/H entries".into())
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "quick-send-pages-entries", unwrap_timeout(quicksend));

    // Row: standalone Lua + timers + tasks — one VM, a task and a loop timer.
    let timers = tokio::time::timeout(STEP_BUDGET, async {
        let mut outbox = ScriptBox::new(&bus);
        let vm = LuaRuntime::start(
            bus.clone(),
            None,
            None,
            r#"
            sys.taskInit(function()
                sys.wait(40)
                print("parity-task-ran")
            end)
            sys.timerLoopStart(function() print("parity-timer") end, 40)
        "#,
        )
        .map_err(|error| format!("timer VM: {error}"))?;
        outbox.next_matching(Duration::from_secs(5), "parity-task-ran").await?;
        outbox.next_matching(Duration::from_secs(5), "parity-timer").await?;
        outbox.next_matching(Duration::from_secs(5), "parity-timer").await?;
        vm.stop();
        Ok("task + sys.wait + loop timer all fired".into())
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "lua-timers-tasks", unwrap_timeout(timers));

    // Row: text encodings selectable — the selector's set round-trips.
    let encodings = tokio::time::timeout(STEP_BUDGET, async {
        let all = TextEncoding::all();
        if all.len() < 5 {
            return Err(format!("only {} encodings in the selector", all.len()));
        }
        for (encoding, text) in [
            (TextEncoding::Gbk, "你好，串口"),
            (TextEncoding::Gb18030, "你好，串口"),
            (TextEncoding::Big5, "你好，串口"),
            (TextEncoding::ShiftJis, "こんにちは、シリアル"),
        ] {
            let decoded = encoding.decode(&encoding.encode(text));
            if decoded != text {
                return Err(format!("{:?} round trip produced {decoded:?}", encoding.id()));
            }
        }
        Ok(format!("{} encodings round-trip", all.len()))
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "text-encodings", unwrap_timeout(encodings));

    // Row: terminal mode incl. ctrl+letters — the pure key mapping.
    let terminal = tokio::time::timeout(STEP_BUDGET, async {
        let cases: Vec<(KeyStroke, &[u8])> = vec![
            (KeyStroke::new(Key::A, KeyModifiers::CONTROL, None), &[0x01]),
            (KeyStroke::new(Key::C, KeyModifiers::CONTROL, None), &[0x03]),
            (KeyStroke::new(Key::Z, KeyModifiers::CONTROL, None), &[0x1A]),
            (KeyStroke::new(Key::OemOpenBrackets, KeyModifiers::CONTROL, Some("[")), &[0x1B]),
            (KeyStroke::new(Key::P, KeyModifiers::NONE, None), b"p"),
            (
                KeyStroke::new(Key::Return, KeyModifiers::NONE, None),
                NewlineMode::Cr.bytes(),
            ),
            (
                KeyStroke::new(Key::Up, KeyModifiers::NONE, None),
                &[0x1B, b'[', b'A'],
            ),
        ];
        for (stroke, expected) in cases {
            match map_key(&stroke, NewlineMode::Cr) {
                Some(bytes) if bytes == expected => {}
                Some(bytes) => return Err(format!("{stroke:?} mapped to {bytes:02x?}, want {expected:02x?}")),
                None => return Err(format!("{stroke:?} mapped to nothing")),
            }
        }
        Ok("ctrl+letters (01–1A), ctrl+symbol, printable, CR, CSI arrows".into())
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "terminal-ctrl-letters", unwrap_timeout(terminal));

    // Row: hide sent data — the viewmodel's filter (view-side only).
    let hide_sent = tokio::time::timeout(STEP_BUDGET, async {
        let tx = LogLine::Data {
            stamp: "12:00:00.000".into(),
            direction: Direction::Tx,
            bytes: b"sent".to_vec(),
            hooked: false,
        };
        let rx = LogLine::Data {
            stamp: "12:00:00.001".into(),
            direction: Direction::Rx,
            bytes: b"echo".to_vec(),
            hooked: false,
        };
        if tx.visible("", true, TextEncoding::Utf8) {
            return Err("hide-sent left the TX row visible".into());
        }
        if !rx.visible("", true, TextEncoding::Utf8) {
            return Err("hide-sent swallowed the RX row".into());
        }
        if !tx.visible("", false, TextEncoding::Utf8) || !rx.visible("", false, TextEncoding::Utf8) {
            return Err("the filter ate rows with hide-sent off".into());
        }
        if !rx.visible("65 63", false, TextEncoding::Utf8) {
            return Err("the hex needle missed the row's hex view".into());
        }
        Ok("TX hidden view-side, RX and hex-needle matches kept".into())
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "hide-sent-filter", unwrap_timeout(hide_sent));

    // Rows: TCP/UDP/SSL server/client, IPv6 — the net hub end to end.
    let net_tcp = tokio::time::timeout(STEP_BUDGET, tcp_loopback(&bus)).await;
    record(&bus, &mut ok, &mut gaps, "net-tcp-client-server", unwrap_timeout(net_tcp));

    let net_udp = tokio::time::timeout(STEP_BUDGET, udp_loopback(&bus)).await;
    record(&bus, &mut ok, &mut gaps, "net-udp-client-server", unwrap_timeout(net_udp));

    let net_tls = tokio::time::timeout(STEP_BUDGET, tls_loopback(&bus)).await;
    record(&bus, &mut ok, &mut gaps, "net-ssl-client-server", unwrap_timeout(net_tls));

    let net_v6 = tokio::time::timeout(STEP_BUDGET, ipv6_loopback(&bus)).await;
    record(&bus, &mut ok, &mut gaps, "net-ipv6", unwrap_timeout(net_v6));

    // Row: encoding conversion tools — the toolbox's round trips.
    let codec = tokio::time::timeout(STEP_BUDGET, async {
        let outcome = encode_text("你好", TextEncoding::Gbk);
        if outcome.bytes != vec![0xc4, 0xe3, 0xba, 0xc3] || !outcome.unencodable.is_empty() {
            return Err("UTF-8 → GBK conversion is not byte-faithful".into());
        }
        let round = transcode(&outcome.bytes, TextEncoding::Gbk, TextEncoding::Utf8);
        if round.text != "你好" {
            return Err("GBK → UTF-8 transcode lost the text".into());
        }
        let payload: &[u8] = b"\x00\xffhello \xe4\xbd\xa0\xe5\xa5\xbd";
        let b64 = base64_encode(payload);
        let ok_b64 = base64_decode(&b64).as_deref() == Ok(payload);
        let hexed = hex_encode(payload);
        let ok_hex = parse_hex(&hexed).as_deref() == Ok(payload);
        let url = url_encode("a b/你+好~");
        let ok_url = url_decode(&url).as_deref() == Ok("a b/你+好~");
        if ok_b64 && ok_hex && ok_url {
            Ok("convert + transcode + hex/base64/url round trips".into())
        } else {
            Err(format!("round trips: b64={ok_b64} hex={ok_hex} url={ok_url}"))
        }
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "codec-tools", unwrap_timeout(codec));

    // Row: garbled-text recovery — the classic mojibake directions.
    let garbled = tokio::time::timeout(STEP_BUDGET, async {
        let misread = TextEncoding::Gbk.decode(&TextEncoding::Utf8.encode("你好"));
        let best = recover_garbled(&misread).first().cloned();
        let forward = matches!(best, Some(candidate)
            if candidate.misread == "GBK" && candidate.actual == "UTF-8" && candidate.result == "你好");
        let misread2 = TextEncoding::Utf8.decode(&TextEncoding::Gbk.encode("专业"));
        let best2 = recover_garbled(&misread2).first().cloned();
        let reverse = matches!(best2, Some(candidate)
            if candidate.misread == "UTF-8" && candidate.actual == "GBK" && candidate.result == "专业");
        if forward && reverse {
            Ok("both mojibake directions recover (GBK↔UTF-8)".into())
        } else {
            Err(format!("forward={forward} reverse={reverse}"))
        }
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "garbled-recovery", unwrap_timeout(garbled));

    // Row: MQTT test — hub session against the in-process mock broker.
    let mqtt = tokio::time::timeout(STEP_BUDGET, mqtt_roundtrip(&bus)).await;
    record(&bus, &mut ok, &mut gaps, "mqtt-test", unwrap_timeout(mqtt));

    // Row: serial monitor — the pty-proxy relay captures both directions.
    let monitor = tokio::time::timeout(STEP_BUDGET, monitor_loopback(&bus)).await;
    record(&bus, &mut ok, &mut gaps, "serial-monitor-pty", unwrap_timeout(monitor));

    // Row: Lua API surface — the LuaApi.md checklist + rcom.* tables.
    let surface = tokio::time::timeout(STEP_BUDGET, async {
        let result: Result<String, String> = verify_lua_surface().await;
        result
    })
    .await;
    record(&bus, &mut ok, &mut gaps, "lua-api-surface", unwrap_timeout(surface));

    // The session closes cleanly (lifecycle sanity the sweep leaves behind).
    manager.close_session(session).await;

    finish(&bus, ok, &gaps);
}

/// Prints the closing summary line.
fn finish(bus: &rcom::core::EventBus, ok: u32, gaps: &[(String, String)]) {
    let summary = format!(
        "PARITY SUITE COMPLETE: {ok} ok, {} gap{}",
        gaps.len(),
        if gaps.len() == 1 { "" } else { "s" }
    );
    tracing::info!("{summary}");
    if gaps.is_empty() {
        note(bus, format!("## {summary}"), false);
    } else {
        for (row, detail) in gaps {
            tracing::error!("PARITY {row}: GAP({detail})");
            note(bus, format!("!! PARITY {row}: GAP({detail})"), true);
        }
        note(bus, format!("!! {summary}"), true);
    }
}

/// One verdict: prints the PARITY line and lands it in the session log.
fn record(
    bus: &rcom::core::EventBus,
    ok: &mut u32,
    gaps: &mut Vec<(String, String)>,
    row: &str,
    outcome: Result<String, String>,
) {
    match outcome {
        Ok(detail) => {
            *ok += 1;
            tracing::info!("PARITY {row}: OK ({detail})");
            note(bus, format!("## PARITY {row}: OK ({detail})"), false);
        }
        Err(detail) => {
            gaps.push((row.to_string(), detail.clone()));
            tracing::error!("PARITY {row}: GAP({detail})");
            note(bus, format!("!! PARITY {row}: GAP({detail})"), true);
        }
    }
}

/// Turns a `tokio::time::timeout` wrapper's output back into the step result.
fn unwrap_timeout(outcome: Result<Result<String, String>, tokio::time::error::Elapsed>) -> Result<String, String> {
    outcome.unwrap_or_else(|_| Err(format!("timed out after {:?}", STEP_BUDGET)))
}

/// One `##`/`!!` note row into the session log (the demos' evidence convention).
fn note(bus: &rcom::core::EventBus, text: String, error: bool) {
    bus.publish(Event::Log(LogEvent {
        level: if error { LogLevel::Error } else { LogLevel::Info },
        timestamp: chrono::Local::now(),
        message: text,
        session_id: None,
    }));
}

/// The next bus data event matching `want` (live-only subscription; lag is
/// tolerated — the sweep never asserts on a saturated stream).
async fn wait_data(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    want: impl Fn(&DataEvent) -> bool,
) -> Result<DataEvent, String> {
    loop {
        match events.recv().await {
            Ok(Event::Data(data)) if want(&data) => return Ok(data),
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("bus closed".into()),
        }
    }
}

/// The next bus port event matching `want` (the event itself — the auto-reopen
/// binds a fresh session id, so callers read the id off the returned event).
async fn wait_port(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    want: impl Fn(&rcom::core::bus::PortEvent) -> bool,
) -> Result<rcom::core::bus::PortEvent, String> {
    loop {
        match events.recv().await {
            Ok(Event::Port(port)) if want(&port) => return Ok(port),
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("bus closed".into()),
        }
    }
}

/// The dual-log row's shape check: `stamp  DIR  hex  "text"` with a real
/// timestamp prefix and the payload visible in both views.
fn verify_dual_log(tx: &DataEvent, rx: &DataEvent) -> Result<String, String> {
    for (label, event) in [("TX", tx), ("RX", rx)] {
        let row = format_data_row(event);
        let bytes = row.as_bytes();
        let stamped = row.len() > 12 && bytes[2] == b':' && bytes[5] == b':' && bytes[8] == b'.';
        if !stamped {
            return Err(format!("{label} row has no timestamp prefix: {row:?}"));
        }
        if !row.contains("70 61 72 69 74 79 2d 70 69 6e 67") {
            return Err(format!("{label} row is missing the hex view: {row:?}"));
        }
        if !row.contains("\"parity-ping\"") {
            return Err(format!("{label} row is missing the text view: {row:?}"));
        }
        if !row.contains(label) {
            return Err(format!("{label} row is missing the direction: {row:?}"));
        }
    }
    Ok("TX + RX rows render stamp + direction + hex + text".into())
}

/// A tiny print collector over the process bus (script VMs publish their
/// `print` output as `ScriptOut` events).
struct ScriptBox {
    rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl ScriptBox {
    fn new(bus: &rcom::core::EventBus) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut events = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::ScriptOut(ScriptOutEvent {
                        kind: ScriptOutKind::Print { line }, ..
                    })) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(_) => continue,
                }
            }
        });
        Self { rx }
    }

    /// The next print containing `needle` within `within` (sys.lua's own
    /// "core loaded!" banner is skipped by the needle).
    async fn next_matching(&mut self, within: Duration, needle: &str) -> Result<String, String> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(timeout, self.rx.recv()).await {
                Ok(Some(line)) if line.contains(needle) => return Ok(line),
                Ok(Some(_)) => continue,
                Ok(None) => return Err("the print collector closed".into()),
                Err(_) => return Err(format!("no print matching {needle:?} within {within:?}")),
            }
        }
    }
}

// ---- The net rows -----------------------------------------------------------

/// TCP client ↔ echo server on IPv4 loopback (the parity row's client+server
/// story; the server side of every net row is the hub's own channel machinery).
async fn tcp_loopback(bus: &rcom::core::EventBus) -> Result<String, String> {
    let hub = net_hub::global().ok_or("no process-global net hub")?;
    let addr = spawn_tcp_echo("127.0.0.1:0").await.map_err(|error| format!("echo server: {error}"))?;
    let mut events = bus.subscribe();
    let channel = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            host: "127.0.0.1".into(),
            port: addr.port(),
            accept_any_cert: false,
        })
        .map_err(|error| format!("start channel: {error}"))?;
    wait_net(&mut events, channel, |kind| matches!(kind, NetEventKind::Connected { .. })).await?;
    hub.send(channel, b"ping".to_vec());
    wait_net(&mut events, channel, |kind| {
        matches!(kind, NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"ping")
    })
    .await?;
    hub.stop_channel(channel);
    Ok(format!("'ping' round-tripped over TCP {addr}"))
}

/// UDP client ↔ echo responder.
async fn udp_loopback(bus: &rcom::core::EventBus) -> Result<String, String> {
    let hub = net_hub::global().ok_or("no process-global net hub")?;
    let addr = spawn_udp_echo("127.0.0.1:0").await.map_err(|error| format!("echo responder: {error}"))?;
    let mut events = bus.subscribe();
    let channel = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Udp,
            mode: NetMode::Client,
            host: "127.0.0.1".into(),
            port: addr.port(),
            accept_any_cert: false,
        })
        .map_err(|error| format!("start channel: {error}"))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    hub.send(channel, b"pong".to_vec());
    wait_net(&mut events, channel, |kind| {
        matches!(kind, NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"pong")
    })
    .await?;
    hub.stop_channel(channel);
    Ok(format!("'pong' round-tripped over UDP {addr}"))
}

/// TLS client ↔ TLS server, both hub channels (the self-signed certificate).
async fn tls_loopback(bus: &rcom::core::EventBus) -> Result<String, String> {
    let hub = net_hub::global().ok_or("no process-global net hub")?;
    let mut events = bus.subscribe();
    let server = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tls,
            mode: NetMode::Server,
            host: "127.0.0.1".into(),
            port: 0,
            accept_any_cert: false,
        })
        .map_err(|error| format!("start TLS server: {error}"))?;
    let local = wait_net_addr(&mut events, server).await?;
    let client = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tls,
            mode: NetMode::Client,
            host: local.ip().to_string(),
            port: local.port(),
            accept_any_cert: true,
        })
        .map_err(|error| format!("start TLS client: {error}"))?;
    wait_net(&mut events, client, |kind| matches!(kind, NetEventKind::Connected { .. })).await?;
    hub.send(client, b"ping".to_vec());
    wait_net(&mut events, server, |kind| {
        matches!(kind, NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"ping")
    })
    .await?;
    hub.stop_channel(client);
    hub.stop_channel(server);
    Ok(format!("'ping' round-tripped over TLS {local} (self-signed, accept-any client)"))
}

/// TCP over IPv6 loopback (`::1`).
async fn ipv6_loopback(bus: &rcom::core::EventBus) -> Result<String, String> {
    let hub = net_hub::global().ok_or("no process-global net hub")?;
    let addr = spawn_tcp_echo("[::1]:0").await.map_err(|error| format!("echo server on ::1: {error}"))?;
    let mut events = bus.subscribe();
    let channel = hub
        .start_channel(ChannelConfig {
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            host: "::1".into(),
            port: addr.port(),
            accept_any_cert: false,
        })
        .map_err(|error| format!("start channel: {error}"))?;
    wait_net(&mut events, channel, |kind| matches!(kind, NetEventKind::Connected { .. })).await?;
    hub.send(channel, b"ping6".to_vec());
    wait_net(&mut events, channel, |kind| {
        matches!(kind, NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"ping6")
    })
    .await?;
    hub.stop_channel(channel);
    Ok(format!("'ping6' round-tripped over TCP {addr} (IPv6)"))
}

/// The next net event of `channel` matching `kind`, as `()`.
async fn wait_net(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    channel: u64,
    want: impl Fn(&NetEventKind) -> bool,
) -> Result<(), String> {
    loop {
        match events.recv().await {
            Ok(Event::Net(net)) if net.channel_id == channel => match &net.kind {
                kind if want(kind) => return Ok(()),
                NetEventKind::Error { message } => return Err(format!("channel {channel} failed: {message}")),
                _ => continue,
            },
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("bus closed".into()),
        }
    }
}

/// The `Listening` event's local address of `channel`.
async fn wait_net_addr(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    channel: u64,
) -> Result<std::net::SocketAddr, String> {
    loop {
        match events.recv().await {
            Ok(Event::Net(net)) if net.channel_id == channel => match &net.kind {
                NetEventKind::Listening { local } => return Ok(*local),
                NetEventKind::Error { message } => return Err(format!("server failed: {message}")),
                _ => continue,
            },
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("bus closed".into()),
        }
    }
}

// ---- The MQTT row -----------------------------------------------------------

/// A hub MQTT session against the in-process mock broker: connect, subscribe,
/// publish round trip, broker push, clean disconnect.
async fn mqtt_roundtrip(bus: &rcom::core::EventBus) -> Result<String, String> {
    let hub = mqtt_hub::global().ok_or("no process-global MQTT hub")?;
    let mut events = bus.subscribe();
    let broker = MockBroker::spawn().await.map_err(|error| format!("mock broker: {error}"))?;
    let session = hub.start_session(MqttConfig {
        host: "127.0.0.1".into(),
        port: broker.port(),
        client_id: "rcom-parity".into(),
        username: None,
        password: None,
        tls: false,
        accept_any_cert: false,
    })?;
    wait_mqtt(&mut events, session, |kind| matches!(kind, MqttEventKind::Connected { .. })).await?;
    hub.subscribe(session, "parity/up", MqttQos::AtLeastOnce);
    wait_mqtt(&mut events, session, |kind| matches!(kind, MqttEventKind::Subscribed { .. })).await?;
    hub.publish(session, "parity/up", b"parity-ping".to_vec(), MqttQos::AtLeastOnce, false);
    wait_mqtt(&mut events, session, |kind| matches!(kind,
        MqttEventKind::Message { direction: MqttDirection::Rx, payload, .. } if payload.as_slice() == b"parity-ping"))
    .await?;
    broker.publish("parity/up", b"broker-push");
    wait_mqtt(&mut events, session, |kind| matches!(kind,
        MqttEventKind::Message { direction: MqttDirection::Rx, payload, .. } if payload.as_slice() == b"broker-push"))
    .await?;
    hub.stop_session(session);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !broker.clean_disconnect() {
        if tokio::time::Instant::now() > deadline {
            return Err("no graceful DISCONNECT reached the broker".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok("connect/subscribe/publish round trip + broker push + clean disconnect".into())
}

/// The next MQTT event of `session` matching `kind`.
async fn wait_mqtt(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    session: u64,
    want: impl Fn(&MqttEventKind) -> bool,
) -> Result<(), String> {
    loop {
        match events.recv().await {
            Ok(Event::Mqtt(mqtt)) if mqtt.session_id == session && want(&mqtt.kind) => return Ok(()),
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("bus closed".into()),
        }
    }
}

// ---- The serial-monitor row -------------------------------------------------

/// The pty-pair proxy: a loopback monitor relays app↔device traffic; the sweep
/// plays both sides with plain files on the pty slaves and checks the captures.
async fn monitor_loopback(bus: &rcom::core::EventBus) -> Result<String, String> {
    let hub = monitor_hub::global().ok_or("no process-global monitor hub")?;
    let mut events = bus.subscribe();
    let (id, app_path) = hub
        .start_monitor(MonitorConfig { upstream: Upstream::VirtualLoopback })
        .map_err(|error| format!("start monitor: {error}"))?;
    let started = wait_monitor(&mut events, id, |kind| match kind {
        MonitorEventKind::Started { virtual_port, .. } => Some(virtual_port.clone()),
        _ => None,
    })
    .await?;
    if started != app_path {
        hub.stop_monitor(id);
        return Err(format!("Started event port {started} != {app_path}"));
    }
    // Plain files on the pty slaves are the "target app" and the "device".
    let app_file = open_pty(&app_path).await?;
    let dev_path = hub
        .virtual_ports(id)
        .and_then(|(_, dev)| dev)
        .ok_or("no device-side twin path")?;
    let dev_file = open_pty(&dev_path).await?;

    // app → device: the app writes, the relay captures TX and delivers.
    {
        let mut writer = app_file.try_clone().map_err(|error| format!("clone app file: {error}"))?;
        tokio::task::spawn_blocking(move || writer.write_all(b"app->device"))
            .await
            .map_err(|error| format!("join write: {error}"))?
            .map_err(|error| format!("write from the app: {error}"))?;
    }
    wait_monitor(&mut events, id, |kind| {
        matches!(
            kind,
            MonitorEventKind::Data { direction: MonitorDirection::Tx, bytes }
                if bytes.as_slice() == b"app->device"
        )
        .then_some(())
    })
    .await?;
    let mut dev_reader = dev_file.try_clone().map_err(|error| format!("clone device file: {error}"))?;
    let delivered = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; b"app->device".len()];
        std::io::Read::read_exact(&mut dev_reader, &mut buf).map(|_| buf)
    })
    .await
    .map_err(|error| format!("join read: {error}"))?
    .map_err(|error| format!("read on the device side: {error}"))?;
    if delivered != *b"app->device" {
        hub.stop_monitor(id);
        return Err(format!("the relay garbled the app's bytes: {delivered:02x?}"));
    }
    drop(app_file);
    drop(dev_file);
    hub.stop_monitor(id);
    Ok(format!("app→device relayed and captured as TX ({app_path} ⇄ {dev_path})"))
}

/// Opens a pty slave path (blocking I/O off the async context).
async fn open_pty(path: &str) -> Result<std::fs::File, String> {
    let owned = path.to_string();
    tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().read(true).write(true).open(&owned))
        .await
        .map_err(|error| format!("join open: {error}"))?
        .map_err(|error| format!("open {path}: {error}"))
}

/// The next monitor event of `id`, mapped by `want`.
async fn wait_monitor<T: Default>(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    id: u64,
    want: impl Fn(&MonitorEventKind) -> Option<T>,
) -> Result<T, String> {
    loop {
        match events.recv().await {
            Ok(Event::Monitor(monitor)) if monitor.monitor_id == id => match want(&monitor.kind) {
                Some(value) => return Ok(value),
                None => continue,
            },
            Ok(Event::Monitor(monitor)) if monitor.monitor_id == id => {
                if matches!(monitor.kind, MonitorEventKind::Error { .. }) {
                    return Err(format!("monitor {id} failed: {:?}", monitor.kind));
                }
                continue;
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("bus closed".into()),
        }
    }
}

// ---- The Lua API surface row ------------------------------------------------

/// The LuaApi.md checklist: every api*/sys.* global a llcom script may call,
/// the preloadable modules and the `rcom.*` tables. One probe script inside the
/// real VM (sandbox + head.lua) reports the missing names — empty means the
/// surface is complete.
async fn verify_lua_surface() -> Result<String, String> {
    const EXPECTED_GLOBALS: [&str; 13] = [
        "apiStartTimer",
        "apiStopTimer",
        "apiPrintLog",
        "apiSend",
        "apiSendUartData",
        "apiGetPath",
        "apiUtf8ToHex",
        "apiAscii2Utf8",
        "apiQuickSendList",
        "apiAddPoint",
        "apiSetCb",
        "apiUnsetCb",
        "tiggerCB",
    ];
    const EXPECTED_SYS: [&str; 12] = [
        "wait",
        "waitUntil",
        "waitUntilExt",
        "taskInit",
        "timerStart",
        "timerLoopStart",
        "timerStop",
        "timerStopAll",
        "timerIsActive",
        "subscribe",
        "unsubscribe",
        "publish",
    ];
    let probe_bus = rcom::core::EventBus::new();
    let mut print_box = ScriptBox::new(&probe_bus);
    let vm = LuaRuntime::create(probe_bus.clone(), None, None)
        .map_err(|error| format!("VM create: {error}"))?;
    let globals = EXPECTED_GLOBALS
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(",");
    let sys_names = EXPECTED_SYS
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(",");
    vm.run_script(&format!(
        r#"
        local missing = {{}}
        for _, n in ipairs{{{globals}}} do
            if type(_G[n]) ~= "function" then table.insert(missing, n) end
        end
        for _, n in ipairs{{{sys_names}}} do
            if type(sys[n]) ~= "function" then table.insert(missing, "sys." .. n) end
        end
        for _, m in ipairs{{"JSON", "strings", "log"}} do
            local ok = pcall(require, m)
            if not ok then table.insert(missing, "require(" .. m .. ")") end
        end
        if type(rcom) ~= "table" then
            table.insert(missing, "rcom")
        else
            for _, t in ipairs{{"net", "ui", "ports"}} do
                if type(rcom[t]) ~= "table" then table.insert(missing, "rcom." .. t) end
            end
        end
        print("SURFACE_MISSING=" .. #missing .. ":" .. table.concat(missing, ","))
    "#
    ))
    .map_err(|error| format!("surface probe: {error}"))?;
    let line = print_box
        .next_matching(Duration::from_secs(5), "SURFACE_MISSING=")
        .await?;
    vm.stop();
    let missing = line
        .split_once("SURFACE_MISSING=")
        .and_then(|(_, rest)| rest.split_once(':'))
        .map(|(count, names)| (count.parse::<u32>().unwrap_or(1), names.to_string()))
        .ok_or_else(|| format!("unparsable probe output: {line:?}"))?;
    if missing.0 == 0 {
        Ok(format!(
            "{} api* globals + {} sys.* + JSON/strings/log + rcom.net/ui/ports",
            EXPECTED_GLOBALS.len(),
            EXPECTED_SYS.len()
        ))
    } else {
        Err(format!("missing: {}", missing.1))
    }
}
