//! The `RCOM_MONITORDEMO` autodemo: the serial monitor proving itself in
//! loopback mode — both ends virtual, the "target app" and the "device" are
//! plain `File` I/O on the two pty slaves, everything through the real hub and
//! relay. Evidence: `AUTODEMO MONITOR OK` in the trace log plus `##` rows in
//! the session log view, with both capture directions and their timestamps.
//!
//! `RCOM_MONITORDEMO=1` — loopback round trip:
//!
//! ```text
//! demo app ─write→ [pty A slave] ⇄ master A ⇄ relay ⇄ master B ⇄ [pty B slave] ←write─ device
//!            ↑ captured as app TX (ts)                    captured as app RX (ts) ↑
//! ```

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crate::core::bus::{Event, EventBus, LogEvent, LogLevel};
use crate::core::monitor::hub::MonitorHub;
use crate::core::monitor::{MonitorConfig, MonitorDirection, MonitorEventKind, Upstream};

/// How long any demo phase may take before the run is declared failed.
const DEMO_TIMEOUT: Duration = Duration::from_secs(20);

/// The demo mode requested by `RCOM_MONITORDEMO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorDemoMode {
    Off,
    Loopback,
}

impl MonitorDemoMode {
    pub fn from_env() -> Self {
        match std::env::var("RCOM_MONITORDEMO").as_deref() {
            Ok("1") => Self::Loopback,
            _ => Self::Off,
        }
    }
}

/// Runs the requested demo (spawn me on the core runtime).
pub async fn run(bus: EventBus, hub: Arc<MonitorHub>) {
    let mode = MonitorDemoMode::from_env();
    if mode == MonitorDemoMode::Off {
        return;
    }
    let outcome = tokio::time::timeout(DEMO_TIMEOUT, loopback(&bus, &hub)).await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => note(&bus, format!("AUTODEMO MONITOR FAILED: {error}"), LogLevel::Error),
        Err(_) => note(&bus, "AUTODEMO MONITOR FAILED: timed out".into(), LogLevel::Error),
    }
}

/// The loopback demo: start the proxy, drive both ends with plain `File` I/O,
/// wait for both capture directions, prove the byte-exact delivery.
async fn loopback(bus: &EventBus, hub: &Arc<MonitorHub>) -> Result<(), String> {
    // Subscribe BEFORE anything starts (the bus is live-only).
    let mut events = bus.subscribe();

    let (id, app_path) = hub
        .start_monitor(MonitorConfig { upstream: Upstream::VirtualLoopback })
        .map_err(|error| format!("start the loopback monitor: {error}"))?;
    let (started_port, started_upstream) = wait_started(&mut events, id).await?;
    note(
        bus,
        format!(
            "AUTODEMO MONITOR: loopback monitor #{id} live — virtual port {app_path} \
             (open it from the target app); upstream {started_upstream}"
        ),
        LogLevel::Info,
    );
    if started_port != app_path {
        return Err(format!("Started event port {started_port} != {app_path}"));
    }
    let Some((_, Some(dev_path))) = hub.virtual_ports(id) else {
        return Err("the loopback monitor has no device-side twin path".into());
    };

    // The demo's "target app" and "device": plain files on the pty slaves.
    let open_path = app_path.clone();
    let app_file = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new().read(true).write(true).open(&open_path)
    })
    .await
    .map_err(|error| format!("join app open: {error}"))?
    .map_err(|error| format!("open the virtual port {app_path}: {error}"))?;
    let open_path = dev_path.clone();
    let dev_file = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new().read(true).write(true).open(&open_path)
    })
    .await
    .map_err(|error| format!("join device open: {error}"))?
    .map_err(|error| format!("open the device-side port {dev_path}: {error}"))?;

    // Direction 1 (app → device): write from the app, wait for the TX capture,
    // read the relayed bytes on the device side.
    let mut writer = app_file.try_clone().map_err(|error| format!("clone app file: {error}"))?;
    tokio::task::spawn_blocking(move || writer.write_all(b"demo-app->device"))
        .await
        .map_err(|error| format!("join app write: {error}"))?
        .map_err(|error| format!("write from the app: {error}"))?;
    let tx = wait_capture(&mut events, id, MonitorDirection::Tx, b"demo-app->device").await?;
    let mut dev_reader = dev_file.try_clone().map_err(|error| format!("clone device file: {error}"))?;
    let delivered = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; b"demo-app->device".len()];
        std::io::Read::read_exact(&mut dev_reader, &mut buf).map(|_| buf)
    })
    .await
    .map_err(|error| format!("join device read: {error}"))?
    .map_err(|error| format!("read on the device side: {error}"))?;
    if delivered != *b"demo-app->device" {
        return Err(format!(
            "the relay garbled the app's bytes: got {delivered:02x?}"
        ));
    }
    note(
        bus,
        format!(
            "AUTODEMO MONITOR: app→device 'demo-app->device' captured as TX at {} and \
             delivered byte-exact to the device side",
            tx.timestamp.format("%H:%M:%S%.3f")
        ),
        LogLevel::Info,
    );

    // Direction 2 (device → app): the device replies, the RX capture proves the
    // reverse relay, the app reads the reply.
    let mut dev_writer =
        dev_file.try_clone().map_err(|error| format!("clone device file: {error}"))?;
    tokio::task::spawn_blocking(move || dev_writer.write_all(b"device->app reply"))
        .await
        .map_err(|error| format!("join device write: {error}"))?
        .map_err(|error| format!("write from the device: {error}"))?;
    let rx = wait_capture(&mut events, id, MonitorDirection::Rx, b"device->app reply").await?;
    let mut app_reader = app_file.try_clone().map_err(|error| format!("clone app file: {error}"))?;
    let replied = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; b"device->app reply".len()];
        std::io::Read::read_exact(&mut app_reader, &mut buf).map(|_| buf)
    })
    .await
    .map_err(|error| format!("join app read: {error}"))?
    .map_err(|error| format!("read on the app side: {error}"))?;
    if replied != *b"device->app reply" {
        return Err(format!("the relay garbled the device's bytes: got {replied:02x?}"));
    }
    if rx.timestamp < tx.timestamp {
        return Err("the RX capture predates the TX capture (stamps are wrong)".into());
    }

    note(
        bus,
        format!(
            "AUTODEMO MONITOR OK: both relay directions captured with direction + timestamps — \
             app→device TX at {} (delivered to the device), device→app RX at {} (delivered to \
             the app); virtual port {app_path} ⇄ {dev_path}; stopping the monitor",
            tx.timestamp.format("%H:%M:%S%.3f"),
            rx.timestamp.format("%H:%M:%S%.3f"),
        ),
        LogLevel::Info,
    );
    drop(app_file);
    drop(dev_file);
    hub.stop_monitor(id);
    Ok(())
}

/// Waits for the monitor's `Started` event and returns its port + upstream.
async fn wait_started(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    monitor_id: u64,
) -> Result<(String, String), String> {
    wait_event(events, |event| match event {
        Event::Monitor(monitor) if monitor.monitor_id == monitor_id => match &monitor.kind {
            MonitorEventKind::Started { virtual_port, upstream } => {
                Some(Ok((virtual_port.clone(), upstream.clone())))
            }
            MonitorEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
}

/// Waits for enough captures of `direction` that their concatenation starts
/// with `prefix` (the tty may split one logical write across reads; the
/// timestamp reported is the event that completed the prefix). `Err` on the
/// monitor's error/failure events or the demo timeout.
async fn wait_capture(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    monitor_id: u64,
    direction: MonitorDirection,
    prefix: &[u8],
) -> Result<crate::core::monitor::MonitorEvent, String> {
    let mut pending: Vec<u8> = Vec::new();
    wait_event(events, |event| match event {
        Event::Monitor(monitor) if monitor.monitor_id == monitor_id => match &monitor.kind {
            MonitorEventKind::Data { direction: dir, bytes } if *dir == direction => {
                pending.extend_from_slice(bytes.as_slice());
                if pending.starts_with(prefix) {
                    Some(Ok(monitor.clone()))
                } else {
                    None
                }
            }
            MonitorEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
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
    tokio::time::timeout(DEMO_TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| Err("the expected monitor event never arrived".to_string()))
}

/// One `##` row on the log view + the trace evidence.
fn note(bus: &EventBus, message: String, level: LogLevel) {
    tracing::info!("{message}");
    bus.publish(Event::Log(LogEvent { level, timestamp: chrono::Local::now(), message }));
}
