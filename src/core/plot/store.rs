//! The plot store (issue #24): the process-global set of named value channels,
//! each a bounded [`PointRing`].
//!
//! This is the core-side decoupling that keeps the UI stall-free at live data
//! rates: producers (the Lua decoders, through the `rcom.ui.add_point` API)
//! append points here at wire rate; the UI **pulls** a snapshot on its pump
//! tick (~30 fps) and never sees a message per point. Nothing plot-shaped
//! crosses the bus per point — only the low-rate decoder lifecycle notices
//! ([`crate::core::bus::PlotEvent`]).
//!
//! Pause semantics (docs/ui-drafts/05-contract.md §5.4): pausing freezes the
//! *plot* — `add_point` becomes a no-op, so decoders idle out — while the byte
//! stream keeps flowing (the log view keeps filling). While paused, sampling
//! anchors to the newest stored point instead of wall-clock "now", so the
//! picture truly stands still.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use chrono::Utc;

use super::ring::{PlotPoint, PointRing};

/// The default ring capacity per channel: the AC's "must not stall at 10k
/// points per channel" budget.
pub const DEFAULT_MAX_POINTS: usize = 10_000;

/// One channel's snapshot for the UI: the name plus its points (oldest →
/// newest), already decimated by [`super::view`].
#[derive(Debug, Clone)]
pub struct SeriesSnapshot {
    pub name: String,
    pub points: Vec<(f64, f64)>,
}

/// A whole-store snapshot: one pull of every channel, sampled in one lock.
/// `anchor_ms` is the time anchor the window was cut at (wall-clock now, or the
/// newest point while paused) — the render's auto-scale reuses it so the drawn
/// window is exactly the sampled one.
#[derive(Debug, Clone, Default)]
pub struct PlotSnapshot {
    pub anchor_ms: f64,
    pub series: Vec<SeriesSnapshot>,
}

#[derive(Debug)]
struct StoreInner {
    channels: BTreeMap<String, PointRing>,
    paused: bool,
    /// Cumulative points ever added (not capped by the rings) — the demo's
    /// "N points rendered" evidence reads this.
    total_added: u64,
}

/// The named-channel plot store. Cheap to clone; shared through
/// [`global_store`].
pub struct PlotStore {
    inner: Mutex<StoreInner>,
    max_points: usize,
    version: AtomicU64,
}

impl Default for PlotStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_POINTS)
    }
}

impl PlotStore {
    pub fn new(max_points: usize) -> Self {
        Self {
            inner: Mutex::new(StoreInner {
                channels: BTreeMap::new(),
                paused: false,
                total_added: 0,
            }),
            max_points: max_points.max(1),
            version: AtomicU64::new(0),
        }
    }

    /// Monotone store revision: bumps on every mutation, so the UI pump can
    /// skip sampling when nothing changed.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Appends a point at the current wall clock. No-op while paused.
    pub fn add_point(&self, channel: &str, value: f64) {
        self.add_point_at(channel, Utc::now().timestamp_millis() as f64, value);
    }

    /// Appends a point with an explicit epoch-millisecond timestamp. No-op
    /// while paused. Non-finite values are dropped (a corrupt decode must not
    /// poison the render math).
    pub fn add_point_at(&self, channel: &str, t_ms: f64, value: f64) {
        if !t_ms.is_finite() || !value.is_finite() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.paused {
            return;
        }
        let ring = inner
            .channels
            .entry(channel.to_string())
            .or_insert_with(|| PointRing::new(self.max_points));
        ring.push(PlotPoint { t: t_ms, v: value });
        inner.total_added += 1;
        drop(inner);
        self.bump();
    }

    /// Pauses/unpauses point ingestion; returns the previous state. The byte
    /// stream itself is untouched (contract §5.4).
    pub fn set_paused(&self, paused: bool) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let was = inner.paused;
        inner.paused = paused;
        drop(inner);
        if was != paused {
            self.bump();
        }
        was
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().unwrap().paused
    }

    /// Drops every channel's points (the plot page's Clear).
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        for ring in inner.channels.values_mut() {
            ring.clear();
        }
        drop(inner);
        self.bump();
    }

    /// Drops one channel's points; unknown channels are a no-op.
    pub fn clear_channel(&self, channel: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ring) = inner.channels.get_mut(channel) {
            ring.clear();
        }
        drop(inner);
        self.bump();
    }

    /// The channel names with their stored point counts, sorted by name.
    pub fn channel_names(&self) -> Vec<(String, usize)> {
        let inner = self.inner.lock().unwrap();
        inner
            .channels
            .iter()
            .map(|(name, ring)| (name.clone(), ring.len()))
            .collect()
    }

    /// Cumulative points ever added across all channels.
    pub fn total_added(&self) -> u64 {
        self.inner.lock().unwrap().total_added
    }

    /// Pulls the latest `window_s` seconds of every channel, decimated to at
    /// most `max_points_per_series` points per series (the UI calls this once
    /// per pump tick with ~the plot's pixel width).
    ///
    /// While paused the window anchors to the newest stored point (a frozen
    /// picture), otherwise to wall-clock now.
    pub fn sample(&self, window_s: f64, max_points_per_series: usize) -> PlotSnapshot {
        let inner = self.inner.lock().unwrap();
        let now_ms = match inner.channels.values().filter_map(|ring| ring.last()).max_by(|a, b| {
            a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal)
        }) {
            Some(last) if inner.paused => last.t,
            _ => Utc::now().timestamp_millis() as f64,
        };
        let cutoff = now_ms - window_s.abs() * 1000.0;
        let series = inner
            .channels
            .iter()
            .map(|(name, ring)| SeriesSnapshot {
                name: name.clone(),
                points: super::view::decimate(
                    &ring
                        .iter()
                        .filter(|point| point.t >= cutoff)
                        .map(|point| (point.t, point.v))
                        .collect::<Vec<_>>(),
                    max_points_per_series,
                ),
            })
            .filter(|series| !series.points.is_empty())
            .collect();
        PlotSnapshot { anchor_ms: now_ms, series }
    }
}

static GLOBAL_STORE: OnceLock<PlotStore> = OnceLock::new();

/// The process-global store: producers (Lua decoders, the demo generator) and
/// the UI all share it. Created on first use.
pub fn global_store() -> &'static PlotStore {
    GLOBAL_STORE.get_or_init(PlotStore::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PlotStore {
        PlotStore::new(4)
    }

    /// Points land in named rings, capped, newest last (the AC's named
    /// channels with bounded history). Timestamps ride the wall clock the way
    /// real producers stamp them.
    #[test]
    fn add_point_fills_named_channels() {
        let store = store();
        let base = chrono::Utc::now().timestamp_millis() as f64;
        for i in 0..6 {
            store.add_point_at("temperature", base + i as f64, i as f64);
        }
        store.add_point_at("pressure", base + 5.0, 42.0);

        let names = store.channel_names();
        assert_eq!(names, vec![("pressure".to_string(), 1), ("temperature".to_string(), 4)]);
        assert_eq!(store.total_added(), 7);

        let snapshot = store.sample(1_000_000.0, 100);
        let temp = snapshot
            .series
            .iter()
            .find(|series| series.name == "temperature")
            .expect("temperature series");
        assert_eq!(temp.points.len(), 4);
        assert_eq!(temp.points[3], (base + 5.0, 5.0));
    }

    /// Pause freezes ingestion (add_point no-ops) while clear/clear_channel
    /// still work, and unpausing resumes the flow (the AC's pause/clear
    /// semantics).
    #[test]
    fn pause_freezes_ingestion_clear_still_works() {
        let store = store();
        store.add_point_at("ch", 1.0, 1.0);
        assert!(!store.set_paused(true));
        assert!(store.is_paused());

        store.add_point_at("ch", 2.0, 2.0);
        assert_eq!(store.channel_names(), vec![("ch".to_string(), 1)]);

        store.clear();
        assert_eq!(store.channel_names(), vec![("ch".to_string(), 0)]);

        store.set_paused(false);
        store.add_point_at("ch", 3.0, 3.0);
        assert_eq!(store.channel_names(), vec![("ch".to_string(), 1)]);

        // Non-finite values never enter a ring.
        store.add_point("ch", f64::NAN);
        store.add_point_at("ch", f64::INFINITY, 1.0);
        assert_eq!(store.channel_names(), vec![("ch".to_string(), 1)]);
    }

    /// Clear-channel drops exactly the one channel's points — the ring itself
    /// stays (a cleared channel that keeps receiving points refills it; an
    /// idle empty channel is filtered from every snapshot).
    #[test]
    fn clear_channel_scopes_to_one_name() {
        let store = store();
        store.add_point_at("a", 1.0, 1.0);
        store.add_point_at("b", 1.0, 2.0);
        store.clear_channel("a");
        let names = store.channel_names();
        assert_eq!(names, vec![("a".to_string(), 0), ("b".to_string(), 1)]);
        // The cleared channel's points are gone from the snapshots too.
        assert!(store.sample(1_000_000.0, 10).series.iter().all(|series| series.name != "a"));
        store.clear_channel("missing");
        assert_eq!(store.channel_names(), vec![("a".to_string(), 0), ("b".to_string(), 1)]);
    }

    /// The version bumps on every visible mutation only — the UI pump samples
    /// on change, not per tick.
    #[test]
    fn version_tracks_mutations() {
        let store = store();
        let v0 = store.version();
        store.add_point_at("ch", 1.0, 1.0);
        assert!(store.version() > v0);

        let v1 = store.version();
        store.add_point_at("ch", 1.0, f64::NAN); // dropped: no bump
        assert_eq!(store.version(), v1);

        let v2 = store.version();
        store.set_paused(true);
        assert!(store.version() > v2);
        let v3 = store.version();
        store.set_paused(true); // same value: no bump
        assert_eq!(store.version(), v3);
    }

    /// The window slices by time: only points inside the last `window_s`
    /// seconds come back, and empty series are omitted.
    #[test]
    fn sample_slices_by_window() {
        let store = store();
        let now = chrono::Utc::now().timestamp_millis() as f64;
        store.add_point_at("old", now - 10_000.0, 1.0);
        store.add_point_at("old", now - 9_500.0, 2.0);
        store.add_point_at("fresh", now - 500.0, 3.0);
        let snapshot = store.sample(1.0, 1000); // last 1s
        assert_eq!(snapshot.series.len(), 1);
        assert_eq!(snapshot.series[0].name, "fresh");
        assert_eq!(snapshot.series[0].points, vec![(now - 500.0, 3.0)]);
    }

    /// While paused, sampling anchors to the newest stored point: the picture
    /// freezes instead of scrolling away.
    #[test]
    fn paused_sample_anchors_to_last_point() {
        let store = store();
        store.add_point_at("ch", 1_000.0, 1.0);
        store.set_paused(true);
        let snapshot = store.sample(2.0, 1000);
        // Anchor = 1000ms, window 2s → the point at 1000ms is still in view.
        assert_eq!(snapshot.series[0].points.len(), 1);
    }

    /// The global store is the same instance from anywhere.
    #[test]
    fn global_store_is_a_singleton() {
        let a = global_store();
        let b = global_store();
        assert!(std::ptr::eq(a, b));
        a.add_point_at("global-store-test", 1.0, 1.0);
        assert!(b.channel_names().iter().any(|(name, _)| name == "global-store-test"));
        b.clear_channel("global-store-test");
    }
}
