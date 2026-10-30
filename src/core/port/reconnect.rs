//! The reconnect state machine (docs/design.md, "Serial layer"; UI↔Core contract §3):
//! `Closed → Open → (error/unplug) → WatchDesired → Open`, llcom semantics.
//!
//! One task over the event bus. It arms when a session opens (`auto-reopen only
//! when the user left it open`), disarms when the user closes, and on a mid-stream
//! error it publishes [`PortState::WatchDesired`] and polls for the *same port*
//! (identity = name + VID/PID) to reappear — reacting both to the hotplug poller's
//! global `Arrived` events and to its own 1 s backoff poll. A reopen reuses the
//! exact [`PortConfig`] of the armed session (the bus carries the baud on `Open`).
//!
//! Deliberate choice: the machine never closes the dead session. The dead session
//! stays in the [`PortManager`](super::manager::PortManager) slot (its forwarder has
//! ended; reader threads self-cancel) and is cleaned up by whichever call replaces
//! or closes it — `manager.open()` and `manager.close()` both close the previous
//! session first. The user's `Closed` event therefore always means "user cancelled",
//! which is exactly the disarm condition of the contract's `WatchDesired → Closed`
//! transition.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::MissedTickBehavior;

use super::manager::PortManager;
use super::{PortConfig, PortInfo};
use crate::core::bus::{Event, EventBus, LogEvent, LogLevel, PortEvent, PortState};

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

/// What the machine is doing (the contract's three states; `Open` is split into
/// armed/idle so only a user-opened session ever auto-reconnects).
enum Phase {
    /// No session, or the user closed it: nothing watches.
    Idle,
    /// A session the user (or a previous auto-reopen) left open is running.
    Armed { desired: PortIdentity, config: PortConfig },
    /// The session died: watching for the port to return.
    Watching {
        desired: PortIdentity,
        config: PortConfig,
        session_id: u64,
        attempt: u32,
        /// Earliest instant the next re-open attempt may run (backoff).
        next_try: Instant,
    },
}

/// Starts the reconnect machine: one task for the lifetime of the manager.
pub fn spawn(manager: Arc<PortManager>, bus: EventBus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { ReconnectMachine { manager, bus }.run().await })
}

struct ReconnectMachine {
    manager: Arc<PortManager>,
    bus: EventBus,
}

impl ReconnectMachine {
    async fn run(self) {
        let mut events = self.bus.subscribe();
        let mut tick = tokio::time::interval(WATCH_POLL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut phase = Phase::Idle;
        loop {
            let watching = matches!(phase, Phase::Watching { .. });
            tokio::select! {
                // The watch poll only runs while watching (the guard keeps the idle
                // machine event-driven; a re-enabled interval fires immediately,
                // which doubles as the "check right away" on entering WatchDesired).
                _ = tick.tick(), if watching => {
                    phase = self.on_watch_tick(phase).await;
                }
                event = events.recv() => match event {
                    Ok(event) => phase = self.on_event(event, phase).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "reconnect machine lagged on the bus");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }

    async fn on_event(&self, event: Event, phase: Phase) -> Phase {
        let Event::Port(port) = event else {
            return phase;
        };
        match port.state {
            PortState::Open => self.arm(&port, phase).await,
            PortState::Error => self.enter_watch(&port, phase),
            PortState::Closed => {
                // The user's close (or the app teardown): cancel any watching.
                Phase::Idle
            }
            PortState::Arrived => match &phase {
                // The hotplug poller announcing a port while we watch for one:
                // full identity is re-checked against the port list on reopen.
                Phase::Watching { desired, .. } if desired.name == port.port => {
                    self.try_reopen(phase).await
                }
                _ => phase,
            },
            // Removed: while armed, the reader reports the failure itself (the
            // authoritative Error arrives within a read timeout); while watching
            // the port is already gone. Informational either way.
            PortState::WatchDesired | PortState::Reopened | PortState::Removed => phase,
        }
    }

    /// A session opened: (re)arm auto-reconnect for it — `auto-reopen only when the
    /// user left it open`, so every `Open` arms and every user `Closed` disarms.
    async fn arm(&self, port: &PortEvent, _phase: Phase) -> Phase {
        let config = PortConfig::new(port.baud.unwrap_or(PortConfig::default().baud_rate));
        let desired = self.identify(&port.port).await;
        tracing::debug!(port = %desired.name, "reconnect machine armed");
        Phase::Armed { desired, config }
    }

    /// The session died on an error/unplug while it should be open: start watching.
    fn enter_watch(&self, port: &PortEvent, phase: Phase) -> Phase {
        let Phase::Armed { desired, config } = phase else {
            return phase; // died after a user close, or nobody was armed: ignore
        };
        tracing::info!(port = %port.port, "session error — watching for the port to reappear");
        self.publish(
            port.session_id,
            &port.port,
            PortState::WatchDesired,
            Some("waiting for the port to reappear (auto-reconnect)".into()),
            None,
        );
        Phase::Watching {
            desired,
            config,
            session_id: port.session_id,
            attempt: 0,
            next_try: Instant::now(),
        }
    }

    /// One watch-poll tick: retry the re-open unless the backoff window is open.
    async fn on_watch_tick(&self, phase: Phase) -> Phase {
        let Phase::Watching { next_try, .. } = &phase else {
            return phase;
        };
        if Instant::now() < *next_try {
            return phase; // backing off after a failed attempt
        }
        self.try_reopen(phase).await
    }

    /// Looks for the desired port right now and, if it is back, re-opens it with the
    /// armed config. A failed attempt arms the backoff (1 s → 5 s cap).
    async fn try_reopen(&self, phase: Phase) -> Phase {
        let watching = match &phase {
            Phase::Watching {
                desired,
                config,
                session_id,
                attempt,
                ..
            } => Some((desired.clone(), config.clone(), *session_id, *attempt)),
            _ => None,
        };
        let Some((desired, config, session_id, attempt)) = watching else {
            return phase;
        };
        let ports = match self.manager.list_ports().await {
            Ok(ports) => ports,
            Err(error) => {
                tracing::warn!("watch poll failed to list ports: {error}");
                return self.watch_again(desired, config, session_id, attempt);
            }
        };
        if !ports.iter().any(|info| desired.matches(info)) {
            return phase; // not back yet; keep watching on the next tick
        }
        match self.manager.open(&desired.name, config.clone()).await {
            Ok(new_id) => {
                tracing::info!(port = %desired.name, session = new_id, "auto-reopen succeeded");
                self.publish(
                    new_id,
                    &desired.name,
                    PortState::Reopened,
                    Some("auto-reconnected".into()),
                    Some(config.baud_rate),
                );
                Phase::Armed { desired, config }
            }
            Err(error) => {
                tracing::warn!(port = %desired.name, "re-open attempt failed: {error}");
                self.bus.publish(Event::Log(LogEvent {
                    level: LogLevel::Warn,
                    timestamp: chrono::Local::now(),
                    message: format!("reconnect attempt on {} failed: {error}", desired.name),
                }));
                self.watch_again(desired, config, session_id, attempt + 1)
            }
        }
    }

    fn watch_again(
        &self,
        desired: PortIdentity,
        config: PortConfig,
        session_id: u64,
        attempt: u32,
    ) -> Phase {
        let next_try = Instant::now() + backoff_delay(attempt);
        Phase::Watching {
            desired,
            config,
            session_id,
            attempt: attempt + 1,
            next_try,
        }
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
            is_mock: false,
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
}
