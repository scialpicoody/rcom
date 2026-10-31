//! The mock backend: hardware-free fake ports that play *scripts* — the demo
//! seam formalized by design-ui §5.7/§6. One [`MockBackend`] owns the mock
//! ports the factory configured (four scenarios under `RCOM_MOCK=all`, one
//! otherwise); every open spawns a player thread that walks the scenario's
//! timeline on a virtual clock.
//!
//! **Playback controls** ([`MockControl`]): pause/resume, speed (0.25×–8×),
//! step, RX injection, scenario triggers (unplug / reappear / error /
//! corrupt-frame) and restart. The handle rides [`super::OpenPort::control`]
//! out of `open()`; the manager surfaces it as `mock_controls(session_id)`.
//! Timelines are wall-clock-relative and scaled by speed; every scenario is
//! deterministic at 1× with no injected triggers (the noisy scenario's
//! randomness is a fixed-seed LCG).
//!
//! **Unplug realism:** an unplugged mock port disappears from the listing,
//! refuses `open`, and a live session dies with a reader error — exactly what
//! a real unplug does, so the reconnect machine and hotplug poller run against
//! the same code paths as with hardware. The `Unplug` scenario additionally
//! plays the full unplug → reappear cycle on a backend-level lifecycle thread
//! (it must outlive the sessions it kills).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::{OpenPort, PortConfig, PortInfo, PortKind, PortWriter, ReadEvent, SerialBackend};
use crate::core::error::{CoreError, Result};

/// The name of the first mock port (single-scenario modes present exactly one
/// port, always named `MOCK0`).
pub const MOCK_PORT_NAME: &str = "MOCK0";

/// How often the player thread re-checks its flags while idle (bounds how long
/// a pause/inject/unplug takes to be noticed).
const PLAYER_TICK: Duration = Duration::from_millis(20);

/// Virtual-clock advance of one `step()` while paused (one framed-protocol
/// chunk at the scenario's 100 ms cadence).
const STEP_MS: f64 = 100.0;

/// How long the echo scenario takes to "wire" a written byte back as RX:
/// just enough for the log view to show TX and RX as two distinct rows (§6.2).
const ECHO_DELAY_MS: f64 = 30.0;

/// The delay between a mock unplug and the port reappearing (scenario D,
/// §6.5's "adjustable 1–30 s" default of 5 s).
pub const UNPLUG_REAPPEAR_MS: u64 = 5_000;

/// When the `Unplug` scenario unplugs on its own after an open (§6.5).
pub const UNPLUG_AFTER_MS: u64 = 10_000;

/// The four mock scenarios (design-ui §6.2–6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockScenario {
    /// A — echo device (`MOCK0`): every write echoed back after 30 ms.
    Echo,
    /// B — noisy sensor (`MOCK1`): ASCII frames, garbled bursts, split frames.
    Noisy,
    /// C — framed protocol (`MOCK2`): `AA 55` frames feeding the plot decoder.
    Framed,
    /// D — unplug/error driver (`MOCK3`): status lines plus the unplug cycle.
    Unplug,
}

impl MockScenario {
    /// Every scenario in `RCOM_MOCK=all` order (the port indexes follow).
    pub const ALL: [MockScenario; 4] = [
        MockScenario::Echo,
        MockScenario::Noisy,
        MockScenario::Framed,
        MockScenario::Unplug,
    ];

    /// The stable id (also the `RCOM_MOCK` value that selects it).
    pub fn id(self) -> &'static str {
        match self {
            MockScenario::Echo => "echo",
            MockScenario::Noisy => "noisy",
            MockScenario::Framed => "framed",
            MockScenario::Unplug => "unplug",
        }
    }

    /// Looks a scenario up by [`id`](Self::id); `None` when unknown.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|scenario| scenario.id() == id)
    }

    /// The human-readable name (a proper noun; not localized — core data).
    pub fn display_name(self) -> &'static str {
        match self {
            MockScenario::Echo => "echo device",
            MockScenario::Noisy => "noisy sensor",
            MockScenario::Framed => "framed protocol",
            MockScenario::Unplug => "unplug driver",
        }
    }
}

/// What `trigger(Unplug | Error | Reappear | …)` can fire (design-ui §6.1;
/// `CorruptFrame` is the framed scenario's extra).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockTrigger {
    /// Pull the plug: the port delists, a live session dies with a reader
    /// error (the `Unplug` scenario follows with its scripted reappear).
    Unplug,
    /// Put the plug back (cancels a pending reappear countdown).
    Reappear,
    /// Kill the live session with an injected reader error.
    Error,
    /// Framed scenario: corrupt exactly one frame (a bad checksum), exercising
    /// the decoder's skip-and-note path.
    CorruptFrame,
}

/// The shared playback state of one mock port: what a [`MockControl`]
/// manipulates and what the player (and lifecycle) threads read.
#[derive(Default)]
struct PlayerShared {
    paused: AtomicBool,
    /// Playback speed, 0.25–8 (an `RwLock` because there is no atomic f64).
    speed: RwLock<f64>,
    /// Restart counter: the player resets its timeline when this changes.
    playback_generation: AtomicU64,
    /// Steps requested while paused (each advances the virtual clock one step).
    steps: AtomicU64,
    /// Bytes to emit as RX on the next tick (`inject_rx`).
    inject: Mutex<VecDeque<Vec<u8>>>,
    /// Queued `Error`/`CorruptFrame` triggers.
    triggers: Mutex<VecDeque<MockTrigger>>,
    /// Framed scenario: corrupt the next frame once (the `CorruptFrame` trigger).
    corrupt_next: AtomicBool,
    /// `Unplug`-scenario trigger consumed by the lifecycle thread.
    trig_unplug: AtomicBool,
    /// `Reappear` trigger consumed by the lifecycle thread's reappear wait.
    trig_reappear: AtomicBool,
    /// Whether the port is present (plugged). False = delisted + refuses open +
    /// live sessions die, like a real vanished port.
    plug: Arc<AtomicBool>,
    /// Bumped by every `open()`: the lifecycle thread re-arms the unplug cycle.
    open_epoch: AtomicU64,
    /// The condvar's mutex (the lifecycle thread's wait) + the condvar itself.
    epoch_mutex: Mutex<()>,
    epoch_cond: std::sync::Condvar,
}

impl PlayerShared {
    fn speed(&self) -> f64 {
        *self.speed.read().expect("mock speed lock")
    }

    /// Sleeps out `virtual_time` of *timeline* time: converted to wall time by
    /// the current speed, in small slices so a speed change lands promptly
    /// (each slice re-reads the speed).
    fn sleep_scaled(&self, virtual_time: Duration) {
        let speed = self.speed().clamp(0.25, 8.0);
        let mut left = virtual_time.mul_f64(1.0 / speed);
        let slice = Duration::from_millis(5);
        while !left.is_zero() {
            let step = left.min(slice);
            std::thread::sleep(step);
            left = left.saturating_sub(step);
        }
    }
}

/// The playback control of one mock port (design-ui §6.1). Clones share the
/// port's state; `Send + Sync`, so the core driver task (and through it the
/// UI's mock drawer) can drive playback from anywhere.
#[derive(Clone)]
pub struct MockControl {
    scenario: MockScenario,
    shared: Arc<PlayerShared>,
}

impl MockControl {
    /// The scenario this port plays.
    pub fn scenario(&self) -> MockScenario {
        self.scenario
    }

    /// Pauses or resumes the timeline (data stops flowing while paused; the
    /// port stays open).
    pub fn pause(&self, on: bool) {
        self.shared.paused.store(on, Ordering::Relaxed);
        tracing::info!(scenario = self.scenario.id(), paused = on, "mock playback");
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Sets the playback speed (clamped to 0.25×–8×).
    pub fn set_speed(&self, speed: f64) {
        let clamped = speed.clamp(0.25, 8.0);
        *self.shared.speed.write().expect("mock speed lock") = clamped;
        tracing::info!(scenario = self.scenario.id(), speed = clamped, "mock speed");
    }

    pub fn speed(&self) -> f64 {
        self.shared.speed()
    }

    /// While paused, advances the virtual clock one step (one scripted item at
    /// the scenario's natural cadence). Ignored while running.
    pub fn step(&self) {
        if self.paused() {
            self.shared.steps.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Injects bytes as an immediate RX chunk (as if the device had sent them).
    pub fn inject_rx(&self, bytes: Vec<u8>) {
        self.shared
            .inject
            .lock()
            .expect("mock inject lock")
            .push_back(bytes);
    }

    /// Fires a scenario trigger.
    pub fn trigger(&self, trigger: MockTrigger) {
        match trigger {
            MockTrigger::Unplug => {
                if self.scenario == MockScenario::Unplug {
                    // Hand the unplug to the lifecycle thread so the scripted
                    // reappear countdown follows it.
                    self.shared.trig_unplug.store(true, Ordering::Relaxed);
                    self.shared.epoch_cond.notify_all();
                } else {
                    self.shared.plug.store(false, Ordering::Relaxed);
                    tracing::info!(scenario = self.scenario.id(), "mock unplugged (injected)");
                }
            }
            MockTrigger::Reappear => {
                self.shared.trig_reappear.store(true, Ordering::Relaxed);
                self.shared.plug.store(true, Ordering::Relaxed);
                self.shared.epoch_cond.notify_all();
                tracing::info!(scenario = self.scenario.id(), "mock replugged (injected)");
            }
            MockTrigger::Error | MockTrigger::CorruptFrame => {
                self.shared
                    .triggers
                    .lock()
                    .expect("mock trigger lock")
                    .push_back(trigger);
            }
        }
    }

    /// Restarts the timeline from its beginning (the open banner re-fires).
    pub fn restart(&self) {
        self.shared.playback_generation.fetch_add(1, Ordering::Relaxed);
        tracing::info!(scenario = self.scenario.id(), "mock timeline restarted");
    }
}

/// One mock port of the backend: its name, scenario and shared player state.
struct MockPort {
    name: String,
    scenario: MockScenario,
    shared: Arc<PlayerShared>,
    /// Guard so the Unplug scenario's lifecycle thread spawns exactly once.
    lifecycle_started: OnceLock<()>,
}

impl MockPort {
    fn info(&self) -> PortInfo {
        PortInfo {
            name: self.name.clone(),
            vid: None,
            pid: None,
            description: Some(format!(
                "rcom mock {} ({})",
                self.scenario.display_name(),
                self.scenario.id()
            )),
            kind: PortKind::Mock(self.scenario),
        }
    }
}

/// The mock backend: the scenario ports the factory configured. One instance
/// per engine, behind the [`super::BackendFactory`].
pub struct MockBackend {
    ports: Vec<MockPort>,
}

impl MockBackend {
    /// Builds the backend for `mode`: `All` = `MOCK0`..`MOCK3` playing the four
    /// scenarios in order; a single scenario = one port named `MOCK0` (§5.7).
    pub fn for_mode(mode: super::factory::MockMode) -> Self {
        let ports = match mode {
            super::factory::MockMode::Off => Vec::new(),
            super::factory::MockMode::All => MockScenario::ALL
                .iter()
                .enumerate()
                .map(|(index, scenario)| Self::port(index, *scenario))
                .collect(),
            super::factory::MockMode::Single(scenario) => vec![Self::port(0, scenario)],
        };
        Self { ports }
    }

    fn port(index: usize, scenario: MockScenario) -> MockPort {
        let shared = Arc::new(PlayerShared::default());
        // Ports start plugged at 1× (the unplugged state is a *scenario
        // event*, not a default; AtomicBool::default() is false and f64's is
        // 0.0 — set both explicitly).
        shared.plug.store(true, Ordering::Relaxed);
        *shared.speed.write().expect("mock speed lock") = 1.0;
        MockPort {
            name: format!("MOCK{index}"),
            scenario,
            shared,
            lifecycle_started: OnceLock::new(),
        }
    }

    /// The playback control of mock port `name`, for driving it before/without
    /// a session (the demo harness paths). `None` when the name is not a mock
    /// port of this backend.
    pub fn control_for(&self, name: &str) -> Option<MockControl> {
        self.ports
            .iter()
            .find(|port| port.name == name)
            .map(|port| MockControl {
                scenario: port.scenario,
                shared: Arc::clone(&port.shared),
            })
    }
}

impl SerialBackend for MockBackend {
    fn enumerate(&self) -> Result<Vec<PortInfo>> {
        Ok(self
            .ports
            .iter()
            .filter(|port| port.shared.plug.load(Ordering::Relaxed))
            .map(MockPort::info)
            .collect())
    }

    fn open(&self, name: &str, _config: &PortConfig) -> Result<OpenPort> {
        let Some(port) = self.ports.iter().find(|port| port.name == name) else {
            return Err(CoreError::NotOpen(name.to_string()));
        };
        if !port.shared.plug.load(Ordering::Relaxed) {
            return Err(CoreError::NotOpen(format!("{name} is unplugged")));
        }

        // The lifecycle thread (Unplug scenario only): this open re-arms it.
        // The arm epoch is captured BEFORE the bump: a first open must arm the
        // freshly spawned thread (it waits for an epoch newer than its birth).
        let epoch_before_open = port.shared.open_epoch.load(Ordering::Relaxed);
        port.shared.open_epoch.fetch_add(1, Ordering::Relaxed);
        port.shared.epoch_cond.notify_all();
        if port.scenario == MockScenario::Unplug && port.lifecycle_started.set(()).is_ok() {
            let shared = Arc::clone(&port.shared);
            let name = port.name.clone();
            std::thread::Builder::new()
                .name(format!("rcom-mock-{name}-lifecycle"))
                .spawn(move || lifecycle(name, shared, epoch_before_open))
                .expect("spawn rcom-mock lifecycle");
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel::<ReadEvent>(256);
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(64);

        let shared = Arc::clone(&port.shared);
        let scenario = port.scenario;
        let port_name = port.name.clone();
        let player_cancel = cancel.clone();
        std::thread::Builder::new()
            .name(format!("rcom-mock-{port_name}"))
            .spawn(move || play(scenario, port_name, shared, write_rx, event_tx, player_cancel))
            .expect("spawn rcom-mock player");

        Ok(OpenPort {
            events: event_rx,
            writer: PortWriter { tx: write_tx },
            cancel,
            control: Some(MockControl {
                scenario: port.scenario,
                shared: Arc::clone(&port.shared),
            }),
        })
    }
}

/// The unplug/reappear lifecycle of an `Unplug`-scenario port (§6.5): armed by
/// every open, it unplugs after [`UNPLUG_AFTER_MS`] (or an injected trigger)
/// and reappears [`UNPLUG_REAPPEAR_MS`] later — the auto-reconnect window the
/// reconnect machine races to. Runs for the port's lifetime, across sessions.
fn lifecycle(name: String, shared: Arc<PlayerShared>, mut armed_epoch: u64) {
    loop {
        // Wait for an open newer than `armed_epoch` (the cycle arms per open;
        // the birth epoch makes a first open arm the fresh thread).
        loop {
            if shared.open_epoch.load(Ordering::Relaxed) > armed_epoch {
                break;
            }
            let guard = shared.epoch_mutex.lock().expect("lifecycle epoch lock");
            let _result = shared.epoch_cond.wait_timeout_while(
                guard,
                Duration::from_millis(200),
                |_: &mut ()| shared.open_epoch.load(Ordering::Relaxed) <= armed_epoch,
            );
            if shared.open_epoch.load(Ordering::Relaxed) > armed_epoch {
                break;
            }
        }

        let armed = shared.open_epoch.load(Ordering::Relaxed);
        // Armed: wait for the scripted unplug, an injected trigger, or the
        // plug being pulled by some other path.
        let mut waited = Duration::ZERO;
        loop {
            if shared.trig_unplug.swap(false, Ordering::Relaxed) {
                tracing::info!(port = %name, "mock unplug triggered");
                break;
            }
            if !shared.plug.load(Ordering::Relaxed) {
                break; // already unplugged (a direct injection)
            }
            if waited >= Duration::from_millis(UNPLUG_AFTER_MS) {
                tracing::info!(port = %name, "mock scripted unplug");
                break;
            }
            shared.sleep_scaled(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
        shared.plug.store(false, Ordering::Relaxed);

        // Reappear countdown (5 s default), or an injected Reappear.
        let mut waited = Duration::ZERO;
        loop {
            if shared.trig_reappear.swap(false, Ordering::Relaxed) {
                break;
            }
            if waited >= Duration::from_millis(UNPLUG_REAPPEAR_MS) {
                break;
            }
            shared.sleep_scaled(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
        shared.plug.store(true, Ordering::Relaxed);
        tracing::info!(port = %name, "mock port reappeared (auto-reconnect window open)");
        // The next cycle arms on the next open (an epoch newer than `armed`).
        armed_epoch = armed;
    }
}

/// The player thread: walks the scenario's timeline on a virtual clock. Ends
/// with `Closed` (cancel/close) or `Error` (unplug / injected error) — like a
/// real reader.
fn play(
    scenario: MockScenario,
    port: String,
    shared: Arc<PlayerShared>,
    mut writes: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<ReadEvent>,
    cancel: Arc<AtomicBool>,
) {
    let mut t_ms: f64 = 0.0;
    let mut last = Instant::now();
    let mut generation = shared.playback_generation.load(Ordering::Relaxed);
    // The per-timeline state (locals, so a restart resets them wholesale).
    let mut due_echoes: VecDeque<(f64, Vec<u8>)> = VecDeque::new();
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D ^ port_hash(&port);
    let mut split_tail: Option<Vec<u8>> = None;
    let mut next_noisy: f64 = 0.0;
    let mut next_framed: f64 = 0.0;
    let mut next_unplug: f64 = 0.0;
    let mut frame_seq: u16 = 0;
    let mut banner_armed = false;

    // The +100 ms open note (§6.2–6.5's banner row).
    let banner = |events: &mpsc::Sender<ReadEvent>| {
        let text = match scenario {
            MockScenario::Echo => "rcom mock echo device v1",
            MockScenario::Noisy => "rcom mock noisy sensor v1 (T/P frames, garbled bursts)",
            MockScenario::Framed => "rcom mock framed protocol v1 (AA 55 frames)",
            MockScenario::Unplug => "sensor link up",
        };
        let _ = events.blocking_send(ReadEvent::Note(format!("{port}: {text}")));
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = events.blocking_send(ReadEvent::Closed);
            return;
        }
        if !shared.plug.load(Ordering::Relaxed) {
            let _ = events.blocking_send(ReadEvent::Error(format!("{port} was unplugged")));
            return;
        }

        // Restart: reset the timeline (the open banner re-fires).
        let current = shared.playback_generation.load(Ordering::Relaxed);
        if current != generation {
            generation = current;
            t_ms = 0.0;
            due_echoes.clear();
            split_tail = None;
            next_noisy = 0.0;
            next_framed = 0.0;
            next_unplug = 0.0;
            banner_armed = false;
            last = Instant::now();
        }

        // Queued triggers: an injected error ends the stream (the session dies
        // like on a driver failure); a corrupt-frame flag marks the next frame.
        loop {
            let trigger = shared.triggers.lock().expect("mock trigger lock").pop_front();
            match trigger {
                Some(MockTrigger::Error) => {
                    let _ = events.blocking_send(ReadEvent::Error("injected error".into()));
                    return;
                }
                Some(MockTrigger::CorruptFrame) => {
                    shared.corrupt_next.store(true, Ordering::Relaxed);
                }
                _ => break,
            }
        }

        // Injected RX goes out immediately (playback state notwithstanding).
        loop {
            let injected = shared.inject.lock().expect("mock inject lock").pop_front();
            match injected {
                Some(bytes) => {
                    if events.blocking_send(ReadEvent::Data(bytes)).is_err() {
                        return; // the session is gone
                    }
                }
                None => break,
            }
        }

        // Pause / step: hold the virtual clock unless a step was requested.
        if shared.paused.load(Ordering::Relaxed) {
            let steps = shared.steps.swap(0, Ordering::Relaxed);
            if steps == 0 {
                last = Instant::now();
                std::thread::sleep(PLAYER_TICK);
                continue;
            }
            t_ms += STEP_MS * steps as f64;
        } else {
            let dt = last.elapsed().as_secs_f64() * 1000.0 * shared.speed();
            last = Instant::now();
            t_ms += dt;
        }

        // The +100 ms banner of every timeline.
        if !banner_armed && t_ms >= 100.0 {
            banner_armed = true;
            banner(&events);
        }

        // Scenario timelines.
        match scenario {
            MockScenario::Echo => {}
            MockScenario::Noisy => {
                noisy_timeline(&mut t_ms, &mut rng, &mut split_tail, &mut next_noisy, &events)
            }
            MockScenario::Framed => {
                framed_timeline(&shared, &mut t_ms, &mut frame_seq, &mut next_framed, &events)
            }
            MockScenario::Unplug => unplug_timeline(&mut t_ms, &mut next_unplug, &events),
        }

        // Echo the written bytes (scenario A): due at virtual t + 30 ms. Other
        // scenarios consume (and drop) writes — their data is generated.
        loop {
            match writes.try_recv() {
                Ok(bytes) => {
                    if scenario == MockScenario::Echo {
                        due_echoes.push_back((t_ms + ECHO_DELAY_MS, bytes));
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    let _ = events.blocking_send(ReadEvent::Closed);
                    return;
                }
            }
        }
        if scenario == MockScenario::Echo {
            while due_echoes.front().is_some_and(|(due, _)| *due <= t_ms) {
                let (_, bytes) = due_echoes.pop_front().expect("front checked");
                if events.blocking_send(ReadEvent::Data(bytes)).is_err() {
                    return;
                }
            }
        }

        std::thread::sleep(PLAYER_TICK);
    }
}

fn port_hash(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x1000_0000_01b3)
    })
}

/// Scenario B (§6.3): every 500 ms a sensor frame; p≈0.2 per window a 32-byte
/// garbled burst (runs of 0x80–0xFF); p≈0.05 per window a frame split
/// mid-UTF-8 across two chunks.
fn noisy_timeline(
    t_ms: &mut f64,
    rng: &mut u64,
    split_tail: &mut Option<Vec<u8>>,
    next: &mut f64,
    events: &mpsc::Sender<ReadEvent>,
) {
    while *t_ms >= *next {
        // Finish a pending split frame first (its tail is due now).
        if let Some(tail) = split_tail.take() {
            let _ = events.blocking_send(ReadEvent::Data(tail));
        }
        // Garbled burst (p≈0.2).
        if next_rng(rng) < 0.2 {
            let _ = events.blocking_send(ReadEvent::Data(garbled_burst(rng)));
        }
        // Split frame (p≈0.05): a multi-byte UTF-8 char cut in half.
        if next_rng(rng) < 0.05 {
            let (head, tail) = split_frame(rng);
            let _ = events.blocking_send(ReadEvent::Data(head));
            *split_tail = Some(tail);
        }
        // The regular sensor frame.
        let frame = format!("{:04},T=23.4,P=1013.2\r\n", *next as u64).into_bytes();
        let _ = events.blocking_send(ReadEvent::Data(frame));
        *next += 500.0;
    }
}

/// Scenario C (§6.4, reconciled with the shipped decoder's byte contract —
/// `core::plot::frames`): one 3-frame chunk per 100 ms (3 curves at ~10 Hz);
/// the `CorruptFrame` trigger corrupts exactly one frame.
fn framed_timeline(
    shared: &Arc<PlayerShared>,
    t_ms: &mut f64,
    frame_seq: &mut u16,
    next: &mut f64,
    events: &mpsc::Sender<ReadEvent>,
) {
    while *t_ms >= *next {
        let mut chunk: Vec<u8> = Vec::with_capacity(21);
        let corrupt = shared.corrupt_next.swap(false, Ordering::Relaxed);
        for channel in 0..3u8 {
            let value = frame_value(*frame_seq, channel);
            let frame = if corrupt && channel == 2 {
                crate::core::plot::frames::build_corrupt_frame(channel, value)
            } else {
                crate::core::plot::frames::build_frame(channel, value)
            };
            chunk.extend_from_slice(&frame);
            *frame_seq = frame_seq.wrapping_add(1);
        }
        let _ = events.blocking_send(ReadEvent::Data(chunk));
        *next += 100.0;
    }
}

/// Deterministic demo values: a slow sawtooth per channel (the decoder plots
/// visible, distinguishable curves).
fn frame_value(seq: u16, channel: u8) -> i16 {
    let base = (seq as i32 * 7 + channel as i32 * 40) % 900;
    (base as i16) + 100 * channel as i16
}

/// Scenario D (§6.5): a status line every 1 s (the unplug itself rides the
/// backend lifecycle thread, not this data timeline).
fn unplug_timeline(t_ms: &mut f64, next: &mut f64, events: &mpsc::Sender<ReadEvent>) {
    while *t_ms >= *next {
        let ms = *next as u64;
        let temp = 20 + (ms / 1000) % 8;
        let status = format!("link ok, up {ms} ms, T={temp}.1\r\n").into_bytes();
        let _ = events.blocking_send(ReadEvent::Data(status));
        *next += 1000.0;
    }
}

/// Fixed-seed LCG step → f64 in [0, 1).
fn next_rng(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*state >> 33) as f64) / (1u64 << 31) as f64
}

/// A 32-byte burst of high bytes (runs of 0x80–0xFF): garbage the UTF-8 decoder
/// flags as garbled.
fn garbled_burst(rng: &mut u64) -> Vec<u8> {
    (0..32)
        .map(|_| 0x80u8 | (next_rng(rng) * 127.0) as u8)
        .collect()
}

/// A frame split mid-UTF-8: a multi-byte character cut between its bytes, so
/// the first chunk ends inside the character (the chunk-boundary decode case).
fn split_frame(rng: &mut u64) -> (Vec<u8>, Vec<u8>) {
    let text = match (next_rng(rng) * 3.0) as u32 {
        0 => "温度=25.6\r\n",
        1 => "圧力=1013\r\n",
        _ => "OK、継続\r\n",
    };
    let bytes = text.as_bytes().to_vec();
    // Split inside the first multi-byte character (between its lead and a
    // continuation byte) — position 3 for every sample here.
    let at = bytes
        .iter()
        .position(|byte| byte & 0xC0 == 0x80)
        .unwrap_or(1)
        .max(1);
    (bytes[..at].to_vec(), bytes[at..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::plot::frames::FrameParser;
    use crate::core::port::factory::MockMode;

    fn open_single(scenario: MockScenario) -> (MockBackend, OpenPort, MockControl) {
        let backend = MockBackend::for_mode(MockMode::Single(scenario));
        let opened = backend.open("MOCK0", &PortConfig::new(115_200)).expect("open");
        let control = opened.control.clone().expect("mock control");
        (backend, opened, control)
    }

    /// The mock lists exactly the configured ports, marked as mock scenarios.
    #[test]
    fn mock_lists_its_ports() {
        let backend = MockBackend::for_mode(MockMode::Single(MockScenario::Echo));
        let ports = backend.enumerate().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, "MOCK0");
        assert_eq!(ports[0].kind, PortKind::Mock(MockScenario::Echo));
        assert!(ports[0].is_mock());

        let all = MockBackend::for_mode(MockMode::All);
        let names: Vec<String> = all
            .enumerate()
            .unwrap()
            .into_iter()
            .map(|info| info.name)
            .collect();
        assert_eq!(names, ["MOCK0", "MOCK1", "MOCK2", "MOCK3"]);
    }

    /// Scenario A: open, write `ping`, the same bytes come back as RX after the
    /// echo delay (the +100 ms banner rides the note stream, not as data).
    #[tokio::test]
    async fn echo_scenario_echoes_writes() {
        let (_backend, opened, _control) = open_single(MockScenario::Echo);
        let mut events = opened.events;
        opened.writer.try_write(b"ping".to_vec()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut echoed = false;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
                Ok(Some(ReadEvent::Data(bytes))) if bytes == b"ping" => {
                    echoed = true;
                    break;
                }
                Ok(Some(ReadEvent::Data(_))) => {}
                Ok(Some(ReadEvent::Note(_))) => {} // the +100 ms banner
                Ok(Some(event)) => panic!("unexpected event: {event:?}"),
                Ok(None) => panic!("stream ended early"),
                Err(_) => {}
            }
        }
        assert!(echoed, "the echo never arrived");

        // Close cancels within a tick (close never hangs).
        opened.cancel.store(true, Ordering::Relaxed);
        drop(opened.writer);
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("close timed out")
            .expect("stream should end");
        assert!(matches!(event, ReadEvent::Closed | ReadEvent::Data(_)));
    }

    /// Scenario B: frames arrive at the 500 ms cadence; the garbled bursts are
    /// invalid UTF-8 (the app-side `had_errors` case); the banner is a note.
    #[tokio::test]
    async fn noisy_scenario_streams_frames_and_a_note() {
        let (_backend, opened, _control) = open_single(MockScenario::Noisy);
        let mut events = opened.events;

        let deadline = Instant::now() + Duration::from_secs(4);
        let (mut frames, mut notes) = (0, 0);
        while Instant::now() < deadline && frames < 3 {
            match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
                Ok(Some(ReadEvent::Data(bytes))) => {
                    if std::str::from_utf8(&bytes).is_ok() {
                        frames += 1;
                    }
                }
                Ok(Some(ReadEvent::Note(_))) => notes += 1,
                Ok(Some(event)) => panic!("unexpected event: {event:?}"),
                Ok(None) => panic!("stream ended early"),
                Err(_) => {}
            }
        }
        assert!(frames >= 2, "expected sensor frames, got {frames}");
        assert!(notes >= 1, "expected the open note");

        // Garbled bursts are probabilistic (p≈0.2/window); verify the burst
        // builder directly (deterministic).
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let burst = garbled_burst(&mut rng);
        assert!(std::str::from_utf8(&burst).is_err(), "the burst must be invalid UTF-8");
        assert_eq!(burst.len(), 32);
    }

    /// Scenario C: the frames parse against the shipped decoder's reference
    /// parser, and the `CorruptFrame` trigger corrupts exactly one frame
    /// (skipped by the parser, never a crash).
    #[tokio::test]
    async fn framed_scenario_streams_parsable_frames() {
        let (_backend, opened, control) = open_single(MockScenario::Framed);
        assert_eq!(control.scenario(), MockScenario::Framed);
        let mut events = opened.events;

        let mut bytes = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(700);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(150), events.recv()).await {
                Ok(Some(ReadEvent::Data(chunk))) => bytes.extend_from_slice(&chunk),
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream ended early"),
                Err(_) => {}
            }
        }
        let stats = FrameParser::default().feed(&bytes);
        assert!(
            stats.frames.len() >= 15,
            "expected ~21 frames in 700 ms, got {}",
            stats.frames.len()
        );
        assert_eq!(stats.skipped, 0, "clean frames must parse");
        let channels: std::collections::HashSet<u8> =
            stats.frames.iter().map(|(ch, _)| *ch).collect();
        assert_eq!(channels, [0u8, 1, 2].into_iter().collect(), "3 curves' channels");

        // The corrupt trigger: exactly one frame goes bad.
        control.trigger(MockTrigger::CorruptFrame);
        let mut after = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(150), events.recv()).await {
                Ok(Some(ReadEvent::Data(chunk))) => after.extend_from_slice(&chunk),
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream ended early"),
                Err(_) => {}
            }
        }
        let stats = FrameParser::default().feed(&after);
        assert!(stats.frames.len() >= 8, "{}", stats.frames.len());
        // One corrupt frame: its 7 bytes leave the parser byte-by-byte (the
        // skip-and-resync strategy), so `skipped` counts the dropped bytes.
        assert_eq!(stats.skipped, 7, "exactly one corrupt frame's bytes expected");
    }

    /// Playback controls: pause freezes the stream, inject_rx emits while
    /// paused, resume continues, speed scales the cadence, restart re-fires
    /// the banner.
    #[tokio::test]
    async fn playback_controls_pause_inject_resume_restart() {
        let (_backend, opened, control) = open_single(MockScenario::Noisy);
        let mut events = opened.events;

        // Drain until the banner note (the timeline's first item).
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(150), events.recv()).await {
                Ok(Some(ReadEvent::Note(_))) => break,
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream ended"),
                Err(_) => {}
            }
        }

        // Pause: no data for 400 ms.
        control.pause(true);
        assert!(control.paused());
        let mut quiet = true;
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
                Ok(Some(ReadEvent::Data(_))) => {
                    quiet = false;
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream ended"),
                Err(_) => {}
            }
        }
        assert!(quiet, "the stream must freeze while paused");

        // Inject while paused: the chunk still goes out.
        control.inject_rx(b"injected".to_vec());
        let got = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match events.recv().await {
                    Some(ReadEvent::Data(bytes)) => break bytes,
                    Some(_) => continue,
                    None => panic!("stream ended"),
                }
            }
        })
        .await
        .expect("injected rx");
        assert_eq!(got, b"injected");

        // Resume + speed: at 8× frames come much faster than 500 ms apart.
        control.pause(false);
        control.set_speed(8.0);
        assert_eq!(control.speed(), 8.0);
        let mut count = 0;
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(150), events.recv()).await {
                Ok(Some(ReadEvent::Data(_))) => count += 1,
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream ended"),
                Err(_) => {}
            }
        }
        assert!(count >= 4, "8× speed must stream quickly (got {count})");

        // Restart: the banner note re-fires.
        control.restart();
        let mut saw_note = false;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !saw_note {
            match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
                Ok(Some(ReadEvent::Note(_))) => saw_note = true,
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream ended"),
                Err(_) => {}
            }
        }
        assert!(saw_note, "restart must re-fire the open note");
    }

    /// Trigger(Unplug) kills the live session with an error and delists the
    /// port; trigger(Reappear) brings it back (AC6's cycle, driven by hand).
    #[tokio::test]
    async fn unplug_trigger_kills_and_reappear_restores() {
        let (backend, opened, control) = open_single(MockScenario::Echo);
        let mut events = opened.events;

        assert_eq!(backend.enumerate().unwrap().len(), 1);
        control.trigger(MockTrigger::Unplug);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
                Ok(Some(ReadEvent::Error(message))) => {
                    assert!(message.contains("unplugged"), "{message}");
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("stream ended without the unplug error"),
                Err(_) => panic!("unplug not noticed in time"),
            }
        }
        assert!(
            backend.enumerate().unwrap().is_empty(),
            "an unplugged port must delist"
        );
        assert!(backend.open("MOCK0", &PortConfig::new(115_200)).is_err());

        control.trigger(MockTrigger::Reappear);
        assert_eq!(backend.enumerate().unwrap().len(), 1);
        assert!(backend.open("MOCK0", &PortConfig::new(115_200)).is_ok());
    }

    /// The Unplug scenario's lifecycle: scripted unplug → reappear, both
    /// observed on the listing (compressed by the 8× speed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unplug_scenario_reappears_after_scripted_unplug() {
        let backend = MockBackend::for_mode(MockMode::Single(MockScenario::Unplug));
        let control = backend.control_for("MOCK0").expect("backend-level control");
        // Compress the 10 s → 5 s cycle to ~1.25 s + 0.63 s.
        control.set_speed(8.0);
        let opened = backend.open("MOCK0", &PortConfig::new(115_200)).unwrap();
        let _ = opened;

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut unplugged = false;
        while Instant::now() < deadline {
            if backend.enumerate().unwrap().is_empty() {
                unplugged = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(unplugged, "the scripted unplug must delist the port");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut replugged = false;
        while Instant::now() < deadline {
            if !backend.enumerate().unwrap().is_empty() {
                replugged = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(replugged, "the port must reappear for auto-reconnect");
    }

    /// Speed clamping keeps the documented 0.25×–8× window.
    #[test]
    fn speed_clamps_to_the_documented_window() {
        let backend = MockBackend::for_mode(MockMode::Single(MockScenario::Echo));
        let control = backend.control_for("MOCK0").expect("control");
        control.set_speed(100.0);
        assert_eq!(control.speed(), 8.0);
        control.set_speed(0.01);
        assert_eq!(control.speed(), 0.25);
    }

    /// The split frame's halves join back into the original frame (the
    /// chunk-boundary decode case scenario B demonstrates).
    #[test]
    fn split_frame_halves_join() {
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20 {
            let (head, tail) = split_frame(&mut rng);
            let mut joined = head.clone();
            joined.extend_from_slice(&tail);
            assert!(
                std::str::from_utf8(&joined).is_ok(),
                "halves must join to valid UTF-8"
            );
        }
    }

    /// Scenario C's frames really decode (guards a format drift between the
    /// mock and the shipped decoder's reference parser).
    #[test]
    fn framed_values_are_deterministic() {
        assert_eq!(frame_value(0, 0), frame_value(0, 0));
        assert_ne!(frame_value(0, 0), frame_value(0, 1));
        let mut parser = FrameParser::default();
        let mut chunk = Vec::new();
        for seq in 0..4u16 {
            for channel in 0..3u8 {
                chunk.extend_from_slice(&crate::core::plot::frames::build_frame(
                    channel,
                    frame_value(seq, channel),
                ));
            }
        }
        let stats = parser.feed(&chunk);
        assert_eq!(stats.frames.len(), 12);
        assert_eq!(stats.skipped, 0);
    }
}
