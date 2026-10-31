//! The `RCOM_MQTTDEMO` autodemo: the MQTT tool proving itself against the
//! in-process mock broker ([`super::mockbroker`]) on loopback, end to end
//! through the real hub, with the evidence in the trace log and as `##` rows in
//! the session log view. No external broker is ever required.
//!
//! - `RCOM_MQTTDEMO=1` — plain-TCP session ↔ mock broker: CONNECT, subscribe,
//!   publish round trip (`ping` comes back through the broker), broker-side
//!   push, clean DISCONNECT.
//! - `RCOM_MQTTDEMO=tls` — the same round trip over TLS (the nettools
//!   self-signed certificate on the broker, the accept-any toggle on the
//!   client).
//! - `RCOM_MQTTDEMO=bridge` — the MQTT⇄serial bridge on `MOCK0` both ways:
//!   serial RX publishes to the broker (up topic), a broker push is written to
//!   the port (down topic). Topics are split so the mock's serial echo cannot
//!   loop the bridge.
//!
//! Everything runs on localhost; no external network anywhere.

use std::sync::Arc;
use std::time::Duration;

use crate::core::bus::{Direction, Event, EventBus, LogEvent, LogLevel};
use crate::core::port::manager::PortManager;
use crate::core::port::mock::MOCK_PORT_NAME;

use super::hub::MqttHub;
use super::mockbroker::MockBroker;
use super::{MqttConfig, MqttDirection, MqttEventKind, MqttQos};

/// How long any demo phase may take before the run is declared failed.
const DEMO_TIMEOUT: Duration = Duration::from_secs(20);

/// The demo mode requested by `RCOM_MQTTDEMO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttDemoMode {
    Off,
    Basic,
    Tls,
    Bridge,
}

impl MqttDemoMode {
    pub fn from_env() -> Self {
        match std::env::var("RCOM_MQTTDEMO").as_deref() {
            Ok("1") => Self::Basic,
            Ok("tls") => Self::Tls,
            Ok("bridge") => Self::Bridge,
            _ => Self::Off,
        }
    }
}

/// Runs the requested demo (spawn me on the core runtime). `manager` is needed
/// only by the bridge mode; other modes run without a port.
pub async fn run(bus: EventBus, hub: Arc<MqttHub>, manager: Option<Arc<PortManager>>) {
    let mode = MqttDemoMode::from_env();
    if mode == MqttDemoMode::Off {
        return;
    }
    let outcome = tokio::time::timeout(DEMO_TIMEOUT, async {
        match mode {
            MqttDemoMode::Off => Ok(()),
            MqttDemoMode::Basic => basic_loopback(&bus, &hub, false).await,
            MqttDemoMode::Tls => basic_loopback(&bus, &hub, true).await,
            MqttDemoMode::Bridge => match manager {
                Some(manager) => bridge_loopback(&bus, &hub, &manager).await,
                None => Err("bridge demo needs the port manager".to_string()),
            },
        }
    })
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => note(&bus, format!("AUTODEMO MQTT FAILED: {error}"), LogLevel::Error),
        Err(_) => note(&bus, "AUTODEMO MQTT FAILED: timed out".into(), LogLevel::Error),
    }
}

/// One note row into the session log (the `##`/`!!` lines).
fn note(bus: &EventBus, text: String, level: LogLevel) {
    bus.publish(Event::Log(LogEvent { level, timestamp: chrono::Local::now(), message: text }));
}

fn demo_config(port: u16, tls: bool) -> MqttConfig {
    MqttConfig {
        host: "127.0.0.1".into(),
        port,
        client_id: "rcom-demo".into(),
        username: None,
        password: None,
        tls,
        accept_any_cert: tls,
    }
}

/// The basic and TLS demos: a mock broker plus a hub session, subscribe,
/// publish round trip, broker push, clean disconnect.
async fn basic_loopback(bus: &EventBus, hub: &MqttHub, tls: bool) -> Result<(), String> {
    let broker = if tls {
        MockBroker::spawn_tls().await.map_err(|error| format!("mock broker (TLS): {error}"))?
    } else {
        MockBroker::spawn().await.map_err(|error| format!("mock broker: {error}"))?
    };
    note(
        bus,
        format!(
            "AUTODEMO MQTT: session → mock broker at {} ({})",
            broker.addr(),
            if tls { "TLS, self-signed broker cert, accept-any client" } else { "plain TCP" }
        ),
        LogLevel::Info,
    );

    let session = hub.start_session(demo_config(broker.port(), tls))?;
    let mut events = bus.subscribe();
    wait_connected(&mut events, session).await?;

    hub.subscribe(session, "demo/up", MqttQos::AtLeastOnce);
    wait_subscribed(&mut events, session).await?;
    hub.publish(session, "demo/up", b"ping".to_vec(), MqttQos::AtLeastOnce, false);
    wait_message(&mut events, session, MqttDirection::Rx, b"ping").await?;
    note(
        bus,
        format!(
            "AUTODEMO MQTT: 'ping' travelled client → broker → client on demo/up \
             (subscribe + publish round trip over {})",
            if tls { "TLS" } else { "TCP" }
        ),
        LogLevel::Info,
    );

    // The broker's own push lands as a client RX too.
    broker.publish("demo/up", b"push");
    wait_message(&mut events, session, MqttDirection::Rx, b"push").await?;

    hub.stop_session(session);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !broker.clean_disconnect() {
        if tokio::time::Instant::now() > deadline {
            return Err("the stop never produced a graceful MQTT DISCONNECT".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    note(
        bus,
        format!(
            "AUTODEMO MQTT OK: clean disconnect confirmed by the broker \
             (session #{session} ↔ mock broker at {} — MQTT connect/subscribe/publish/\
             disconnect all proven{})",
            broker.addr(),
            if tls { " over TLS" } else { "" }
        ),
        LogLevel::Info,
    );
    Ok(())
}

/// The bridge demo: mock broker + `MOCK0` + the MQTT⇄serial bridge, both
/// directions. Down (broker→serial) and up (serial→broker) are different
/// topics, so the mock port's echo cannot loop the bridge.
async fn bridge_loopback(
    bus: &EventBus,
    hub: &Arc<MqttHub>,
    manager: &Arc<PortManager>,
) -> Result<(), String> {
    const UP: &str = "demo/serial"; // serial RX → broker
    const DOWN: &str = "demo/cmd"; // broker → serial

    let broker = MockBroker::spawn().await.map_err(|error| format!("mock broker: {error}"))?;
    let session = manager
        .open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
        .await
        .map_err(|error| format!("open {MOCK_PORT_NAME}: {error}"))?;
    let mqtt = hub.start_session(demo_config(broker.port(), false))?;
    let mut events = bus.subscribe();
    wait_connected(&mut events, mqtt).await?;
    hub.subscribe(mqtt, DOWN, MqttQos::AtLeastOnce);
    wait_subscribed(&mut events, mqtt).await?;

    let bridged = hub
        .attach_bridge(mqtt, UP)
        .map_err(|error| format!("attach bridge: {error}"))?;
    note(
        bus,
        format!(
            "AUTODEMO MQTT: bridge attached — session #{mqtt} ⇄ serial session {bridged} \
             ({MOCK_PORT_NAME}); up={UP}, down={DOWN}"
        ),
        LogLevel::Info,
    );

    // Direction 1 (serial → MQTT): a byte written to the port echoes back as
    // serial RX, the bridge publishes it to UP, the broker records it.
    manager.write(b"via-serial".to_vec()).await.map_err(|error| format!("serial write: {error}"))?;
    if !broker.wait_message(UP, b"via-serial", Duration::from_secs(5)).await {
        return Err("serial RX never reached the broker".into());
    }
    note(
        bus,
        format!("AUTODEMO MQTT: 'via-serial' travelled port RX → bridge → broker on {UP}"),
        LogLevel::Info,
    );

    // Direction 2 (MQTT → serial): the broker pushes on DOWN, the bridge writes
    // it out the port — its TX event is the proof.
    broker.publish(DOWN, b"via-mqtt");
    wait_port_tx(bus, session, b"via-mqtt").await?;
    note(
        bus,
        format!(
            "AUTODEMO MQTT OK: bridge proven both directions — broker '{DOWN}' push reached the \
             port as TX 'via-mqtt', and port RX reached the broker as '{UP}' publish; detaching"
        ),
        LogLevel::Info,
    );
    hub.detach_bridge();
    hub.stop_session(mqtt);
    manager.close().await;
    Ok(())
}

// ---- bus wait helpers ------------------------------------------------------

/// Waits for the session's `Connected` (or an `Error`, which fails the demo).
async fn wait_connected(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    session: u64,
) -> Result<(), String> {
    wait_event(events, |event| match event {
        Event::Mqtt(mqtt) if mqtt.session_id == session => match &mqtt.kind {
            MqttEventKind::Connected { server } => Some(Ok(format!("{server} connected"))),
            MqttEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
    .map(|_| ())
}

/// Waits for the session's `Subscribed` of the demo topic.
async fn wait_subscribed(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    session: u64,
) -> Result<(), String> {
    wait_event(events, |event| match event {
        Event::Mqtt(mqtt) if mqtt.session_id == session => match &mqtt.kind {
            MqttEventKind::Subscribed { .. } => Some(Ok(())),
            MqttEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
    .map(|_| ())
}

/// Waits for a message of the session in `direction` with exactly `payload`.
async fn wait_message(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    session: u64,
    direction: MqttDirection,
    expected: &[u8],
) -> Result<(), String> {
    wait_event(events, |event| match event {
        Event::Mqtt(mqtt) if mqtt.session_id == session => match &mqtt.kind {
            MqttEventKind::Message { direction: dir, payload, .. }
                if *dir == direction && payload.as_slice() == expected =>
            {
                Some(Ok(()))
            }
            MqttEventKind::Error { message } => Some(Err(message.clone())),
            _ => None,
        },
        _ => None,
    })
    .await
    .map(|_| ())
}

/// Waits for a TX chunk on the port session (the MQTT→serial proof).
async fn wait_port_tx(bus: &EventBus, session: u64, expected: &[u8]) -> Result<(), String> {
    let mut events = bus.subscribe();
    wait_event(&mut events, |event| match event {
        Event::Data(data)
            if data.session_id == session
                && data.direction == Direction::Tx
                && data.bytes.as_slice() == expected =>
        {
            Some(Ok(()))
        }
        _ => None,
    })
    .await
    .map(|_| ())
}

/// Shared bounded wait: `predicate` decides; `Some(Ok/Err)` ends the wait.
async fn wait_event<T>(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    mut predicate: impl FnMut(&Event) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let wait = async {
        while let Ok(event) = events.recv().await {
            if let Some(outcome) = predicate(&event) {
                return outcome;
            }
        }
        Err("bus closed".into())
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| Err("timed out waiting for the demo step".into()))
}
