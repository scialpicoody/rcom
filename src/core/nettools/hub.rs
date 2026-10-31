//! The net hub: the one handle that owns every channel instance and the
//! net⇄serial bridge. The tools page drives it from the UI thread (its methods
//! are sync — they only spawn tokio tasks on the captured runtime handle), the
//! Lua `rcom.net` API resolves it globally, and the `RCOM_NETDEMO` demo uses it
//! end to end.
//!
//! Housekeeping rides the bus: channel closures remove their registry entry,
//! and port lifecycle events track the session id the bridge attaches to.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::bus::{Event, EventBus, PortState};
use crate::core::port::manager::PortManager;

use super::bridge;
use super::channel;
use super::{ChannelConfig, NetEvent, NetEventKind, NetMode, NetProtocol};

/// One registered channel instance.
struct ChannelEntry {
    config: ChannelConfig,
    cmd: channel::CmdTx,
    cancel: channel::Cancel,
}

/// The net⇄serial bridge attached at most once at a time.
struct BridgeEntry {
    channel_id: u64,
    session_id: u64,
    tasks: Vec<tokio::task::AbortHandle>,
}

/// The registry + factory of net channels. Created inside the tokio runtime
/// (it captures the runtime handle); handed out as `Arc<NetHub>` — see
/// [`global`].
pub struct NetHub {
    bus: EventBus,
    manager: Arc<PortManager>,
    runtime: tokio::runtime::Handle,
    next_id: AtomicU64,
    channels: Mutex<HashMap<u64, ChannelEntry>>,
    bridge: Mutex<Option<BridgeEntry>>,
    /// The session the bridge binds to (the latest Open/Reopened port).
    session: Mutex<Option<u64>>,
}

impl NetHub {
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
            channels: Mutex::new(HashMap::new()),
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

    /// Starts a channel. Only light validation happens here (the user is
    /// typing; DNS resolution belongs to the worker), but TLS material is
    /// prepared eagerly so a broken setup fails in the command, not only on
    /// the bus. Returns the new channel id.
    pub fn start_channel(&self, config: ChannelConfig) -> Result<u64, String> {
        if config.host.trim().is_empty() {
            return Err("empty host".into());
        }
        if config.protocol == NetProtocol::Tls {
            if config.mode == NetMode::Server {
                super::tls::server_acceptor()?;
            } else {
                super::tls::client_connector(config.accept_any_cert)?;
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (cmd, cancel) = channel::start(self.bus.clone(), id, config.clone(), &self.runtime);
        self.channels.lock().expect("channels mutex").insert(id, ChannelEntry { config, cmd, cancel });
        Ok(id)
    }

    /// Stops a channel (a no-op for unknown ids). Publishes the terminal
    /// `Disconnected` event itself: cancel is graceful; abort (inside the
    /// worker paths) is only the fallback for a wedged worker.
    pub fn stop_channel(&self, id: u64) {
        let entry = self.channels.lock().expect("channels mutex").remove(&id);
        if let Some(entry) = entry {
            entry.cancel.cancel();
            self.publish_disconnected(id, &entry.config);
        }
    }

    /// Stops every channel and the bridge (app shutdown).
    pub fn stop_all(&self) {
        let entries: Vec<(u64, channel::Cancel, ChannelConfig)> = self
            .channels
            .lock()
            .expect("channels mutex")
            .drain()
            .map(|(id, entry)| (id, entry.cancel, entry.config))
            .collect();
        for (id, cancel, config) in entries {
            cancel.cancel();
            self.publish_disconnected(id, &config);
        }
        self.detach_bridge();
    }

    /// Sends user bytes into a channel; `false` when the id is unknown or the
    /// worker is gone.
    pub fn send(&self, id: u64, bytes: Vec<u8>) -> bool {
        match self.channels.lock().expect("channels mutex").get(&id) {
            Some(entry) => entry.cmd.send(bytes).is_ok(),
            None => false,
        }
    }

    /// The live channel ids (oldest first).
    pub fn channel_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> =
            self.channels.lock().expect("channels mutex").keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Whether a channel is alive.
    pub fn is_running(&self, id: u64) -> bool {
        self.channels.lock().expect("channels mutex").contains_key(&id)
    }

    /// Attaches the net⇄serial bridge: the channel's net RX goes out the
    /// current session's port (through the regular write path, so any TX hook
    /// and the TX log row see it) and the session's serial RX goes into the
    /// channel. Fails when no port session is open or the channel is unknown;
    /// replaces an existing bridge.
    pub fn attach_bridge(self: &Arc<Self>, channel_id: u64) -> Result<u64, String> {
        let session_id = self
            .session
            .lock()
            .expect("session mutex")
            .ok_or_else(|| "no open port session to bridge with".to_string())?;
        if !self.is_running(channel_id) {
            return Err(format!("channel {channel_id} is not running"));
        }
        self.detach_bridge();
        // The closure holds the hub only weakly: the hub owns the bridge, so an
        // owning capture would be a cycle keeping a dropped hub alive.
        let weak = Arc::downgrade(self);
        let send: bridge::SendFn = Arc::new(move |bytes| match weak.upgrade() {
            Some(hub) => hub.send(channel_id, bytes),
            None => false,
        });
        let tasks = bridge::spawn(
            self.bus.clone(),
            Arc::clone(&self.manager),
            channel_id,
            session_id,
            send,
            &self.runtime,
        );
        self.bridge
            .lock()
            .expect("bridge mutex")
            .replace(BridgeEntry { channel_id, session_id, tasks });
        Ok(session_id)
    }

    /// Removes the bridge, if any.
    pub fn detach_bridge(&self) {
        if let Some(entry) = self.bridge.lock().expect("bridge mutex").take() {
            tracing::debug!(
                channel = entry.channel_id,
                session = entry.session_id,
                "net bridge detached"
            );
            for task in entry.tasks {
                task.abort();
            }
        }
    }

    /// The channel the bridge currently feeds, if any.
    pub fn bridged_channel(&self) -> Option<u64> {
        self.bridge.lock().expect("bridge mutex").as_ref().map(|entry| entry.channel_id)
    }

    /// The session id the bridge would bind to right now.
    pub fn current_session(&self) -> Option<u64> {
        *self.session.lock().expect("session mutex")
    }

    fn publish_disconnected(&self, id: u64, config: &ChannelConfig) {
        self.bus.publish(Event::Net(NetEvent {
            channel_id: id,
            protocol: config.protocol,
            mode: config.mode,
            timestamp: chrono::Local::now(),
            kind: NetEventKind::Disconnected { reason: None },
        }));
    }
}

impl Drop for NetHub {
    fn drop(&mut self) {
        // A dropped hub leaves no workers behind (channels self-report
        // Disconnected; the registry is dead anyway).
        for (_, entry) in self.channels.get_mut().expect("channels mutex").drain() {
            entry.cancel.cancel();
        }
        self.detach_bridge();
    }
}

static GLOBAL: std::sync::OnceLock<Arc<NetHub>> = std::sync::OnceLock::new();

/// Installs the process-global hub (call once, inside the tokio runtime) and
/// starts its bus housekeeping. [`global`] hands the Arc out from then on.
pub fn init_global(bus: EventBus, manager: Arc<PortManager>) -> Arc<NetHub> {
    let hub = NetHub::new(bus, manager);
    let _ = GLOBAL.set(Arc::clone(&hub));
    hub
}

/// The process-global hub, once installed.
pub fn global() -> Option<&'static Arc<NetHub>> {
    GLOBAL.get()
}

/// Watches the bus for the hub's lifetime (subscribed at creation, so no event
/// is missed): channel closures clean the registry, port lifecycle tracks the
/// bridge's session. The task holds the hub weakly — dropping the hub ends it.
fn start_housekeeping(hub: &Arc<NetHub>) {
    let weak = Arc::downgrade(hub);
    let mut events = hub.bus.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let Some(hub) = weak.upgrade() else { return };
                    match event {
                        Event::Net(net) => {
                            if matches!(net.kind, NetEventKind::Disconnected { .. }) {
                                hub.channels.lock().expect("channels mutex").remove(&net.channel_id);
                            }
                        }
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
                        Event::Data(_) | Event::Log(_) | Event::ScriptOut(_)
                        | Event::Mqtt(_) | Event::Plot(_) | Event::Monitor(_) => {}
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
    use crate::core::nettools::{NetDirection, NetEventKind, NetMode, NetProtocol};
    use crate::core::port::manager::PortManager;
    use crate::core::port::mock::{MockBackend, MOCK_PORT_NAME};

    use super::super::channel;

    /// Wait (bounded) for a net event of `channel` matching `predicate`. Takes
    /// a receiver the caller created BEFORE the events were published (the bus
    /// is live-only — the contract's no-replay rule).
    async fn wait_net(
        events: &mut tokio::sync::broadcast::Receiver<Event>,
        channel: u64,
        mut predicate: impl FnMut(&NetEventKind) -> bool,
    ) -> Option<NetEvent> {
        let wait = async {
            while let Ok(event) = events.recv().await {
                if let Event::Net(net) = event {
                    if net.channel_id == channel && predicate(&net.kind) {
                        return net;
                    }
                }
            }
            unreachable!("bus closed")
        };
        tokio::time::timeout(Duration::from_secs(5), wait).await.ok()
    }

    /// Subscribes the test's event stream before anything publishes.
    fn stream(bus: &EventBus) -> tokio::sync::broadcast::Receiver<Event> {
        bus.subscribe()
    }

    /// The hub end to end: start → send → echo received → stop removes the
    /// entry and publishes the terminal Disconnected exactly once (the hub's,
    /// since the supervisor defers to the stopper).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hub_channel_lifecycle() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::new()), bus.clone()));
        let hub = Arc::new(NetHub::new(bus.clone(), manager));
        let addr = channel::spawn_tcp_echo("127.0.0.1:0").await.unwrap();

        let id = hub
            .start_channel(super::super::ChannelConfig {
                protocol: NetProtocol::Tcp,
                mode: NetMode::Client,
                host: "127.0.0.1".into(),
                port: addr.port(),
                accept_any_cert: false,
            })
            .expect("hub must start the channel");
        assert!(hub.is_running(id));
        assert_eq!(hub.channel_ids(), vec![id]);

        let mut events = stream(&bus);
        wait_net(&mut events, id, |kind| matches!(kind, NetEventKind::Connected { .. })).await
            .expect("client must connect");
        assert!(hub.send(id, b"ping".to_vec()), "send must be accepted");
        let echo = wait_net(&mut events, id, |kind| matches!(
            kind,
            NetEventKind::Data { direction: NetDirection::Rx, .. }
        ))
        .await
        .expect("echo must arrive");
        let NetEventKind::Data { bytes, .. } = echo.kind else { unreachable!() };
        assert_eq!(bytes.as_slice(), b"ping");

        hub.stop_channel(id);
        let closed = wait_net(&mut events, id, |kind| matches!(kind, NetEventKind::Disconnected { .. }))
            .await
            .expect("stop must publish Disconnected");
        assert_eq!(closed.channel_id, id);
        assert!(!hub.is_running(id), "housekeeping must have removed the entry");

        // A second stop is a no-op; sends to the dead id are refused.
        hub.stop_channel(id);
        assert!(!hub.send(id, b"x".to_vec()));
    }

    /// The bridge path through the hub: session tracking follows the port
    /// events; attaching without a channel or without a session fails; the
    /// working attach returns the session and is replaceable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hub_bridge_attaches_to_the_tracked_session() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::new()), bus.clone()));
        let hub = Arc::new(NetHub::new(bus.clone(), Arc::clone(&manager)));
        let addr = channel::spawn_tcp_echo("127.0.0.1:0").await.unwrap();

        // No session yet: the bridge refuses.
        let id = hub
            .start_channel(super::super::ChannelConfig {
                protocol: NetProtocol::Tcp,
                mode: NetMode::Client,
                host: "127.0.0.1".into(),
                port: addr.port(),
                accept_any_cert: false,
            })
            .unwrap();
        assert!(hub.attach_bridge(id).is_err(), "no session → refuse");
        assert!(hub.attach_bridge(999).is_err(), "unknown channel → refuse");

        // The manager's open publishes Port Open; housekeeping tracks it.
        let session = manager.open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while hub.current_session().is_none() {
            assert!(tokio::time::Instant::now() < deadline, "session was never tracked");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hub.current_session(), Some(session));

        // Attaching needs the channel connected? No — running suffices.
        assert_eq!(hub.attach_bridge(id).ok(), Some(session));
        assert_eq!(hub.bridged_channel(), Some(id));
        // Re-attaching replaces (no error, no duplicate).
        assert_eq!(hub.attach_bridge(id).ok(), Some(session));
        hub.detach_bridge();
        assert_eq!(hub.bridged_channel(), None);

        // A Closed event of the tracked session clears it.
        bus.publish(Event::Port(PortEvent {
            session_id: session,
            port: MOCK_PORT_NAME.into(),
            state: crate::core::bus::PortState::Closed,
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

    /// The serial→net direction through the REAL hub bridge: a device chunk on
    /// the session's bus reaches the channel and goes out over TCP — visible
    /// as the channel's net TX event (the echo server's reply comes back as RX).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hub_bridge_forwards_serial_rx_to_the_channel() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(Arc::new(MockBackend::new()), bus.clone()));
        let hub = Arc::new(NetHub::new(bus.clone(), Arc::clone(&manager)));
        let addr = channel::spawn_tcp_echo("127.0.0.1:0").await.unwrap();
        let id = hub
            .start_channel(super::super::ChannelConfig {
                protocol: NetProtocol::Tcp,
                mode: NetMode::Client,
                host: "127.0.0.1".into(),
                port: addr.port(),
                accept_any_cert: false,
            })
            .unwrap();
        let session = manager.open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
            .await
            .unwrap();
        let mut events = stream(&bus);
        wait_net(&mut events, id, |kind| matches!(kind, NetEventKind::Connected { .. })).await
            .expect("client must connect");
        let bridged = hub.attach_bridge(id).expect("bridge must attach");
        assert_eq!(bridged, session);
        // Give the bridge tasks a beat to subscribe (they ride the same live
        // bus; the test then publishes with both subscriptions in place).
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Device bytes on the session's bus → bridge → channel → TCP out.
        crate::core::bus::publish_data(&bus, session, Direction::Rx, b"via-hub".to_vec());
        let proof = wait_net(&mut events, id, |kind| matches!(
            kind,
            NetEventKind::Data { direction: NetDirection::Tx, bytes, .. } if bytes.as_slice() == b"via-hub"
        ))
        .await;
        assert!(proof.is_some(), "serial RX must reach the channel through the hub bridge");

        // …and the echo comes back as net RX (the reverse direction's input).
        let back = wait_net(&mut events, id, |kind| matches!(
            kind,
            NetEventKind::Data { direction: NetDirection::Rx, bytes, .. } if bytes.as_slice() == b"via-hub"
        ))
        .await;
        assert!(back.is_some(), "the echo of the bridged bytes must come back as net RX");
        hub.stop_all();
    }
}
