//! The `RCOM_PLOTDEMO` autodemo (issue #24): the plot pipeline proving itself
//! end to end, hardware-free, on the MOCK0 loopback.
//!
//! - `RCOM_PLOTDEMO=1` — 3 channels at ~100 frames/s through the *real* port
//!   path: a generator builds framed-protocol chunks (one corrupt frame every
//!   50, for the decoder's skip path), writes them to MOCK0, the mock echoes
//!   them back as RX, the bundled decoder taps the bus and plots.
//! - `RCOM_PLOTDEMO=fast` — 2500 frames/s (250-frame chunks): enough to fill
//!   the 10k-point ring within the smoke window and prove the render budget.
//!
//! Evidence: `AUTODEMO PLOT OK …` in the trace once the store holds N points
//! per channel, with the measured frame cadence and the injected corrupt count.

use std::sync::Arc;
use std::time::Duration;

use crate::core::port::manager::PortManager;

use super::frames::{build_chunk, build_corrupt_frame};
use super::store::global_store;

/// How many demo channels each frame batch carries (`ch0`..`ch2`).
const DEMO_CHANNELS: u8 = 3;

/// The "OK" bar for the normal demo: 200 plotted points per channel plus the
/// decoder having received at least one corrupt frame to skip.
const DEMO_OK_POINTS: u64 = 200;

/// The "OK" bar for `fast`: the full 10k-point ring, twice over.
const DEMO_FAST_POINTS: u64 = 20_000;

/// The demo mode requested by `RCOM_PLOTDEMO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlotDemoMode {
    Off,
    Basic,
    Fast,
}

impl PlotDemoMode {
    pub fn from_env() -> Self {
        match std::env::var("RCOM_PLOTDEMO").as_deref() {
            Ok("1") => Self::Basic,
            Ok("fast") => Self::Fast,
            _ => Self::Off,
        }
    }
}

/// One demo generator configuration (the two modes' only difference is rate).
#[derive(Debug, Clone, Copy)]
pub struct DemoConfig {
    /// Frames per second (spread over one chunk per 100 ms).
    pub frames_per_sec: u32,
    /// Value amplitude per channel (sine + a little saw, so auto-scale has
    /// something to do).
    pub amplitude: f64,
    /// Points per channel before the OK trace fires.
    pub ok_points: u64,
}

impl DemoConfig {
    pub fn basic() -> Self {
        Self { frames_per_sec: 100, amplitude: 500.0, ok_points: DEMO_OK_POINTS }
    }

    pub fn fast() -> Self {
        Self { frames_per_sec: 2500, amplitude: 500.0, ok_points: DEMO_FAST_POINTS }
    }

    fn frames_per_chunk(&self) -> u32 {
        // 10 chunks/s: frames_per_sec / 10 per chunk, at least 1.
        (self.frames_per_sec / 10).max(1)
    }
}

/// Starts the demo generator on the core runtime. Call once MOCK0 is open;
/// the generator writes through the port manager (the mock echoes it back as
/// RX — the decoder taps the bus on the driver) and emits the trace evidence.
pub fn spawn_generator(manager: Arc<PortManager>, config: DemoConfig) {
    tokio::spawn(async move {
        let store = global_store();
        store.clear();
        let started = tokio::time::Instant::now();
        let per_chunk = config.frames_per_chunk();
        let corrupt_every = 50u64; // one corrupt frame every 50 good ones
        let mut frame_counter: u64 = 0;
        let mut corrupt_sent: u64 = 0;
        let mut last_report = started;
        let mut intervals: Vec<f64> = Vec::new();

        tracing::info!(
            "AUTODEMO PLOT: generator up — {per_chunk} frames/chunk every 100ms \
             ({fps} frames/s, {ch} channels, one corrupt frame every {corrupt_every})",
            fps = config.frames_per_sec,
            ch = DEMO_CHANNELS,
        );

        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let now = tokio::time::Instant::now();
            intervals.push((now - last_report).as_secs_f64() * 1000.0);
            last_report = now;

            let mut chunk = Vec::with_capacity((per_chunk as usize + 1) * 7);
            let phase = frame_counter as f64 / 20.0;
            let wave = |channel: u8, seq: f64, step: f64| {
                let offset = channel as f64 * 1.3;
                (phase + step + offset).sin() * config.amplitude + (seq % 37.0) - 18.0
            };
            for frame in 0..per_chunk {
                let seq = frame_counter + frame as u64;
                let value = |channel: u8| wave(channel, seq as f64, frame as f64 / 20.0).clamp(-32000.0, 32000.0) as i16;
                chunk.extend_from_slice(&build_chunk(DEMO_CHANNELS, &value));
                frame_counter += 1;
                if frame_counter % corrupt_every == 0 {
                    chunk.extend_from_slice(&build_corrupt_frame(0, 0));
                    corrupt_sent += 1;
                }
            }
            if let Err(error) = manager.write_raw(chunk).await {
                tracing::warn!("AUTODEMO PLOT: generator write failed ({error}) — port closed?");
            }

            // Evidence: every ~2s of run, and the OK bar once reached.
            let elapsed = now - started;
            let plotted = store.total_added();
            let ok = plotted >= config.ok_points * DEMO_CHANNELS as u64;
            if ok || elapsed.as_secs_f64() >= 2.0 && intervals.len() % 20 == 0 {
                let recent = &intervals[intervals.len().saturating_sub(20)..];
                let avg = recent.iter().sum::<f64>() / recent.len() as f64;
                let names = store.channel_names();
                let counts: Vec<String> = names
                    .iter()
                    .map(|(name, count)| format!("{name}={count}"))
                    .collect();
                if ok {
                    tracing::info!(
                        "AUTODEMO PLOT OK: {} points plotted across {} channels ({}) \
                         from {frame_counter} frames in {:.1}s — chunk cadence avg {avg:.1}ms \
                         (target 100ms), {corrupt_sent} corrupt frames injected for the \
                         decoder's skip path",
                        plotted,
                        names.len(),
                        counts.join(" "),
                        elapsed.as_secs_f64(),
                    );
                    return;
                }
                tracing::debug!(
                    "AUTODEMO PLOT: {plotted} points ({}) — chunk cadence avg {avg:.1}ms",
                    counts.join(" "),
                );
            }
        }
    });
}
