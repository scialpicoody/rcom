//! The per-channel point ring: a bounded FIFO of `(t, v)` samples (issue #24).
//!
//! `t` is an epoch-millisecond `f64`, `v` the channel value. Pushing past the
//! capacity drops the *oldest* points, so memory stays bounded no matter the
//! data rate; the latest `cap` points are always the window a plot renders.

use std::collections::VecDeque;

/// One time-series sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotPoint {
    /// Epoch milliseconds.
    pub t: f64,
    pub v: f64,
}

/// A bounded ring of [`PlotPoint`]s.
#[derive(Debug)]
pub struct PointRing {
    cap: usize,
    points: VecDeque<PlotPoint>,
}

impl PointRing {
    /// A ring holding the latest `cap` points (`cap >= 1`; larger caps clamp
    /// down — a plot of zero points is not a plot).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            points: VecDeque::new(),
        }
    }

    /// The configured capacity.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The number of points currently held.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Appends a point, dropping the oldest when full.
    pub fn push(&mut self, point: PlotPoint) {
        if self.points.len() >= self.cap {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    /// The newest point, if any.
    pub fn last(&self) -> Option<PlotPoint> {
        self.points.back().copied()
    }

    /// Iterates the points oldest → newest.
    pub fn iter(&self) -> impl Iterator<Item = PlotPoint> + '_ {
        self.points.iter().copied()
    }

    /// Drops every point.
    pub fn clear(&mut self) {
        self.points.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(t: f64, v: f64) -> PlotPoint {
        PlotPoint { t, v }
    }

    /// Pushing past the capacity rotates: the oldest points leave first, order
    /// of the survivors stays oldest → newest (the AC's ring rotation).
    #[test]
    fn ring_rotates_dropping_oldest() {
        let mut ring = PointRing::new(4);
        for i in 0..10 {
            ring.push(point(i as f64, i as f64 * 2.0));
        }
        assert_eq!(ring.len(), 4);
        let seen: Vec<(f64, f64)> = ring.iter().map(|p| (p.t, p.v)).collect();
        assert_eq!(seen, vec![(6.0, 12.0), (7.0, 14.0), (8.0, 16.0), (9.0, 18.0)]);
        assert_eq!(ring.last(), Some(point(9.0, 18.0)));
    }

    /// A capacity clamps to at least one point, and below the cap the ring just
    /// grows.
    #[test]
    fn ring_holds_up_to_cap() {
        let mut ring = PointRing::new(0);
        ring.push(point(1.0, 1.0));
        ring.push(point(2.0, 2.0));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.last(), Some(point(2.0, 2.0)));

        let mut ring = PointRing::new(8);
        for i in 0..3 {
            ring.push(point(i as f64, 0.0));
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.cap(), 8);
    }

    /// Clear empties the ring but keeps it usable.
    #[test]
    fn clear_empties_but_keeps_the_ring() {
        let mut ring = PointRing::new(2);
        ring.push(point(1.0, 1.0));
        ring.clear();
        assert!(ring.is_empty());
        ring.push(point(2.0, 5.0));
        assert_eq!(ring.last(), Some(point(2.0, 5.0)));
    }
}
