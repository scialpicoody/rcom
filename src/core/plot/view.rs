//! Pure plot-view math (issue #24): auto-scale ranges and point decimation.
//!
//! FerroUI-free on purpose — the custom render control (app side) calls these
//! from `render`, and the unit tests pin the math headlessly.

use super::store::SeriesSnapshot;

/// The value/time window a plot renders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewRange {
    pub t_min: f64,
    pub t_max: f64,
    pub v_min: f64,
    pub v_max: f64,
}

/// Extra vertical headroom (as a fraction of the span) added around the data
/// so curves do not touch the frame.
const VALUE_PAD: f64 = 0.1;

/// The time axis is fixed to the trailing window `[anchor - window, anchor]`;
/// the value axis auto-scales to every visible series with a 10% pad. A flat
/// line or a single point gets a ±max(1, |v|) band, so something is always
/// drawable. With no data at all the window is `[0, 1]`.
pub fn auto_range(series: &[SeriesSnapshot], window_s: f64, anchor_ms: f64) -> ViewRange {
    let window_ms = window_s.abs() * 1000.0;
    let (t_min, t_max) = (anchor_ms - window_ms, anchor_ms);
    let mut v_min = f64::INFINITY;
    let mut v_max = f64::NEG_INFINITY;
    for points in series.iter().map(|series| &series.points) {
        for &(_, v) in points {
            if v < v_min {
                v_min = v;
            }
            if v > v_max {
                v_max = v;
            }
        }
    }
    if !v_min.is_finite() || !v_max.is_finite() {
        return ViewRange { t_min, t_max, v_min: 0.0, v_max: 1.0 };
    }
    if (v_max - v_min).abs() < f64::EPSILON {
        let band = v_min.abs().max(1.0);
        v_min -= band;
        v_max += band;
    }
    let span = v_max - v_min;
    ViewRange {
        t_min,
        t_max,
        v_min: v_min - span * VALUE_PAD,
        v_max: v_max + span * VALUE_PAD,
    }
}

/// Decimates an ascending-by-time point list to at most `max_points` samples
/// with an even stride (keeping the first and last point). This bounds the
/// polyline a render builds per channel — at 10k stored points/channel and a
/// ~1000 px wide plot, the draw cost follows the pixels, not the history.
pub fn decimate(points: &[(f64, f64)], max_points: usize) -> Vec<(f64, f64)> {
    if points.len() <= max_points.max(2) {
        return points.to_vec();
    }
    let max_points = max_points.max(2);
    let stride = (points.len() - 1) as f64 / (max_points - 1) as f64;
    let mut out = Vec::with_capacity(max_points);
    for i in 0..max_points {
        out.push(points[(i as f64 * stride) as usize]);
    }
    out
}

/// Maps a data point into plot-area pixel coordinates (`size` = (width,
/// height) of the area; origin top-left). Clamps to the area rectangle, so a
/// stray point never draws outside the frame.
pub fn to_pixel(t: f64, v: f64, range: &ViewRange, width: f64, height: f64) -> (f64, f64) {
    let t_span = (range.t_max - range.t_min).abs().max(1.0);
    let v_span = (range.v_max - range.v_min).abs().max(f64::EPSILON);
    let x = ((t - range.t_min) / t_span * width).clamp(0.0, width);
    let y = (height - (v - range.v_min) / v_span * height).clamp(0.0, height);
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(name: &str, points: &[(f64, f64)]) -> SeriesSnapshot {
        SeriesSnapshot {
            name: name.to_string(),
            points: points.to_vec(),
        }
    }

    /// Auto-scale spans the data with a 10% pad; the time axis is the trailing
    /// window (the AC's auto-scale math).
    #[test]
    fn auto_range_spans_data_with_pad() {
        let series = [series("a", &[(0.0, -10.0), (1.0, 10.0)])];
        let range = auto_range(&series, 10.0, 10_000.0);
        assert_eq!((range.t_min, range.t_max), (0.0, 10_000.0));
        assert!((range.v_min - (-12.0)).abs() < 1e-9, "{range:?}");
        assert!((range.v_max - 12.0).abs() < 1e-9, "{range:?}");
    }

    /// A flat line gets a ±max(1, |v|) band; a single point too; no data is
    /// the 0..1 unit window — all three must stay drawable. The 10% pad rides
    /// on top of every band.
    #[test]
    fn auto_range_handles_flat_single_and_empty() {
        let flat = [series("flat", &[(0.0, 5.0), (1.0, 5.0)])];
        let range = auto_range(&flat, 1.0, 1000.0);
        // Band ±5 around the flat 5 → [0, 10], then the pad: [-1, 11].
        assert!((range.v_min - (-1.0)).abs() < 1e-9 && (range.v_max - 11.0).abs() < 1e-9, "{range:?}");

        let flat_zero = [series("flat", &[(0.0, 0.0), (1.0, 0.0)])];
        let range = auto_range(&flat_zero, 1.0, 1000.0);
        // Band ±1 → [-1, 1], then the pad: [-1.2, 1.2].
        assert!((range.v_min - (-1.2)).abs() < 1e-9 && (range.v_max - 1.2).abs() < 1e-9, "{range:?}");

        let single = [series("single", &[(0.0, -3.0)])];
        let range = auto_range(&single, 1.0, 1000.0);
        // Band ±3 → [-6, 0], then the pad: [-6.6, 0.6].
        assert!((range.v_min - (-6.6)).abs() < 1e-9 && (range.v_max - 0.6).abs() < 1e-9, "{range:?}");

        let range = auto_range(&[], 1.0, 1000.0);
        assert_eq!((range.v_min, range.v_max), (0.0, 1.0));
    }

    /// Decimation keeps the endpoints, shrinks to the cap and preserves order.
    #[test]
    fn decimate_keeps_endpoints_under_cap() {
        let points: Vec<(f64, f64)> = (0..10_000).map(|i| (i as f64, i as f64)).collect();
        let shrunk = decimate(&points, 1000);
        assert_eq!(shrunk.len(), 1000);
        assert_eq!(shrunk[0], (0.0, 0.0));
        assert_eq!(shrunk[999], (9_999.0, 9_999.0));
        assert!(shrunk.windows(2).all(|pair| pair[0].0 < pair[1].0), "order preserved");

        // Under the cap: a verbatim copy.
        let small = vec![(1.0, 2.0), (3.0, 4.0)];
        assert_eq!(decimate(&small, 1000), small);
        // Degenerate caps still produce at least the endpoints.
        let two = decimate(&points, 0);
        assert_eq!(two.len(), 2);
    }

    /// The value→pixel mapping puts v_min at the bottom, v_max at the top,
    /// clamps strays, and is linear in between.
    #[test]
    fn to_pixel_maps_and_clamps() {
        let range = ViewRange { t_min: 0.0, t_max: 100.0, v_min: 0.0, v_max: 10.0 };
        assert_eq!(to_pixel(0.0, 0.0, &range, 200.0, 100.0), (0.0, 100.0));
        assert_eq!(to_pixel(100.0, 10.0, &range, 200.0, 100.0), (200.0, 0.0));
        let (x, y) = to_pixel(50.0, 5.0, &range, 200.0, 100.0);
        assert!((x - 100.0).abs() < 1e-9 && (y - 50.0).abs() < 1e-9, "{x} {y}");

        // Strays clamp into the frame instead of drawing outside it.
        assert_eq!(to_pixel(-50.0, 99.0, &range, 200.0, 100.0), (0.0, 0.0));
        assert_eq!(to_pixel(150.0, -99.0, &range, 200.0, 100.0), (200.0, 100.0));
    }
}
