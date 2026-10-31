//! The plot page (issue #24): the bottom tools pane's curve tab.
//!
//! The XAML carries the skeleton (named controls, no visible strings); this
//! module resolves them, builds the custom [`PlotView`] render control into
//! `PlotHost`, wires the decoder Start/Stop, Pause/Clear and zoom controls,
//! and consumes [`UiMsg::Plot`] messages (decoder lifecycle → the status
//! line; the rows also land in the main log view via the host driver).
//!
//! The decoder picker lists the same scripts the Script page edits: the
//! built-in samples first (the bundled framed-protocol decoder among them),
//! then the saved scripts of `<config>/rcom/scripts/` — decoders are managed
//! exactly like other scripts (edited on the Script page, selected here).
//!
//! Rendering cadence: the pump (controller.rs, ~30 fps) calls [`pump_tick`],
//! which invalidates the view only when the plot store changed — points flow
//! into the store at wire rate from the decoder, the UI samples per tick and
//! never per point.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{Button, CheckBox, ComboBox, Control, Grid, TabItem, TextBlock};

use rust_i18n::t;

use super::host::{core_host, Command};
use super::plot_view::{format_window_s, PlotView};
use super::scriptstore;
use rcom::core::bus::{PlotEvent, PlotEventKind};
use rcom::core::lua::scripts::FRAMED_DECODER_SAMPLE;
use rcom::core::plot::store::global_store;

/// One page instance: the resolved controls plus the plot view and its
/// pump-sync state.
struct PlotPage {
    tab: Ref<TabItem>,
    decoder_caption: Ref<TextBlock>,
    decoder: Ref<ComboBox>,
    start: Ref<Button>,
    stop: Ref<Button>,
    pause: Ref<CheckBox>,
    clear: Ref<Button>,
    status: Ref<TextBlock>,
    host: Ref<Grid>,
    window_caption: Ref<TextBlock>,
    zoom_out: Ref<Button>,
    window_label: Ref<TextBlock>,
    zoom_in: Ref<Button>,
    /// The plot control itself (built here, not in the XAML — the design.md
    /// pin-gap custom render control).
    view: Ref<PlotView>,
    /// The saved script names behind the picker (samples come first, from
    /// [`scriptstore::SAMPLES`]).
    scripts: RefCell<Vec<String>>,
    /// The plot-store revision the view last rendered — the pump skips
    /// invalidation while nothing changed.
    rendered_version: Cell<u64>,
}

impl PlotPage {
    /// Applies the locale strings once at attach time (the app re-creates the
    /// window per locale change — the rest of the app works the same way).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("plot_tab").to_string())));
        self.decoder_caption.set_text(Some(&t!("plot_decoder")));
        self.start.set_content(Some(Rc::new(t!("plot_start").to_string())));
        self.stop.set_content(Some(Rc::new(t!("plot_stop").to_string())));
        self.pause.set_content(Some(Rc::new(t!("plot_pause").to_string())));
        self.clear.set_content(Some(Rc::new(t!("plot_clear").to_string())));
        self.window_caption.set_text(Some(&t!("plot_window")));
        self.zoom_in.set_content(Some(Rc::new(t!("plot_zoom_in").to_string())));
        self.zoom_out.set_content(Some(Rc::new(t!("plot_zoom_out").to_string())));
        self.set_status(t!("plot_status_idle").to_string());
        self.sync_window_label();
    }

    fn set_status(&self, text: String) {
        self.status.set_text(Some(&text));
    }

    /// Rebuilds the picker: the built-in samples first, then the saved scripts
    /// (`<config>/rcom/scripts/*.lua`); the bundled framed-protocol decoder
    /// preselects.
    fn refresh_decoder_picker(&self) {
        let items = self.decoder.items();
        items.clear();
        *self.scripts.borrow_mut() = scriptstore::list(scriptstore::scripts_dir().as_deref());
        let mut framed_index = None;
        for (index, sample) in scriptstore::SAMPLES.iter().enumerate() {
            if sample.code == FRAMED_DECODER_SAMPLE {
                framed_index.get_or_insert(index);
            }
            let value: BoxedValue = Rc::new(sample.name.to_string());
            items.add(Some(value));
        }
        for name in self.scripts.borrow().iter() {
            let value: BoxedValue = Rc::new(name.clone());
            items.add(Some(value));
        }
        let selected = framed_index.unwrap_or(0);
        self.decoder.set_selected_index(selected as i32);
    }

    /// The code of the selected script: samples read from the binary, saved
    /// scripts from the store.
    fn selected_code(&self) -> Option<(String, String)> {
        let index = self.decoder.selected_index();
        if index < 0 {
            return None;
        }
        let index = index as usize;
        let samples = scriptstore::SAMPLES.len();
        if index < samples {
            let sample = &scriptstore::SAMPLES[index];
            Some((sample.name.to_string(), sample.code.to_string()))
        } else {
            let name = self.scripts.borrow().get(index - samples).cloned()?;
            let code = scriptstore::load(scriptstore::scripts_dir().as_deref(), &name)?;
            Some((name, code))
        }
    }

    /// Start: the selected script boots as a decoder VM on the core runtime
    /// (a previous one is stopped first, per the command). The confirmation
    /// comes back as a `UiMsg::Plot` lifecycle event.
    fn start_decoder(&self) {
        let Some((name, code)) = self.selected_code() else {
            self.set_status(t!("plot_start_missing").to_string());
            return;
        };
        core_host()
            .commands
            .send(Command::PlotDecoderStart { name, code })
            .ok();
        self.set_status(t!("plot_status_starting").to_string());
    }

    /// Stop: the running decoder unwinds; the driver confirms with the event.
    fn stop_decoder(&self) {
        core_host().commands.send(Command::PlotDecoderStop).ok();
    }

    /// Pause freezes the *plot*: the store stops accepting points (decoders
    /// idle out) while the byte stream keeps flowing (contract §5.4 — the log
    /// view keeps filling).
    fn apply_pause(&self) {
        let paused = self.pause.is_checked() == Some(true);
        global_store().set_paused(paused);
    }

    /// Clear drops every channel's points (the rings stay).
    fn clear_plot(&self) {
        global_store().clear();
    }

    /// The zoom buttons halve/double the rendered time window (the wheel on
    /// the plot area does the same).
    fn zoom(&self, inwards: bool) {
        if inwards {
            self.view.zoom_in();
        } else {
            self.view.zoom_out();
        }
        self.sync_window_label();
    }

    fn sync_window_label(&self) {
        self.window_label.set_text(Some(&format_window_s(self.view.window_s())));
    }

    /// The UI reaction to one [`PlotEvent`] (also mirrored to the main log
    /// view by the host driver).
    fn apply_plot(&self, plot: &PlotEvent) {
        match &plot.kind {
            PlotEventKind::DecoderStarted { name } => {
                self.set_status(t!("plot_status_running", name = name).to_string());
            }
            PlotEventKind::DecoderStopped { .. } => {
                self.set_status(t!("plot_status_stopped").to_string());
            }
            PlotEventKind::DecoderFailed { name, message } => {
                self.set_status(t!("plot_status_failed", name = name, reason = message).to_string());
            }
        }
    }

    /// The pump's per-tick nudge: re-render when (and only when) the store
    /// changed since the last render. The version is an atomic load; the
    /// invalidate is a dirty-mark, so the render itself stays ≤ 30 fps and
    /// costs only the decimated draw.
    fn pump_tick(&self) {
        let version = global_store().version();
        if version != self.rendered_version.get() {
            self.rendered_version.set(version);
            self.view.invalidate_visual();
        }
    }
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle to the page (the pump and the event handlers run
    /// there; the pattern of the controller's PUMP).
    static PAGE: RefCell<Option<Rc<PlotPage>>> = RefCell::new(None);
}

/// Resolves the named controls of the plot tab, builds the plot view, sets
/// the strings and wires the events. One insertion point in the controller's
/// `attach` (beside the net tools page).
pub fn attach(element: &Ref<StyledElement>) {
    let view = PlotView::new();
    let page = Rc::new(PlotPage {
        tab: NameScopeExtensions::get(element, "PlotTab"),
        decoder_caption: NameScopeExtensions::get(element, "PlotDecoderCaption"),
        decoder: NameScopeExtensions::get(element, "PlotDecoderSelector"),
        start: NameScopeExtensions::get(element, "PlotDecoderStartButton"),
        stop: NameScopeExtensions::get(element, "PlotDecoderStopButton"),
        pause: NameScopeExtensions::get(element, "PlotPauseCheck"),
        clear: NameScopeExtensions::get(element, "PlotClearButton"),
        status: NameScopeExtensions::get(element, "PlotStatus"),
        host: NameScopeExtensions::get(element, "PlotHost"),
        window_caption: NameScopeExtensions::get(element, "PlotWindowCaption"),
        zoom_out: NameScopeExtensions::get(element, "PlotZoomOutButton"),
        window_label: NameScopeExtensions::get(element, "PlotWindowLabel"),
        zoom_in: NameScopeExtensions::get(element, "PlotZoomInButton"),
        view,
        scripts: RefCell::new(Vec::new()),
        rendered_version: Cell::new(0),
    });

    // The plot control into its host: it stretches with the pane.
    page.host.children().add(page.view.clone().upcast::<Control>());

    page.apply_ui_strings();
    page.refresh_decoder_picker();

    page.start.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.start_decoder()
    });
    page.stop.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.stop_decoder()
    });
    page.pause.is_checked_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_pause()
    });
    page.clear.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.clear_plot()
    });
    page.zoom_in.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.zoom(true)
    });
    page.zoom_out.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.zoom(false)
    });
    // Wheel over the plot: scroll up zooms in, down zooms out.
    page.view.pointer_wheel_changed({
        let page = Rc::clone(&page);
        move |_, args| {
            args.set_handled(true);
            page.zoom(args.delta().y > 0.0);
        }
    });

    PAGE.with(|cell| *cell.borrow_mut() = Some(page));
}

/// The pump's entry for [`UiMsg::Plot`](super::host::UiMsg::Plot) — the
/// decoder lifecycle lands in the page's status line.
pub fn apply_plot(plot: PlotEvent) {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.apply_plot(&plot);
        }
    });
}

/// The pump's per-tick plot nudge (a store-version check; invalidates the
/// view when data changed).
pub fn pump_tick() {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.pump_tick();
        }
    });
}
