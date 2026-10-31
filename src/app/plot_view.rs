//! The plot render control (issue #24): a small custom `VisualImpl::render`
//! control — the design.md pin-gap answer ("no charts crate published at this
//! pin; plot is a small custom render control").
//!
//! Draw pipeline per render pass (the UI pump invalidates it when the plot
//! store changed, ≤ once per pump tick ~30 fps):
//!
//! 1. **pull** — one [`PlotStore::sample`] of every channel into a snapshot,
//!    decimated to about two points per pixel (the draw cost follows the
//!    pixels, not the 10k-point history);
//! 2. **auto-scale** — the time axis is the trailing window (the zoom state,
//!    a styled property so it re-renders on change), the value axis spans
//!    every visible series with a pad (the pure math lives in
//!    `core::plot::view`, unit-tested headlessly);
//! 3. **draw** — dark plot area, grid lines, one polyline per channel in its
//!    palette colour (clipped to the plot rect), a legend (swatch + name) and
//!    the min/max value labels.
//!
//! Lives on the UI thread only (FerroUI objects are `Rc`); the store is
//! process-global and lock-guarded, so render never touches the message pump.

use std::rc::Rc;

use ferroui_base::input::InputElementImpl;
use ferroui_base::interactivity::InteractiveImpl;
use ferroui_base::layout::LayoutableImpl;
use ferroui_base::media::immutable::{ImmutablePen, ImmutableSolidColorBrush};
use ferroui_base::media::{
    Color, DrawingContext, FormattedText, FlowDirection, IBrush, IPen, Typeface,
};
use ferroui_base::utilities::CultureInfo;
use ferroui_base::{
    ferro_class, ferro_class_info, ferro_impl_classes, ferro_property, instantiate, FerroObjectImpl,
    FerroProperty, Point, Rect, Ref, Size, StyledElementImpl, StyledProperty, Visual, VisualImpl,
};
use ferroui_controls::{Control, ControlImpl};

use rcom::core::plot::store::{global_store, PlotSnapshot};
use rcom::core::plot::view::{auto_range, to_pixel};

/// The default rendered time window, seconds (the zoom buttons/wheel rescale
/// it by factors of two).
pub const DEFAULT_WINDOW_S: f64 = 30.0;

/// The zoom floor/ceiling: below one second a serial-rate curve is all edges,
/// above ten minutes nothing scrolls visibly any more.
const MIN_WINDOW_S: f64 = 1.0;
const MAX_WINDOW_S: f64 = 600.0;

/// The fixed look of the plot area (an oscilloscope-style dark field, so the
/// curve palette stays readable under either theme).
const PLOT_BG: (u8, u8, u8) = (24, 26, 32);
const GRID: (u8, u8, u8) = (58, 62, 70);
const LABEL: (u8, u8, u8) = (196, 200, 208);

/// The per-channel palette: distinct hues, assigned by series order (the
/// store's channel map is sorted by name, so a channel keeps its colour while
/// the channel set is stable).
const PALETTE: [(u8, u8, u8); 8] = [
    (0, 229, 255),   // cyan
    (255, 64, 129),  // magenta
    (174, 234, 0),   // lime
    (255, 196, 0),   // amber
    (179, 136, 255), // violet
    (255, 110, 64),  // orange
    (105, 240, 174), // mint
    (68, 138, 255),  // azure
];

fn color(rgb: (u8, u8, u8)) -> Color {
    Color::from_rgb(rgb.0, rgb.1, rgb.2)
}

fn brush(rgb: (u8, u8, u8)) -> Rc<dyn IBrush> {
    Rc::new(ImmutableSolidColorBrush::new(color(rgb)))
}

fn pen(rgb: (u8, u8, u8), thickness: f64) -> Rc<dyn IPen> {
    Rc::new(ImmutablePen::with_brush(
        Some(Rc::new(ImmutableSolidColorBrush::new(color(rgb)))),
        thickness,
    ))
}

/// One small label (the legend, the min/max marks): the pin's text draws
/// through `FormattedText`.
fn label(text: &str, rgb: (u8, u8, u8)) -> FormattedText {
    FormattedText::new(
        text,
        CultureInfo::invariant_culture(),
        FlowDirection::LeftToRight,
        Typeface::default_typeface(),
        11.0,
        Some(brush(rgb)),
    )
}

/// The legend/window label for a window length: whole seconds above one
/// second, one decimal below.
pub fn format_window_s(window_s: f64) -> String {
    if window_s >= 10.0 || (window_s - window_s.round()).abs() < f64::EPSILON {
        format!("{:.0} s", window_s)
    } else {
        format!("{:.1} s", window_s)
    }
}

/// The curve-plot control: a plain `Control` (no template) that draws itself
/// in [`VisualImpl::render`] from the process-global plot store.
#[repr(C)]
pub struct PlotView {
    base: Control,
}

ferro_class!(PlotView: Control);
ferro_class_info!(PlotView { new: PlotView::new });
// The Impl traits with nothing overridden (the pattern of TickBar, the pin's
// custom-draw control); `VisualImpl` is implemented by hand below — listing it
// here would collide (the macro writes an empty-overrides impl).
ferro_impl_classes!(PlotView: FerroObjectImpl, StyledElementImpl, LayoutableImpl, InteractiveImpl, InputElementImpl, ControlImpl);

impl VisualImpl for PlotView {
    fn render(this: &Self, dc: &mut DrawingContext) {
        let render_started = std::time::Instant::now();
        let bounds = this.bounds();
        let (width, height) = (bounds.width, bounds.height);
        if width < 8.0 || height < 8.0 {
            return; // nothing sensible to draw in a sliver
        }

        // The plot field (hit testing is bounds-based, so wheel input lands
        // here without a background brush).
        dc.fill_rectangle(&brush(PLOT_BG), Rect::from_size(Size::new(width, height)), 0.0);

        // The pull: one sample of every channel, decimated to the pixel width.
        let window_s = this.window_s();
        let max_points = ((width / 2.0) as usize).max(32);
        let snapshot: PlotSnapshot = global_store().sample(window_s, max_points);
        let range = auto_range(&snapshot.series, window_s, snapshot.anchor_ms);

        // Grid: three horizontal value lines and time ticks every 1/6 window.
        let grid_pen = pen(GRID, 1.0);
        for fraction in [0.25, 0.5, 0.75] {
            let y = height * fraction;
            dc.draw_line(&grid_pen, Point::new(0.0, y), Point::new(width, y));
        }
        for step in 1..6 {
            let t = range.t_min + (range.t_max - range.t_min) * (step as f64 / 6.0);
            let (x, _) = to_pixel(t, 0.0, &range, width, height);
            dc.draw_line(&grid_pen, Point::new(x, 0.0), Point::new(x, height));
        }

        // The curves: one polyline per series, clipped to the plot area.
        let clip = dc.push_clip(Rect::from_size(Size::new(width, height)));
        for (index, series) in snapshot.series.iter().enumerate() {
            let rgb = PALETTE[index % PALETTE.len()];
            let line_pen = pen(rgb, 1.6);
            let mut previous: Option<Point> = None;
            for &(t, v) in &series.points {
                let (x, y) = to_pixel(t, v, &range, width, height);
                let point = Point::new(x, y);
                if let Some(from) = previous {
                    dc.draw_line(&line_pen, from, point);
                }
                previous = Some(point);
            }
        }
        dc.pop(clip);

        // The legend, top-left: swatch + channel name per series.
        let swatch_w = 14.0;
        let gap = 8.0;
        let mut x = 6.0;
        for (index, series) in snapshot.series.iter().enumerate() {
            let rgb = PALETTE[index % PALETTE.len()];
            let text = label(&series.name, rgb);
            let text_w = text.width();
            if x + swatch_w + text_w > width {
                break; // the legend never overflows the plot
            }
            let text_h = text.height();
            let swatch_y = (text_h - 4.0) / 2.0; // mid-line of the label row
            dc.fill_rectangle(&brush(rgb), Rect::new(x, swatch_y, swatch_w, 4.0), 2.0);
            dc.draw_text(&text, Point::new(x + swatch_w + 3.0, 0.0));
            x += swatch_w + text_w + gap;
        }

        // The value-range marks, left edge: max at the top, min at the bottom.
        let v_max = label(&format_value(range.v_max), LABEL);
        let v_min = label(&format_value(range.v_min), LABEL);
        dc.draw_text(&v_max, Point::new(4.0, 2.0));
        dc.draw_text(&v_min, Point::new(4.0, height - v_min.height() - 2.0));

        // The live-rate budget (issue #24): one render pass over the sampled
        // snapshot must stay far inside a 33 ms frame; breaches trace so the
        // PLOTDEMO fast smoke proves the draw by their absence.
        let elapsed = render_started.elapsed();
        if elapsed > std::time::Duration::from_millis(20) {
            tracing::warn!(target: "rcom::plot", "plot render {:.1} ms for {} series — over the 20 ms budget",
                elapsed.as_secs_f64() * 1000.0, snapshot.series.len());
        }
    }
}

ferroui_base::ferro_properties! { impl PlotView {
    ferro_property!(
        /// The rendered time window in seconds — the zoom state. Changing it
        /// re-renders (`affects_render` in the static constructor).
        pub fn window_s_property() -> StyledProperty<f64> {
            FerroProperty::register::<PlotView, _>("WindowS", DEFAULT_WINDOW_S)
        }
    );
} }

impl PlotView {
    fn static_constructor() {
        Visual::affects_render::<PlotView>(&[Self::window_s_property().as_property()]);
    }

    /// Creates the class data; see [`ferroui_base::FerroObject::construct`].
    pub fn construct() -> Self {
        Self { base: Control::construct() }
    }

    pub fn new() -> Ref<Self> {
        instantiate(Self::construct())
    }

    /// The rendered time window in seconds.
    pub fn window_s(&self) -> f64 {
        self.get_value(Self::window_s_property())
    }

    pub fn set_window_s(&self, value: f64) {
        self.set_value(Self::window_s_property(), value.clamp(MIN_WINDOW_S, MAX_WINDOW_S));
    }

    /// Zooms the time window in (a shorter window: more detail).
    pub fn zoom_in(&self) {
        self.set_window_s(self.window_s() / 2.0);
    }

    /// Zooms the time window out (a longer window: more history).
    pub fn zoom_out(&self) {
        self.set_window_s(self.window_s() * 2.0);
    }
}

/// A value-axis label: compact for big/small magnitudes (`1.2k`, `-3.5m`).
fn format_value(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 10_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if magnitude >= 1_000.0 {
        format!("{:.0}", value)
    } else if magnitude < 0.01 && magnitude > 0.0 {
        format!("{:.1}m", value * 1_000.0)
    } else {
        format!("{:.1}", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window label reads whole seconds above ten, one decimal below
    /// (the zoom buttons halve/double, so 7.5 s must stay readable).
    #[test]
    fn window_labels_format() {
        assert_eq!(format_window_s(30.0), "30 s");
        assert_eq!(format_window_s(600.0), "600 s");
        assert_eq!(format_window_s(7.5), "7.5 s");
        assert_eq!(format_window_s(1.0), "1 s");
        assert_eq!(format_window_s(3.75), "3.8 s");
    }

    /// Value labels stay compact across magnitudes.
    #[test]
    fn value_labels_format() {
        assert_eq!(format_value(0.0), "0.0");
        assert_eq!(format_value(23.26), "23.3");
        assert_eq!(format_value(-1234.0), "-1234");
        assert_eq!(format_value(15_000.0), "15.0k");
        assert_eq!(format_value(0.004), "4.0m");
    }
}
