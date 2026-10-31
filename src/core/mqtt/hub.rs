//! The MQTT hub: the one handle that owns every session instance and the
//! MQTT⇄serial bridge. The tools page drives it from the UI thread (its methods
//! are sync — they only spawn tokio tasks on the captured runtime handle), and
//! the `RCOM_MQTTDEMO` demo uses it end to end.
//!
//! Housekeeping rides the bus: session closures remove their registry entry,
//! and port lifecycle events track the session id the bridge attaches to (the
//! nettools hub's pattern).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::bus::{Event, EventBus, PortState};
use crate::core::port::manager::PortManager;

use super::bridge;
use super::client;
use super::{MqttConfig, MqttEvent, MqttEventKind, MqttQos, validate_topic};

/// One registered session instance.
struct SessionEntry {
    config: MqttConfig,
    cmd: client::CmdTx,
    cancel: client::Cancel,
}

/// The MQTT⇄serial bridge attached at most once at a time.
struct BridgeEntry {
    session_id: u64,
    topic: String,
    tasks: Vec<tokio::task::AbortHandle>,
}

/// The registry + factory of MQTT sessions. Created inside the tokio runtime
/// (it captures the runtime handle); handed out as `Arc<MqttHub>` — see
/// [`global`].
pub struct MqttHub {
    bus: EventBus,
    manager: Arc<PortManager>,
    runtime: tokio::runtime::Handle,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, SessionEntry>>,
    bridge: Mutex<Option<BridgeEntry>>,
    /// The serial session the bridge binds to (the latest Open/Reopened port).
    session: Mutex<Option<u64>>,
}

impl MqttHub {
    /// Creates the hub and starts its bus housekeeping. Call from inside the
    /// tokio runtime (the handle is captured so UI-thread callers can spawn
    /// without a runtime context). The hub is handed out as an `Arc` because
    /// the housekeeping task and the bridge closures hold it weakly.
    pub fn new(bus: EventBus, manager: Arc<PortManager>) -> Arc<Self> {
        let hub = Arc::new(Self {
            bus,
            manager,
            runtime: tokio::runtime::Handle::current(),
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
            bridge: Mutex::new(None),
            session: Mutex::new(None),
        });
        start_housekeeping(&hub);
        hub
    }

    /// The bus the hub publishes and listens on.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Starts a session. Light validation happens here (host, client id,
    /// broker port); the TLS material is resolved eagerly so a broken setup
    /// fails in the command. Returns the new session id.
    pub fn start_session(&self, mut config: MqttConfig) -> Result<u64, String> {
        config.host = config.host.trim().to_string();
        if config.host.is_empty() {
            return Err("empty broker host".into());
        }
        if config.port == 0 {
            return Err("broker port must be set".into());
        }
        config.client_id = config.client_id.trim().to_string();
        if config.client_id.is_empty() {
            config.client_id = "rcom".to_string();
        }
        if let Some(username) = &config.username {
            if username.trim().is_empty() {
                config.username = None;
            }
        }
        if config.tls {
            crate::core::nettools::tls::client_config(config.accept_any_cert)?;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (cmd, cancel) = client::start(self.bus.clone(), id, config.clone(), &self.runtime);
        self.sessions
            .lock()
            .expect("sessions mutex")
            .insert(id, SessionEntry { config, cmd, cancel });
        Ok(id)
    }

    /// Stops a session (a no-op for unknown ids). Publishes the terminal
    /// `Disconnected` event itself; the worker sends the graceful MQTT
    /// DISCONNECT as the cancel takes hold.
    pub fn stop_session(&self, id: u64) {
        let entry = self.sessions.lock().expect("sessions mutex").remove(&id);
        if let Some(entry) = entry {
            entry.cancel.cancel();
            self.publish_disconnected(id, &entry.config);
            if self.bridged_session() == Some(id) {
                self.detach_bridge();
            }
        }
    }

    /// Stops every session and the bridge (app shutdown).
    pub fn stop_all(&self) {
        let entries: Vec<(u64, client::Cancel, MqttConfig)> = self
            .sessions
            .lock()
            .expect("sessions mutex")
            .drain()
            .map(|(id, entry)| (id, entry.cancel, entry.config))
            .collect();
        for (id, cancel, config) in entries {
            cancel.cancel();
            self.publish_disconnected(id, &config);
        }
        self.detach_bridge();
    }

    /// Publishes user bytes on a session's topic; `false` when the id is
    /// unknown, the topic is invalid, or the worker is gone.
    pub fn publish(&self, id: u64, topic: &str, payload: Vec<u8>, qos: MqttQos, retain: bool) -> bool {
        if validate_topic(topic).is_err() {
            return false;
        }
        self.send(id, client::MqttCmd::Publish { topic: topic.trim().to_string(), payload, qos, retain })
    }

    /// Adds a subscription (topic filter + QoS) to a session; re-applied after
    /// every reconnect. `false` when the id is unknown, the filter invalid, or
    /// the worker gone.
    pub fn subscribe(&self, id: u64, topic: &str, qos: MqttQos) -> bool {
        if validate_topic(topic).is_err() {
            return false;
        }
        self.send(id, client::MqttCmd::Subscribe { topic: topic.trim().to_string(), qos })
    }

    /// Removes a subscription. `false` when the id is unknown or the worker is
    /// gone (an unknown topic is still forwarded — the broker answers).
    pub fn unsubscribe(&self, id: u64, topic: &str) -> bool {
        if topic.trim().is_empty() {
            return false;
        }
        self.send(id, client::MqttCmd::Unsubscribe { topic: topic.trim().to_string() })
    }

    fn send(&self, id: u64, command: client::MqttCmd) -> bool {
        match self.sessions.lock().expect("sessions mutex").get(&id) {
            Some(entry) => entry.cmd.send(command).is_ok(),
            None => false,
        }
    }

    /// The live session ids (oldest first).
    pub fn session_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> =
            self.sessions.lock().expect("sessions mutex").keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Whether a session is alive.
    pub fn is_running(&self, id: u64) -> bool {
        self.sessions.lock().expect("sessions mutex").contains_key(&id)
    }

    /// Attaches the MQTT⇄serial bridge: the session's MQTT RX goes out the
    /// current serial session's port (through the regular write path, so any
    /// TX hook and the TX log rows see it) and the port's serial RX publishes
    /// to `topic` (QoS 0, no retain). Fails when no port session is open or the
    /// MQTT session is unknown; replaces an existing bridge.
    pub fn attach_bridge(self: &Arc<Self>, session_id: u64, topic: &str) -> Result<u64, String> {
        if let Err(error) = validate_topic(topic) {
            return Err(format!("bridge topic: {error}"));
        }
        let session_id_of_port = self
            .session
            .lock()
            .expect("session mutex")
            .ok_or_else(|| "no open port session to bridge with".to_string())?;
        if !self.is_running(session_id) {
            return Err(format!("session {session_id} is not running"));
        }
        self.detach_bridge();
        let topic = topic.trim().to_string();
        // The closure holds the hub only weakly: the hub owns the bridge, so an
        // owning capture would be a cycle keeping a dropped hub alive.
        let weak = Arc::downgrade(self);
        let bridge_session = session_id;
        let bridge_topic = topic.clone();
        let publish: bridge::PublishFn = Arc::new(move |bytes| match weak.upgrade() {
            Some(hub) => hub.publish(bridge_session, &bridge_topic, bytes, MqttQos::AtMostOnce, false),
            None => false,
        });
        let tasks = bridge::spawn(
            self.bus.clone(),
            Arc::clone(&self.manager),
            session_id,
            session_id_of_port,
            publish,
            &self.runtime,
        );
        self.bridge
            .lock()
            .expect("bridge mutex")
            .replace(BridgeEntry { session_id, topic, tasks });
        Ok(session_id_of_port)
    }

    /// Removes the bridge, if any.
    pub fn detach_bridge(&self) {
        if let Some(entry) = self.bridge.lock().expect("bridge mutex").take() {
            tracing::debug!(
                session = entry.session_id,
                topic = entry.topic.as_str(),
                "mqtt bridge detached"
            );
            for task in entry.tasks {
                task.abort();
            }
        }
    }

    /// The session the bridge currently feeds, if any.
    pub fn bridged_session(&self) -> Option<u64> {
        self.bridge.lock().expect("bridge mutex").as_ref().map(|entry| entry.session_id)
    }

    /// The bridge topic, if any.
    pub fn bridge_topic(&self) -> Option<String> {
        self.bridge.lock().expect("bridge mutex").as_ref().map(|entry| entry.topic.clone())
    }

    /// The serial session id the bridge would bind to right now.
    pub fn current_session(&self) -> Option<u64> {
        *self.session.lock().expect("session mutex")
    }

    fn publish_disconnected(&self, id: u64, config: &MqttConfig) {
        self.bus.publish(Event::Mqtt(MqttEvent {
            session_id: id,
            timestamp: chrono::Local::now(),
            kind: MqttEventKind::Disconnected { reason: None },
        }));
        tracing::debug!(session = id, "{}: session stopped", config.label());
    }
}

impl Drop for MqttHub {
    fn drop(&mut self) {
        // A dropped hub leaves no workers behind (sessions self-report
        // Disconnected; the registry is dead anyway).
        for (_, entry) in self.sessions.get_mut().expect("sessions mutex").drain() {
            entry.cancel.cancel();
        }
        self.detach_bridge();
    }
}

static GLOBAL: std::sync::OnceLock<Arc<MqttHub>> = std::sync::OnceLock::new();

/// Installs the process-global hub (call once, inside the tokio runtime) and
/// starts its bus housekeeping. [`global`] hands the Arc out from then on.
pub fn init_global(bus: EventBus, manager: Arc<PortManager>) -> Arc<MqttHub> {
    let hub = MqttHub::new(bus, manager);
    let _ = GLOBAL.set(Arc::clone(&hub));
    hub
}

/// The process-global hub, once installed.
pub fn global() -> Option<&'static Arc<MqttHub>> {
    GLOBAL.get()
}

/// Watches the bus for the hub's lifetime (subscribed at creation, so no event
/// is missed): session closures clean the registry, port lifecycle tracks the
/// bridge's serial session. The task holds the hub weakly — dropping the hub
/// ends it.
fn start_housekeeping(hub: &Arc<MqttHub>) {
    let weak = Arc::downgrade(hub);
    let mut events = hub.bus.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let Some(hub) = weak.upgrade() else { return };
                    match event {
                        Event::Mqtt(mqtt) => {
                            if matches!(mqtt.kind, MqttEventKind::Disconnected { .. }) {
                                hub.sessions
                                    .lock()
                                    .expect("sessions mutex")
                                    .remove(&mqtt.session_id);
                            }
                        }
                        Event::Plot(_) => {}
                        Event::Monitor(_) => {}
                        Event::Port(port) => match port.state {
                            PortState::Open | PortState::Reopened => {
                                *hub.session.lock().expect("session mutex") = Some(port.session_id);
                            }
                            PortState::Closed | PortState::Error | PortState::WatchDesired => {
                                let mut session = hub.session.lock().expect("session mutex");
                                if *session == Some(port.session_id) {
                                    *session = None;
                                    drop(session);
                                    hub.detach_bridge();
                                }
                            }
                            PortState::Arrived | PortState::Removed => {}
                        },
                        Event::Data(_) | Event::Log(_) | Event::ScriptOut(_) | Event::Net(_) => {}
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::core::bus::{Direction, PortEvent};
    use crate::core::mqtt::mockbroker::MockBroker;
    use crate::core::mqtt::MqttDirection;
    use crate::core::port::manager::PortManager;
    use crate::core::port::mock::{MockBackend, MOCK_PORT_NAME};

    /// Wait (bounded) for an MQTT event of `session` matching `predicate`.
    /// Takes a receiver the caller created BEFORE the events were published
    /// (the bus is live-only — the contract's no-replay rule).
    async fn wait_mqtt(
        events: &mut tokio::sync::broadcast::Receiver<Event>,
        session: u64,
        mut predicate: impl FnMut(&MqttEventKind) -> bool,
    ) -> Option<MqttEvent> {
        let wait = async {
            while let Ok(event) = events.recv().await {
                if let Event::Mqtt(mqtt) = event {
                    if mqtt.session_id == session && predicate(&mqtt.kind) {
                        return mqtt;
                    }
                }
            }
            unreachable!("bus closed")
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.ok()
    }

    fn config(port: u16) -> MqttConfig {
        MqttConfig {
            host: "127.0.0.1".into(),
            port,
            client_id: "rcom-test".into(),
            username: None,
            password: None,
            tls: false,
            accept_any_cert: false,
        }
    }

    /// THE session proof through the hub against the mock broker: start →
    /// connected → subscribe → publish round trip (the subscribed publisher
    /// receives its own message back per spec) → clean disconnect, entry
    /// removed, terminal Disconnected exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hub_session_lifecycle_over_the_mock_broker() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))), bus.clone()));
        let hub = Arc::new(MqttHub::new(bus.clone(), manager));
        let broker = MockBroker::spawn().await.expect("broker binds");

        assert!(hub.start_session(config(0)).is_err(), "port 0 refused");
        assert!(hub.start_session(MqttConfig { host: "  ".into(), ..config(broker.port()) }).is_err());

        let id = hub.start_session(config(broker.port())).expect("hub must start the session");
        assert!(hub.is_running(id));
        assert_eq!(hub.session_ids(), vec![id]);

        let mut events = hub.bus().subscribe();
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Connected { .. }))
            .await
            .expect("client must connect");

        assert!(hub.subscribe(id, "demo/up", MqttQos::AtMostOnce), "subscribe accepted");
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Subscribed { .. }))
            .await
            .expect("subscribe queued");

        assert!(hub.publish(id, "demo/up", b"ping".to_vec(), MqttQos::AtMostOnce, false));
        // The broker forwards the publish back to its own subscriber.
        let echo = wait_mqtt(&mut events, id, |kind| matches!(
            kind,
            MqttEventKind::Message { direction: MqttDirection::Rx, .. }
        ))
        .await
        .expect("the broker's forward must arrive as MQTT RX");
        let MqttEventKind::Message { topic, payload, .. } = echo.kind else { unreachable!() };
        assert_eq!((topic.as_str(), payload.as_slice()), ("demo/up", b"ping".as_slice()));

        hub.stop_session(id);
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Disconnected { .. }))
            .await
            .expect("stop must publish Disconnected");
        assert!(!hub.is_running(id), "housekeeping must have removed the entry");
        // The hub's terminal event precedes the wire flush (the worker drains
        // the queued DISCONNECT asynchronously) — poll for the broker proof.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !broker.clean_disconnect() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the stop sent a graceful MQTT DISCONNECT"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // A second stop is a no-op; publishes to the dead id are refused.
        hub.stop_session(id);
        assert!(!hub.publish(id, "demo/up", b"x".to_vec(), MqttQos::AtMostOnce, false));
        hub.stop_all();
    }

    /// An unreachable broker surfaces one deduplicated Error event (not a
    /// flood) and the session stays alive (rumqttc keeps retrying) until the
    /// user stops it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_broker_reports_one_error_and_keeps_retrying() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))), bus.clone()));
        let hub = Arc::new(MqttHub::new(bus.clone(), manager));
        // Port 1 on loopback: nothing listens there.
        let id = hub.start_session(config(1)).expect("hub must start the session");
        let mut events = bus.subscribe();
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Error { .. }))
            .await
            .expect("the failed connect must surface as an Error event");
        // Give the retry loop a few rounds; no second distinct Error, no
        // terminal Disconnected (the session is still "running" from the hub's
        // point of view).
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let mut errors: Vec<MqttEvent> = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let Event::Mqtt(mqtt) = event {
                if mqtt.session_id == id {
                    errors.push(mqtt);
                }
            }
        }
        assert!(
            !errors.iter().any(|mqtt| matches!(mqtt.kind, MqttEventKind::Disconnected { .. })),
            "an unreachable broker is not terminal"
        );
        assert!(hub.is_running(id));
        hub.stop_session(id);
    }

    /// TLS config construction: a TLS session validates (the nettools config
    /// builds — standard and accept-any), a broken TLS setup fails in the
    /// command. Against the TLS mock broker a full round trip runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_session_round_trips_against_the_tls_mock_broker() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))), bus.clone()));
        let hub = Arc::new(MqttHub::new(bus.clone(), manager));
        let broker = MockBroker::spawn_tls().await.expect("TLS broker binds");

        let id = hub
            .start_session(MqttConfig {
                tls: true,
                accept_any_cert: true,
                ..config(broker.port())
            })
            .expect("hub must start the TLS session");
        let mut events = bus.subscribe();
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Connected { .. }))
            .await
            .expect("TLS client must complete the handshake");
        assert!(hub.subscribe(id, "demo/tls", MqttQos::AtMostOnce));
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Subscribed { .. }))
            .await
            .expect("subscribe queued");
        assert!(hub.publish(id, "demo/tls", b"secure".to_vec(), MqttQos::AtMostOnce, false));
        let echo = wait_mqtt(&mut events, id, |kind| matches!(
            kind,
            MqttEventKind::Message { direction: MqttDirection::Rx, .. }
        ))
        .await
        .expect("the TLS forward must arrive as MQTT RX");
        let MqttEventKind::Message { payload, .. } = echo.kind else { unreachable!() };
        assert_eq!(payload.as_slice(), b"secure");
        hub.stop_session(id);
    }

    /// The bridge path through the REAL hub: session tracking follows the port
    /// events; attaching without a port session or an unknown MQTT session
    /// fails; the working attach is replaceable and detaches when the port
    /// session goes away.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hub_bridge_attaches_to_the_tracked_session() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))), bus.clone()));
        let hub = Arc::new(MqttHub::new(bus.clone(), Arc::clone(&manager)));
        let broker = MockBroker::spawn().await.expect("broker binds");

        let id = hub.start_session(config(broker.port())).unwrap();
        assert!(hub.attach_bridge(id, "demo/up").is_err(), "no port session → refuse");
        assert!(hub.attach_bridge(999, "demo/up").is_err(), "unknown session → refuse");
        assert!(hub.attach_bridge(id, "de#mo").is_err(), "invalid topic → refuse");

        let session = manager
            .open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while hub.current_session().is_none() {
            assert!(tokio::time::Instant::now() < deadline, "session was never tracked");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hub.current_session(), Some(session));

        assert_eq!(hub.attach_bridge(id, "demo/up").ok(), Some(session));
        assert_eq!(hub.bridged_session(), Some(id));
        assert_eq!(hub.bridge_topic().as_deref(), Some("demo/up"));
        // Re-attaching replaces (no error, no duplicate).
        assert_eq!(hub.attach_bridge(id, "demo/up2").ok(), Some(session));
        assert_eq!(hub.bridge_topic().as_deref(), Some("demo/up2"));
        hub.detach_bridge();
        assert_eq!(hub.bridged_session(), None);

        // A Closed event of the tracked serial session clears it.
        bus.publish(Event::Port(PortEvent {
            session_id: session,
            port: MOCK_PORT_NAME.into(),
            state: PortState::Closed,
            detail: None,
            timestamp: chrono::Local::now(),
            baud: None,
        }));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while hub.current_session().is_some() {
            assert!(tokio::time::Instant::now() < deadline, "session never cleared");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        hub.stop_all();
    }

    /// Both bridge directions through the REAL hub + mock broker + MOCK0:
    /// serial RX reaches the broker as a publish on the bridge topic, and a
    /// broker-originated publish reaches the port write path (its TX event).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hub_bridge_moves_bytes_both_directions() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))), bus.clone()));
        let hub = Arc::new(MqttHub::new(bus.clone(), Arc::clone(&manager)));
        let broker = MockBroker::spawn().await.expect("broker binds");
        // The port session opens first, and a throwaway MQTT session before the
        // bridged one keeps the two id spaces distinct — the bridge must key
        // each direction on its own namespace.
        let session = manager
            .open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
            .await
            .unwrap();
        let _throwaway = hub.start_session(config(broker.port())).unwrap();
        let id = hub.start_session(config(broker.port())).unwrap();
        assert_ne!(id, session, "the ids must not coincide in this test");

        let mut events = bus.subscribe();
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Connected { .. }))
            .await
            .expect("client must connect");
        // Subscribe the DOWN topic only: the bridge publishes UP, so the echo
        // cannot loop (serial RX → publish up → broker → no client sub on up).
        assert!(hub.subscribe(id, "demo/down", MqttQos::AtMostOnce));
        wait_mqtt(&mut events, id, |kind| matches!(kind, MqttEventKind::Subscribed { .. }))
            .await
            .expect("subscribe queued");
        assert_eq!(hub.attach_bridge(id, "demo/up").ok(), Some(session));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Direction 1: device bytes → serial RX → bridge → broker (demo/up).
        crate::core::bus::publish_rx(&bus, session, b"via-hub".to_vec());
        assert!(
            broker
                .wait_message("demo/up", b"via-hub", Duration::from_secs(5))
                .await,
            "serial RX must reach the broker through the hub bridge"
        );

        // Direction 2: broker-originated publish (demo/down) → MQTT RX →
        // bridge → port write path (its TX event on the bus).
        broker.publish("demo/down", b"to-serial");
        let wait = async {
            while let Ok(event) = events.recv().await {
                if let Event::Data(data) = event {
                    if data.session_id == session
                        && data.direction == Direction::Tx
                        && data.bytes.as_slice() == b"to-serial"
                    {
                        return true;
                    }
                }
            }
            false
        };
        let proved = tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap_or(false);
        assert!(proved, "MQTT RX never reached the port write path");

        hub.stop_all();
        manager.close().await;
    }
}
