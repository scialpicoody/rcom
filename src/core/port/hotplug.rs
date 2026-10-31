//! The hotplug poller (docs/design.md, "Stack": poll `available_ports()` (1 s) —
//! portable and robust). Every second it enumerates the ports, diffs the result
//! against the previous listing and publishes process-global
//! ([`GLOBAL_SESSION_ID`]) [`PortState::Arrived`] / [`PortState::Removed`] events —
//! the contract §1 "Port (global)" feed for the port list and the
//! [`reconnect`](super::reconnect) machine's WatchDesired trigger.
//!
//! The first listing is the silent baseline: only *changes* are events.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;

use super::reconnect::PortIdentity;
use super::{PortInfo, SerialBackend};
use crate::core::bus::{Event, EventBus, PortEvent, PortState, GLOBAL_SESSION_ID};

/// The poll cadence (design.md: 1 s).
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The ports that arrived and left between two listings, in listing order.
pub fn diff_ports(prev: &HashSet<PortIdentity>, next: &[PortInfo]) -> (Vec<PortIdentity>, Vec<PortIdentity>) {
    let next_keys: HashSet<PortIdentity> = next.iter().map(PortIdentity::of).collect();
    let arrived = next
        .iter()
        .map(PortIdentity::of)
        .filter(|identity| !prev.contains(identity))
        .collect();
    let removed = prev
        .difference(&next_keys)
        .cloned()
        .collect();
    (arrived, removed)
}

/// Starts the poller: one task enumerating every [`POLL_INTERVAL`] for the lifetime
/// of the process.
pub fn spawn(backend: Arc<dyn SerialBackend>, bus: EventBus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let first = enumerate(&backend).await.unwrap_or_default();
        let mut current: HashSet<PortIdentity> = first.iter().map(PortIdentity::of).collect();

        loop {
            tick.tick().await;
            let ports = enumerate(&backend).await.unwrap_or_default();
            let (arrived, removed) = diff_ports(&current, &ports);
            for identity in arrived {
                tracing::debug!(port = %identity.name, "hotplug: port arrived");
                publish(&bus, &identity, PortState::Arrived);
            }
            for identity in removed {
                tracing::debug!(port = %identity.name, "hotplug: port removed");
                publish(&bus, &identity, PortState::Removed);
            }
            current = ports.iter().map(PortIdentity::of).collect();
        }
    })
}

async fn enumerate(backend: &Arc<dyn SerialBackend>) -> crate::core::Result<Vec<PortInfo>> {
    // `enumerate` touches the OS: keep it off the async workers.
    let backend = Arc::clone(backend);
    tokio::task::spawn_blocking(move || backend.enumerate()).await?
}

fn publish(bus: &EventBus, identity: &PortIdentity, state: PortState) {
    bus.publish(Event::Port(PortEvent {
        session_id: GLOBAL_SESSION_ID,
        port: identity.name.clone(),
        state,
        detail: None,
        timestamp: chrono::Local::now(),
        baud: None,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, vid: Option<u16>, pid: Option<u16>) -> PortInfo {
        PortInfo {
            name: name.into(),
            vid,
            pid,
            description: None,
            kind: crate::core::port::PortKind::Real,
        }
    }

    /// The diff is the exact set-level change between two listings: arrivals are the
    /// new identities, removals the vanished ones (the hotplug poller's only logic).
    #[test]
    fn diff_reports_arrivals_and_removals() {
        let prev: HashSet<PortIdentity> = [info("COM3", Some(1), Some(2)), info("COM4", None, None)]
            .iter()
            .map(PortIdentity::of)
            .collect();

        // COM3 came back as different hardware; COM5 is new; COM4 stayed.
        let next = vec![
            info("COM3", Some(9), Some(9)),
            info("COM4", None, None),
            info("COM5", None, None),
        ];
        let (arrived, removed) = diff_ports(&prev, &next);

        assert_eq!(arrived.len(), 2);
        assert!(arrived.contains(&PortIdentity::of(&info("COM3", Some(9), Some(9)))));
        assert!(arrived.contains(&PortIdentity::of(&info("COM5", None, None))));
        assert_eq!(removed, vec![PortIdentity::of(&info("COM3", Some(1), Some(2)))]);
    }

    #[test]
    fn diff_of_unchanged_listing_is_empty() {
        let ports = vec![info("COM3", Some(1), Some(2))];
        let prev: HashSet<PortIdentity> = ports.iter().map(PortIdentity::of).collect();
        let (arrived, removed) = diff_ports(&prev, &ports);
        assert!(arrived.is_empty() && removed.is_empty());
    }
}
