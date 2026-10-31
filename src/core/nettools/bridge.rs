//! The net⇄serial bridge: one direction each way, both driven off the bus.
//!
//! - serial RX (`Event::Data`, `Direction::Rx`, the bridged session) → the net
//!   channel (the [`SendFn`] closure, wired by the hub);
//! - net RX (`Event::Net`, `Data { direction: Rx }`, the bridged channel) →
//!   the port write path ([`PortManager::write`]) — so the bridge's net-side
//!   bytes are indistinguishable from any other TX: they pass the Lua
//!   pre-send hook and show up as the session's TX rows automatically
//!   (docs/ui-drafts/05-contract.md §1.5: "bridged data ALSO appears as
//!   session Rx (origin NetBridge)" — in the landed bus the direction split is
//!   net→serial = TX, serial→net = RX).
//!
//! The bridge is per session id: it binds to the session that was open when it
//! was attached and stops forwarding when that session goes away (the hub's
//! housekeeping detaches on the port's terminal events).

use std::sync::Arc;

use crate::core::bus::{Direction, Event, EventBus};
use crate::core::port::manager::PortManager;

/// Pushes bytes into a net channel; `false` when the channel is gone.
pub(crate) type SendFn = Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>;

/// Spawns the two forwarder tasks (through `runtime` — the hub attaches the
/// bridge from the UI thread); their abort handles end them on detach.
pub(crate) fn spawn(
    bus: EventBus,
    manager: Arc<PortManager>,
    channel_id: u64,
    session_id: u64,
    send: SendFn,
    runtime: &tokio::runtime::Handle,
) -> Vec<tokio::task::AbortHandle> {
    // Serial RX → net.
    let serial_to_net = {
        let bus = bus.clone();
        let send = Arc::clone(&send);
        runtime.spawn(async move {
            let mut events = bus.subscribe();
            while let Ok(event) = events.recv().await {
                if let Event::Data(data) = event {
                    if data.direction == Direction::Rx && data.session_id == session_id {
                        if !send(data.bytes.as_slice().to_vec()) {
                            return; // channel gone
                        }
                    }
                }
            }
        })
    };

    // Net RX → serial TX (the session write path).
    let net_to_serial = {
        let bus = bus.clone();
        runtime.spawn(async move {
            let mut events = bus.subscribe();
            while let Ok(event) = events.recv().await {
                if let Event::Net(net) = event {
                    if net.channel_id == channel_id {
                        if let super::NetEventKind::Data { direction: super::NetDirection::Rx, bytes, .. } =
                            net.kind
                        {
                            if manager.write(bytes.as_slice().to_vec()).await.is_err() {
                                return; // session gone
                            }
                        }
                    }
                }
            }
        })
    };

    vec![serial_to_net.abort_handle(), net_to_serial.abort_handle()]
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::core::bus::{publish_rx, Payload};
    use crate::core::nettools::{NetDirection, NetEventKind, NetMode, NetProtocol};
    use crate::core::port::manager::PortManager;
    use crate::core::port::mock::{MockBackend, MOCK_PORT_NAME};

    /// THE bridge proof over a real open session (MOCK0): serial RX of the
    /// bridged session is pushed into the channel closure, and net RX of the
    /// bridged channel goes out through `PortManager::write` — visible as the
    /// session's TX event. Other sessions' data and other channels' net data
    /// stay out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_moves_bytes_both_directions_over_a_real_session() {
        let bus = EventBus::new();
        let manager = Arc::new(PortManager::new(
            Arc::new(MockBackend::for_mode(crate::core::port::factory::MockMode::Single(crate::core::port::MockScenario::Echo))),
            bus.clone(),
        ));
        let session = manager.open(MOCK_PORT_NAME, crate::core::port::PortConfig::new(115_200))
            .await
            .expect("MOCK0 must open");

        // What the bridge pushes into "the net channel" (the closure fakes it).
        let reached_net: Arc<std::sync::Mutex<Vec<Vec<u8>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let send: SendFn = {
            let reached_net = Arc::clone(&reached_net);
            Arc::new(move |bytes: Vec<u8>| {
                reached_net.lock().unwrap().push(bytes);
                true
            })
        };

        const CHANNEL: u64 = 9;
        // Subscribe before anything publishes (live bus), and give the bridge
        // tasks a beat to make their own subscriptions.
        let mut events = bus.subscribe();
        let tasks = spawn(bus.clone(), Arc::clone(&manager), CHANNEL, session, send, &tokio::runtime::Handle::current());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Direction 1 (serial RX → net): publish a device chunk for the
        // bridged session; a neighbour session's chunk must stay out.
        publish_rx(&bus, session, b"from-device".to_vec());
        publish_rx(&bus, session + 100, b"other-session".to_vec());

        // Direction 2 (net → serial TX): a net RX chunk of the bridged
        // channel; a neighbour channel's chunk must stay out.
        bus.publish(Event::Net(crate::core::nettools::NetEvent {
            channel_id: CHANNEL,
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            timestamp: chrono::Local::now(),
            kind: NetEventKind::Data {
                direction: NetDirection::Rx,
                remote: "127.0.0.1:1".parse().unwrap(),
                bytes: Payload::from_vec(b"from-net".to_vec()),
            },
        }));
        bus.publish(Event::Net(crate::core::nettools::NetEvent {
            channel_id: CHANNEL + 1,
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            timestamp: chrono::Local::now(),
            kind: NetEventKind::Data {
                direction: NetDirection::Rx,
                remote: "127.0.0.1:1".parse().unwrap(),
                bytes: Payload::from_vec(b"other-channel".to_vec()),
            },
        }));

        // Direction 1 completes asynchronously; poll for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = reached_net.lock().unwrap().contains(&b"from-device".to_vec());
            if got || tokio::time::Instant::now() > deadline {
                assert!(got, "serial RX never reached the net side");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !reached_net.lock().unwrap().iter().any(|bytes| bytes == b"other-session"),
            "another session's RX must not cross the bridge"
        );

        // Direction 2: the net chunk went out the port — its TX event lands on
        // the bus (and the mock's echo comes back as RX, which the bridge also
        // forwards: whatever arrives here, the TX of `from-net` is the proof).
        let wait = async {
            while let Ok(event) = events.recv().await {
                if let Event::Data(data) = event {
                    if data.session_id == session
                        && data.direction == Direction::Tx
                        && data.bytes.as_slice() == b"from-net"
                    {
                        return true;
                    }
                }
            }
            false
        };
        let proved = tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap_or(false);
        assert!(proved, "net RX never reached the port write path");

        // Detach: the abort handles end both forwarders.
        for task in &tasks {
            task.abort();
        }
        manager.close().await;
    }
}
