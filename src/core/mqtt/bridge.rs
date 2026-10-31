//! The MQTT⇄serial bridge: one direction each way, both driven off the bus
//! (the net⇄serial bridge's sibling, issue #21's AC "Topic ⇄ serial bridge,
//! optional per connection").
//!
//! - serial RX (`Event::Data`, `Direction::Rx`, the bridged session) → an MQTT
//!   publish to the bridge's configured topic through the [`PublishFn`]
//!   closure, which the hub wires to the session (QoS 0, no retain — the
//!   bridged stream is live traffic, not state);
//! - MQTT RX (`Event::Mqtt`, `Message { direction: Rx }`, the bridged session)
//!   → the port write path ([`PortManager::write`]) — so broker-side bytes are
//!   indistinguishable from any other TX: they pass the Lua pre-send hook and
//!   show up as the session's TX rows automatically (contract §1.5: "bridged
//!   data ALSO appears as session Rx/Tx").
//!
//! The bridge binds to the session that was open when it was attached and
//! stops forwarding when either side goes away (the hub's housekeeping detaches
//! on the port's terminal events and on the MQTT session's Disconnected).

use std::sync::Arc;

use crate::core::bus::{Direction, Event, EventBus};
use crate::core::port::manager::PortManager;

use super::MqttEventKind;

/// Publishes serial bytes to the bridge topic; `false` when the session is gone.
pub(crate) type PublishFn = Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>;

/// Spawns the two forwarder tasks (through `runtime` — the hub attaches the
/// bridge from the UI thread); their abort handles end them on detach. The two
/// session ids are distinct namespaces: `mqtt_session` selects the MQTT
/// session's RX messages, `serial_session` the serial session's RX data.
pub(crate) fn spawn(
    bus: EventBus,
    manager: Arc<PortManager>,
    mqtt_session: u64,
    serial_session: u64,
    publish: PublishFn,
    runtime: &tokio::runtime::Handle,
) -> Vec<tokio::task::AbortHandle> {
    // Serial RX → MQTT publish.
    let serial_to_mqtt = {
        let publish = Arc::clone(&publish);
        let bus = bus.clone();
        runtime.spawn(async move {
            let mut events = bus.subscribe();
            while let Ok(event) = events.recv().await {
                if let Event::Data(data) = event {
                    if data.direction == Direction::Rx
                        && data.session_id == serial_session
                        && !publish(data.bytes.as_slice().to_vec())
                    {
                        return; // session gone
                    }
                }
            }
        })
    };

    // MQTT RX → serial TX (the session write path).
    let mqtt_to_serial = {
        runtime.spawn(async move {
            let mut events = bus.subscribe();
            while let Ok(event) = events.recv().await {
                if let Event::Mqtt(mqtt) = event {
                    if mqtt.session_id == mqtt_session {
                        if let MqttEventKind::Message {
                            direction: super::MqttDirection::Rx,
                            payload,
                            ..
                        } = mqtt.kind
                        {
                            if manager.write(payload.as_slice().to_vec()).await.is_err() {
                                return; // session gone
                            }
                        }
                    }
                }
            }
        })
    };

    vec![serial_to_mqtt.abort_handle(), mqtt_to_serial.abort_handle()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::bus::{publish_rx, Payload};
    use crate::core::mqtt::{MqttDirection, MqttEvent, MqttQos};
    use crate::core::port::manager::PortManager;
    use crate::core::port::mock::{MockBackend, MOCK_PORT_NAME};

    /// THE bridge proof over a real open session (MOCK0): serial RX of the
    /// bridged session is pushed into the publish closure, and MQTT RX of the
    /// bridged session goes out through `PortManager::write` — visible as the
    /// session's TX event. Other sessions' data and other sessions' MQTT
    /// traffic stay out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_moves_bytes_both_directions_over_a_real_session() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))), bus.clone()));
        let session = manager
            .open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
            .await
            .expect("MOCK0 must open");

        /// What the bridge "publishes" (the test fakes the MQTT side): topic +
        /// payload pairs.
        type Published = Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;
        let reached_mqtt: Published = Arc::new(std::sync::Mutex::new(Vec::new()));
        const TOPIC: &str = "demo/up";
        let publish: PublishFn = {
            let reached_mqtt = Arc::clone(&reached_mqtt);
            Arc::new(move |bytes: Vec<u8>| {
                reached_mqtt
                    .lock()
                    .unwrap()
                    .push((TOPIC.to_string(), bytes));
                true
            })
        };

        const MQTT_SESSION: u64 = 9;
        // The mock port greets ("MOCK0 ready…") — that RX must flush through
        // the bus BEFORE the bridge subscribes, or it would be bridged too.
        // Subscribe before anything publishes (live bus), and give the bridge
        // tasks a beat to make their own subscriptions. The two session ids are
        // deliberately distinct (the port session is 1) — the test proves the
        // serial side keys on the serial session, the MQTT side on the MQTT one.
        let mut events = bus.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match events.try_recv() {
                Ok(Event::Data(data))
                    if data.session_id == session && data.direction == Direction::Rx =>
                {
                    break; // RX is flowing — the port's side is settled now
                }
                Ok(Event::Log(log))
                    if log.session_id == Some(session) =>
                {
                    break; // the mock's greeting note — the port's side is live
                }
                _ => {}
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the mock port's greeting never arrived"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let tasks = spawn(
            bus.clone(),
            Arc::clone(&manager),
            MQTT_SESSION,
            session,
            publish,
            &tokio::runtime::Handle::current(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Direction 1 (serial RX → MQTT): a device chunk of the bridged
        // session; a neighbour session's chunk must stay out.
        publish_rx(&bus, session, b"from-device".to_vec());
        publish_rx(&bus, session + 100, b"other-session".to_vec());

        // Direction 2 (MQTT → serial TX): an MQTT RX of the bridged session; a
        // neighbour session's message and local TX publishes stay out.
        for (session, direction, bytes) in [
            (MQTT_SESSION, MqttDirection::Rx, b"from-mqtt".to_vec()),
            (MQTT_SESSION, MqttDirection::Tx, b"local-pub".to_vec()),
            (MQTT_SESSION + 1, MqttDirection::Rx, b"other-session".to_vec()),
        ] {
            bus.publish(Event::Mqtt(MqttEvent {
                session_id: session,
                timestamp: chrono::Local::now(),
                kind: MqttEventKind::Message {
                    direction,
                    topic: "demo/down".into(),
                    payload: Payload::from_vec(bytes),
                    qos: MqttQos::AtMostOnce,
                    retain: false,
                },
            }));
        }

        // Direction 1 completes asynchronously; poll for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = reached_mqtt
                .lock()
                .unwrap()
                .iter()
                .any(|(_, bytes)| bytes == b"from-device");
            if got || tokio::time::Instant::now() > deadline {
                assert!(got, "serial RX never reached the MQTT side");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let published = reached_mqtt.lock().unwrap().clone();
        assert_eq!(published.len(), 1, "only the bridged session's RX is published");
        assert_eq!(published[0].0, TOPIC, "the publish carries the bridge topic");

        // Direction 2: the MQTT chunk went out the port — its TX event lands on
        // the bus (the mock echoes it back; the TX of `from-mqtt` is the proof).
        let wait = async {
            while let Ok(event) = events.recv().await {
                if let Event::Data(data) = event {
                    if data.session_id == session
                        && data.direction == Direction::Tx
                        && data.bytes.as_slice() == b"from-mqtt"
                    {
                        return true;
                    }
                }
            }
            false
        };
        let proved = tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap_or(false);
        assert!(proved, "MQTT RX never reached the port write path");

        for task in &tasks {
            task.abort();
        }
        manager.close().await;
    }
}
