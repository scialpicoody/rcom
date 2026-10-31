//! The reconnect state machine (docs/design.md, "Serial layer"; UI↔Core contract §3):
//! `Closed → Open → (error/unplug) → WatchDesired → Open`, llcom semantics.
//!
//! One task over the event bus, tracking **every session independently**
//! (ticket #35 — the pre-#35 machine held one armed phase and so serialized
//! real-port sessions): it arms when a session opens (`auto-reopen only when
//! the user left it open`), disarms when the user closes *that* session, and
//! on a mid-stream error it publishes [`PortState::WatchDesired`] for the dead
//! session and polls for the *same port* (identity = name + VID/PID) to
//! reappear — reacting both to the hotplug poller's global `Arrived` events
//! and to its own 1 s backoff poll. A reopen reuses the exact [`PortConfig`]
//! of the armed session (the bus carries the baud on `Open`). Sessions on
//! other ports are never touched: opening session B never disarms A, and two
//! ports can be watched at once.
//!
//! Deliberate choice: the machine never closes the dead session. The dead
//! session stays in the [`PortManager`](super::manager::PortManager) slot (its
//! forwarder has ended; reader threads self-cancel) and is cleaned up by
//! whichever call replaces or closes it — `manager.open()` (dead sessions are
//! replaced) and `manager.close_session()` both do. The user's `Closed` event
//! therefore always means "user cancelled", which is exactly the disarm
//! condition of the contract's `WatchDesired → Closed` transition. When the
//! port comes back but someone else already opened it (the open fails with
//! `AlreadyOpen`), the machine stands down: the newer session's own `Open`
//! event armed it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::MissedTickBehavior;

use super::manager::PortManager;
use super::{PortConfig, PortInfo};
use crate::core::bus::{Event, EventBus, LogEvent, LogLevel, PortEvent, PortState};
use crate::core::error::CoreError;

/// How often the watching machine polls the port list on its own — shared with the
/// hotplug poller's 1 s cadence (design.md: "poll `available_ports()` (1 s)").
pub const WATCH_POLL: Duration = Duration::from_secs(1);

/// Backoff for repeated failed re-opens (a port can be listed while its driver is
/// not ready yet): doubles from [`BACKOFF_START`], capped at [`BACKOFF_MAX`].
pub const BACKOFF_START: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The delay before retry `attempt` (0-based) after a failed re-open: 1 s, 2 s, 4 s,
/// then capped at 5 s.
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(4);
    let doubled = BACKOFF_START
        .checked_mul(1u32 << shift)
        .unwrap_or(BACKOFF_MAX);
    doubled.min(BACKOFF_MAX)
}

/// The detail strings the machine publishes (the UI render layer localizes
/// these two known values; ticket #25's sweep keeps core strings English-only
/// data and translates at the render boundary).
pub const DETAIL_WATCHING: &str = "waiting for the port to reappear (auto-reconnect)";
pub const DETAIL_RECONNECTED: &str = "auto-reconnected";

/// The identity of a port: its OS name plus USB VID/PID when the OS reports them
/// (design.md: "Port identity = name + VID/PID"). Ports without USB identity (the
/// mock, or a non-USB serial device) are identified by name alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]

pub struct PortIdentity {
    pub name: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
}

impl PortIdentity {
    /// The identity of a listed port.
    pub fn of(info: &PortInfo) -> Self {
        Self {
            name: info.name.clone(),
            vid: info.vid,
            pid: info.pid,
        }
    }

    /// A name-only identity (fallback when the port was not listed at arm time).
    pub fn by_name(name: &str) -> Self {
        Self {
            name: name.to_string(),
            vid: None,
            pid: None,
        }
    }

    /// Whether `info` is the same physical port as this identity. USB ids must match
    /// when both sides report them; a name-only identity matches any port of the
    /// same name.
    pub fn matches(&self, info: &PortInfo) -> bool {
        if info.name != self.name {
            return false;
        }
        match (self.vid, self.pid, info.vid, info.pid) {
            (Some(vid_a), Some(pid_a), Some(vid_b), Some(pid_b)) => {
                vid_a == vid_b && pid_a == pid_b
            }
            _ => true,
        }
    }
}

/// What the machine is doing for one session. Per-session maps replace the
/// pre-#35 single-phase enum: sessions on different ports arm, die and
/// reconnect independently.
#[derive(Clone)]
struct Armed {
    desired: PortIdentity,
    config: PortConfig,
}

/// A dead session the machine is watching: the port to look for, the config
/// to re-open with, and the backoff state.
struct WatchState {
    desired: PortIdentity,
    config: PortConfig,
    /// The dead session's id (the WatchDesired/Reopened events' routing key).
    session_id: u64,
    attempt: u32,
    /// Earliest instant the next re-open attempt may run (backoff).
    next_try: Instant,
}

/// Starts the reconnect machine: one task for the lifetime of the manager.
pub fn spawn(manager: Arc<PortManager>, bus: EventBus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        ReconnectMachine {
            manager,
            bus,
            armed: HashMap::new(),
            watching: HashMap::new(),
        }
        .run()
        .await
    })
}

struct ReconnectMachine {
    manager: Arc<PortManager>,
    bus: EventBus,
    /// Sessions the user left open, keyed by session id.
    armed: HashMap<u64, Armed>,
    /// Dead sessions being watched, keyed by the dead session's id.
    watching: HashMap<u64, WatchState>,
}

impl ReconnectMachine {
    async fn run(mut self) {
        let mut events = self.bus.subscribe();
        let mut tick = tokio::time::interval(WATCH_POLL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let watching = !self.watching.is_empty();
            tokio::select! {
                // The watch poll only runs while something is watched (the guard
                // keeps the idle machine event-driven; a re-enabled interval fires
                // immediately, which doubles as the "check right away" on entering
                // WatchDesired).
                _ = tick.tick(), if watching => {
                    self.on_watch_tick().await;
                }
                event = events.recv() => match event {
                    Ok(event) => self.on_event(event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "reconnect machine lagged on the bus");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }

    async fn on_event(&mut self, event: Event) {
        let Event::Port(port) = event else {
            return;
        };
        match port.state {
            PortState::Open => {
                // A new live session on this port replaces any dead one the
                // machine was watching (the reopen path — the manager already
                // dropped the dead session): stand that watch down.
                let stood_down: Vec<u64> = self
                    .watching
                    .values()
                    .filter(|watch| watch.desired.name == port.port)
                    .map(|watch| watch.session_id)
                    .collect();
                for session_id in stood_down {
                    tracing::debug!(session = session_id, port = %port.port,
                        "watch stood down: the port was reopened");
                    self.watching.remove(&session_id);
                }
                // (Re)arm auto-reconnect for the new session — `auto-reopen
                // only when the user left it open`, so every `Open` arms and
                // every user `Closed` disarms.
                let config = PortConfig::new(port.baud.unwrap_or(PortConfig::default().baud_rate));
                let desired = self.identify(&port.port).await;
                tracing::debug!(session = port.session_id, port = %desired.name, "reconnect machine armed");
                self.armed.insert(port.session_id, Armed { desired, config });
            }
            PortState::Error => {
                // The session died while it should be open: watch for its port.
                // (Died after a user close, or nobody armed it: ignore.)
                if let Some(Armed { desired, config }) = self.armed.remove(&port.session_id) {
                    tracing::info!(session = port.session_id, port = %port.port,
                        "session error — watching for the port to reappear");
                    self.publish(
                        port.session_id,
                        &port.port,
                        PortState::WatchDesired,
                        Some(DETAIL_WATCHING.into()),
                        None,
                    );
                    self.watching.insert(
                        port.session_id,
                        WatchState {
                            desired,
                            config,
                            session_id: port.session_id,
                            attempt: 0,
                            next_try: Instant::now(),
                        },
                    );
                }
            }
            PortState::Closed => {
                // The user's close (or the app teardown): cancel any arm or
                // watch for that session. Other sessions are untouched.
                self.armed.remove(&port.session_id);
                self.watching.remove(&port.session_id);
            }
            PortState::Arrived => {
                // The hotplug poller announcing a port while we watch for one:
                // full identity is re-checked against the port list on reopen.
                let watchers: Vec<u64> = self
                    .watching
                    .values()
                    .filter(|watch| watch.desired.name == port.port)
                    .map(|watch| watch.session_id)
                    .collect();
                for session_id in watchers {
                    self.try_reopen(session_id).await;
                }
            }
            // Removed: while armed, the reader reports the failure itself (the
            // authoritative Error arrives within a read timeout); while watching
            // the port is already gone. Informational either way.
            PortState::WatchDesired | PortState::Reopened | PortState::Removed => {}
        }
    }

    /// One watch-poll tick: retry every due re-open (the backoff window holds
    /// the rest).
    async fn on_watch_tick(&mut self) {
        let now = Instant::now();
        let due: Vec<u64> = self
            .watching
            .values()
            .filter(|watch| now >= watch.next_try)
            .map(|watch| watch.session_id)
            .collect();
        for session_id in due {
            self.try_reopen(session_id).await;
        }
    }

    /// Looks for the watched port of `session_id` right now and, if it is
    /// back, re-opens it with the armed config. A failed attempt arms the
    /// backoff (1 s → 5 s cap); `AlreadyOpen` stands the watch down (someone
    /// else opened the port — their `Open` event armed their own session).
    async fn try_reopen(&mut self, session_id: u64) {
        let Some(WatchState { desired, config, .. }) = self.watching.get(&session_id) else {
            return;
        };
        let desired = desired.clone();
        let config = config.clone();
        let ports = match self.manager.list_ports().await {
            Ok(ports) => ports,
            Err(error) => {
                tracing::warn!("watch poll failed to list ports: {error}");
                self.watch_again(session_id);
                return;
            }
        };
        if !ports.iter().any(|info| desired.matches(info)) {
            return; // not back yet; keep watching on the next tick
        }
        match self.manager.open(&desired.name, config.clone()).await {
            Ok(new_id) => {
                self.watching.remove(&session_id);
                tracing::info!(port = %desired.name, session = new_id, "auto-reopen succeeded");
                self.publish(
                    new_id,
                    &desired.name,
                    PortState::Reopened,
                    Some(DETAIL_RECONNECTED.into()),
                    Some(config.baud_rate),
                );
                self.armed.insert(new_id, Armed { desired, config });
            }
            Err(CoreError::AlreadyOpen(_)) => {
                self.watching.remove(&session_id);
                tracing::info!(
                    port = %desired.name,
                    "auto-reopen stood down: the port was reopened elsewhere"
                );
            }
            Err(error) => {
                tracing::warn!(port = %desired.name, "re-open attempt failed: {error}");
                self.bus.publish(Event::Log(LogEvent {
                    level: LogLevel::Warn,
                    timestamp: chrono::Local::now(),
                    message: format!("reconnect attempt on {} failed: {error}", desired.name),
                    session_id: Some(session_id),
                }));
                self.watch_again(session_id);
            }
        }
    }

    /// Arms the backoff for the watched session after one failed attempt.
    fn watch_again(&mut self, session_id: u64) {
        let Some(watch) = self.watching.get_mut(&session_id) else {
            return;
        };
        watch.attempt = watch.attempt.saturating_add(1);
        watch.next_try = Instant::now() + backoff_delay(watch.attempt);
    }

    /// The identity of `name` as listed right now; name-only when it is not listed
    /// (or enumeration fails).
    async fn identify(&self, name: &str) -> PortIdentity {
        match self.manager.list_ports().await {
            Ok(ports) => ports
                .iter()
                .find(|info| info.name == name)
                .map(PortIdentity::of)
                .unwrap_or_else(|| PortIdentity::by_name(name)),
            Err(_) => PortIdentity::by_name(name),
        }
    }

    fn publish(
        &self,
        session_id: u64,
        port: &str,
        state: PortState,
        detail: Option<String>,
        baud: Option<u32>,
    ) {
        self.bus.publish(Event::Port(PortEvent {
            session_id,
            port: port.to_string(),
            state,
            detail,
            timestamp: chrono::Local::now(),
            baud,
        }));
    }
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

    #[test]
    fn identity_matches_same_usb_port() {
        let identity = PortIdentity::of(&info("COM3", Some(0x2e8a), Some(0x000a)));
        assert!(identity.matches(&info("COM3", Some(0x2e8a), Some(0x000a))));
        // A different device behind the same name must not match.
        assert!(!identity.matches(&info("COM3", Some(0x10c4), Some(0xea60))));
        // Nor the same device under a different name.
        assert!(!identity.matches(&info("COM7", Some(0x2e8a), Some(0x000a))));
    }

    #[test]
    fn identity_without_usb_ids_matches_by_name() {
        let identity = PortIdentity::of(&info("MOCK0", None, None));
        assert!(identity.matches(&info("MOCK0", None, None)));
        assert!(!identity.matches(&info("MOCK1", None, None)));
    }

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), BACKOFF_MAX);
        assert_eq!(backoff_delay(99), BACKOFF_MAX);
    }

    /// THE per-session reconnect AC (ticket #35): MOCK3's session dies and the
    /// machine watches + re-opens **it** while MOCK0's session — armed all
    /// along — streams untouched. The dead session is replaced in the
    /// manager's map (no leak).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_session_reconnect_watches_one_session_without_touching_another() {
        use crate::core::bus::Direction;
        use crate::core::port::factory::{BackendFactory, MockMode};
        use crate::core::port::mock::MockTrigger;
        use crate::core::port::testing::EventTap;

        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let backend = BackendFactory::with_mode(MockMode::All);
        let manager = Arc::new(PortManager::new(Arc::new(backend), bus.clone()));
        spawn(manager.clone(), bus.clone());

        let healthy = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open echo");
        let dying = manager.open("MOCK3", PortConfig::new(115_200)).await.expect("open unplug");

        // Kill MOCK3's session; the machine must watch it (its own session id)
        // while MOCK0 keeps working.
        let control = loop {
            if let Some(control) = manager.mock_controls(dying) {
                break control;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        control.set_speed(8.0);
        control.trigger(MockTrigger::Unplug);

        let watch = tap.next_port_state(Some(dying), PortState::WatchDesired, Duration::from_secs(10));
        assert_eq!(watch.port, "MOCK3", "the watch is on the dead session's port");

        // The healthy session is untouched the whole time.
        manager.write_for(healthy, b"alive".to_vec()).await.expect("write healthy");
        let tx = tap.next_data(healthy, Direction::Tx, Duration::from_secs(5));
        assert_eq!(tx.bytes.as_slice(), b"alive");
        assert!(manager.session_alive(healthy).await, "MOCK0 was never disturbed");

        // The machine re-opens MOCK3 (8× reappear ≈ 625 ms + the poll cadence).
        let reopened = tap.next_port_state(None, PortState::Reopened, Duration::from_secs(15));
        assert_eq!(reopened.port, "MOCK3");
        assert_ne!(reopened.session_id, dying, "a fresh session id");
        assert!(manager.session_alive(reopened.session_id).await);
        assert!(
            manager.session_alive(healthy).await,
            "the healthy session survived the reconnect cycle"
        );
        let ids = manager.session_ids().await;
        assert_eq!(ids.len(), 2, "the dead session was replaced, not leaked: {ids:?}");
        assert!(ids.contains(&healthy) && ids.contains(&reopened.session_id), "{ids:?}");

        manager.close_all().await;
    }
}
