//! The controller of `MainWindow`: looks the named controls up in the window's
//! name scope, wires their events to core commands, and pumps the core's
//! messages into the per-session log views.
//!
//! Ticket #37 (design-ui §2/§3): the window shell is the session tab strip —
//! one tab per session, each with its own log model/scrollback, send bar,
//! quick-send page set and chip; the shared controls render the ACTIVE tab's
//! view-model (switching a tab re-renders from the model; presentation state
//! is viewmodel state, never core state — contract §5.3). Data routing filters
//! by `session_id` everywhere (§3.6: a tab must never process another
//! session's data events). The mock drawer keys off `PortInfo::kind` and the
//! `mock_controls()` handle only (AC9 — zero mock env references here).
//!
//! Ticket #17's encoding-switch re-render, #16's hide-sent filter, #19's
//! script page and #25's i18n live-apply carry over, now per session.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::layout::Orientation;
use ferroui_base::media::{Color, SolidColorBrush};
use ferroui_base::threading::{DispatcherPriority, DispatcherTimer};
use ferroui_base::reactive::IDisposable;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{
    Border, Button, CheckBox, ComboBox, Control, Flyout, PipsPager, ProgressBar, ScrollViewer,
    StackPanel, TabControl, TabItem, TextBox, TextBlock,
};

use super::host::{core_host, AutodemoMode, Command, MockAction, UiMsg};
use super::logview::{self, LogLine, ViewMode};
use super::quicksend::{QuickSendEntry, QuickSendStore, PAGE_COUNT};
use super::scriptstore;
use super::session_vm::SessionVm;
use super::ui_patterns::{self, Notifier, OpenProgressGuard};
use super::{tools_net, tools_settings};
use rcom::core::bus::{Direction, PortState, GLOBAL_SESSION_ID};
use rcom::core::codec::{parse_hex, TextEncoding};
use rcom::core::port::{MockScenario, PortInfo};

use rust_i18n::t;

/// The baud rates the baud selector offers; 115200 is preselected (or the
/// persisted launch default — ticket #25).
pub(crate) const BAUD_RATES: [u32; 7] = [9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800];

/// How often the UI pumps messages from the core driver.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(30);

/// The script editor's monospaced families, first available wins (the pin's
/// TextBox has no XAML-settable font family — set in code, per the #19 brief).
const SCRIPT_EDITOR_FONT: &str = "Menlo, Consolas, DejaVu Sans Mono, Courier New";

/// The mock playback speeds the drawer offers (§6.1's 0.25×–8× window).
const MOCK_SPEEDS: [f64; 6] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

/// Chip colours (§3.2): Closed grey · Open green · WatchDesired amber.
fn chip_colors(state: PortState) -> (Color, Color) {
    match state {
        PortState::Open | PortState::Reopened => {
            (Color::from_argb(0x2E, 0x6C, 0xCB, 0x5F), logview::rx_color())
        }
        PortState::WatchDesired => {
            (Color::from_argb(0x2E, 0xF7, 0xB2, 0x4B), logview::note_color())
        }
        _ => (Color::from_argb(0x2E, 0xFF, 0xFF, 0xFF), Color::from_argb(0xC5, 0xFF, 0xFF, 0xFF)),
    }
}

pub struct MainController {
    /// The window itself: the title re-applies with the language (ticket #25's
    /// live switch) and the settings page holds its own handle for the theme.
    window: Ref<crate::MainWindow>,
    // The session tab strip (the window shell).
    session_tabs: Ref<TabControl>,
    close_tab: Ref<Button>,
    new_tab: Ref<Button>,
    // The log toolbar (per-active-session presentation state).
    follow_toggle: Ref<CheckBox>,
    view_mode_selector: Ref<ComboBox>,
    filter_box: Ref<TextBox>,
    shown_status: Ref<TextBlock>,
    timestamps_toggle: Ref<CheckBox>,
    hide_sent: Ref<CheckBox>,
    clear_log: Ref<Button>,
    log_scroller: Ref<ScrollViewer>,
    log_rows: Ref<StackPanel>,
    // Chip + banner + open progress.
    state_chip: Ref<Border>,
    state_chip_text: Ref<TextBlock>,
    banner_text: Ref<TextBlock>,
    banner_panel: Ref<StackPanel>,
    banner_close_all: Ref<Button>,
    open_progress: Ref<ProgressBar>,
    // Session toolbar.
    port_selector: Ref<ComboBox>,
    baud_selector: Ref<ComboBox>,
    open_close: Ref<Button>,
    refresh_button: Ref<Button>,
    encoding_selector: Ref<ComboBox>,
    port_caption: Ref<TextBlock>,
    baud_caption: Ref<TextBlock>,
    encoding_caption: Ref<TextBlock>,
    // Mock drawer.
    mock_drawer_button: Ref<Button>,
    mock_drawer: Ref<ferroui_controls::Grid>,
    mock_pause: Ref<Button>,
    mock_step: Ref<Button>,
    mock_speed: Ref<ComboBox>,
    mock_inject_box: Ref<TextBox>,
    mock_inject: Ref<Button>,
    mock_trigger: Ref<Button>,
    mock_restart: Ref<Button>,
    // Send bar.
    hex_send: Ref<CheckBox>,
    send_box: Ref<TextBox>,
    send_button: Ref<Button>,
    // Quick-send panel.
    quick_send_title: Ref<TextBlock>,
    quick_hook_badge: Ref<Border>,
    quick_hook_badge_text: Ref<TextBlock>,
    quick_page_label: Ref<TextBlock>,
    quick_pager: Ref<PipsPager>,
    quick_entries: Ref<StackPanel>,
    quick_label_caption: Ref<TextBlock>,
    quick_label_box: Ref<TextBox>,
    quick_payload_caption: Ref<TextBlock>,
    quick_payload_box: Ref<TextBox>,
    quick_hex_check: Ref<CheckBox>,
    quick_add: Ref<Button>,
    quick_save: Ref<Button>,
    quick_delete: Ref<Button>,
    // The script page (bottom pane, ticket #19).
    script_tab: Ref<TabItem>,
    script_file_caption: Ref<TextBlock>,
    script_selector: Ref<ComboBox>,
    script_name_box: Ref<TextBox>,
    script_new: Ref<Button>,
    script_save: Ref<Button>,
    script_delete: Ref<Button>,
    script_run: Ref<Button>,
    script_stop: Ref<Button>,
    tx_hook_caption: Ref<TextBlock>,
    tx_hook_box: Ref<TextBox>,
    tx_hook_enable: Ref<CheckBox>,
    script_editor: Ref<TextBox>,
    // The Terminal placeholder (ticket #34's slot).
    // The `+` port picker flyout (mounted once, content rebuilt per listing).
    picker_flyout: RefCell<Option<Ref<Flyout>>>,
    // The tab-close confirm flyout (the §4.1 confirm for tabs with open ports;
    // built lazily, shown from the ✕ handler).
    close_tab_flyout: RefCell<Option<Ref<Flyout>>>,
    /// The window-closing subscription (dropping it unsubscribes the guard).
    _closing_guard: RefCell<Option<Rc<dyn IDisposable>>>,
    // State.
    tabs: RefCell<Vec<Rc<RefCell<SessionVm>>>>,
    active: Cell<usize>,
    next_tab_id: Cell<usize>,
    /// Ports of the last listing (the pickers' data).
    ports: RefCell<Vec<PortInfo>>,
    /// Which tab asked for which port (pending opens; the Open event binds).
    /// Values are tab ids ([`SessionVm::tab_id`]) — stable across removals.
    pending_opens: RefCell<std::collections::HashMap<String, usize>>,
    /// Whether the UI is applying a VM to the shared controls (guards the
    /// control-event handlers against feedback loops during tab switches).
    syncing: Cell<bool>,
    /// Whether the quit guard already caught one close (one-click-past).
    quit_guard_passed: Cell<bool>,
    /// The open-progress reveal guard while an open future runs long.
    open_progress_guard: RefCell<Option<Rc<OpenProgressGuard>>>,
    /// The saved script names behind the picker (samples come first, from
    /// [`scriptstore::SAMPLES`]).
    scripts: RefCell<Vec<String>>,
    /// Whether the persisted launch-default port already preselected (the
    /// first listing after launch only — afterwards the user's choice wins).
    port_preset_done: Cell<bool>,
    notifier: RefCell<Option<Rc<Notifier>>>,
}

impl MainController {
    /// Builds the controller for `window`: resolves the named controls of
    /// `MainWindow.xaml`, subscribes their events and starts the pump. Call on the
    /// UI thread, after the window's markup is populated.
    pub fn attach(window: &Ref<crate::MainWindow>) {
        let element: &Ref<StyledElement> = window.upcast_ref();

        let this = Rc::new(Self {
            window: window.clone(),
            session_tabs: NameScopeExtensions::get(element, "SessionTabs"),
            close_tab: NameScopeExtensions::get(element, "CloseTabButton"),
            new_tab: NameScopeExtensions::get(element, "NewTabButton"),
            follow_toggle: NameScopeExtensions::get(element, "FollowToggle"),
            view_mode_selector: NameScopeExtensions::get(element, "ViewModeSelector"),
            filter_box: NameScopeExtensions::get(element, "FilterBox"),
            shown_status: NameScopeExtensions::get(element, "ShownStatus"),
            timestamps_toggle: NameScopeExtensions::get(element, "TimestampsToggle"),
            hide_sent: NameScopeExtensions::get(element, "HideSentToggle"),
            clear_log: NameScopeExtensions::get(element, "ClearLogButton"),
            log_scroller: NameScopeExtensions::get(element, "LogScroller"),
            log_rows: NameScopeExtensions::get(element, "LogRows"),
            state_chip: NameScopeExtensions::get(element, "StateChip"),
            state_chip_text: NameScopeExtensions::get(element, "StateChipText"),
            banner_text: NameScopeExtensions::get(element, "BannerText"),
            banner_panel: NameScopeExtensions::get(element, "BannerPanel"),
            banner_close_all: NameScopeExtensions::get(element, "BannerCloseAllButton"),
            open_progress: NameScopeExtensions::get(element, "OpenProgress"),
            port_selector: NameScopeExtensions::get(element, "PortSelector"),
            baud_selector: NameScopeExtensions::get(element, "BaudSelector"),
            open_close: NameScopeExtensions::get(element, "OpenCloseButton"),
            refresh_button: NameScopeExtensions::get(element, "RefreshButton"),
            encoding_selector: NameScopeExtensions::get(element, "EncodingSelector"),
            port_caption: NameScopeExtensions::get(element, "PortCaption"),
            baud_caption: NameScopeExtensions::get(element, "BaudCaption"),
            encoding_caption: NameScopeExtensions::get(element, "EncodingCaption"),
            mock_drawer_button: NameScopeExtensions::get(element, "MockDrawerButton"),
            mock_drawer: NameScopeExtensions::get(element, "MockDrawer"),
            mock_pause: NameScopeExtensions::get(element, "MockPauseButton"),
            mock_step: NameScopeExtensions::get(element, "MockStepButton"),
            mock_speed: NameScopeExtensions::get(element, "MockSpeedSelector"),
            mock_inject_box: NameScopeExtensions::get(element, "MockInjectBox"),
            mock_inject: NameScopeExtensions::get(element, "MockInjectButton"),
            mock_trigger: NameScopeExtensions::get(element, "MockTriggerButton"),
            mock_restart: NameScopeExtensions::get(element, "MockRestartButton"),
            hex_send: NameScopeExtensions::get(element, "HexSendCheck"),
            send_box: NameScopeExtensions::get(element, "SendBox"),
            send_button: NameScopeExtensions::get(element, "SendButton"),
            quick_send_title: NameScopeExtensions::get(element, "QuickSendTitle"),
            quick_hook_badge: NameScopeExtensions::get(element, "QuickSendHookBadge"),
            quick_hook_badge_text: NameScopeExtensions::get(element, "QuickSendHookBadgeText"),
            quick_page_label: NameScopeExtensions::get(element, "QuickSendPageLabel"),
            quick_pager: NameScopeExtensions::get(element, "QuickSendPager"),
            quick_entries: NameScopeExtensions::get(element, "QuickSendEntries"),
            quick_label_caption: NameScopeExtensions::get(element, "QuickSendLabelCaption"),
            quick_label_box: NameScopeExtensions::get(element, "QuickSendLabelBox"),
            quick_payload_caption: NameScopeExtensions::get(element, "QuickSendPayloadCaption"),
            quick_payload_box: NameScopeExtensions::get(element, "QuickSendPayloadBox"),
            quick_hex_check: NameScopeExtensions::get(element, "QuickSendHexCheck"),
            quick_add: NameScopeExtensions::get(element, "QuickSendAddButton"),
            quick_save: NameScopeExtensions::get(element, "QuickSendSaveButton"),
            quick_delete: NameScopeExtensions::get(element, "QuickSendDeleteButton"),
            script_tab: NameScopeExtensions::get(element, "ScriptTab"),
            script_file_caption: NameScopeExtensions::get(element, "ScriptFileCaption"),
            script_selector: NameScopeExtensions::get(element, "ScriptSelector"),
            script_name_box: NameScopeExtensions::get(element, "ScriptNameBox"),
            script_new: NameScopeExtensions::get(element, "ScriptNewButton"),
            script_save: NameScopeExtensions::get(element, "ScriptSaveButton"),
            script_delete: NameScopeExtensions::get(element, "ScriptDeleteButton"),
            script_run: NameScopeExtensions::get(element, "ScriptRunButton"),
            script_stop: NameScopeExtensions::get(element, "ScriptStopButton"),
            tx_hook_caption: NameScopeExtensions::get(element, "TxHookCaption"),
            tx_hook_box: NameScopeExtensions::get(element, "TxHookBox"),
            tx_hook_enable: NameScopeExtensions::get(element, "TxHookEnable"),
            script_editor: NameScopeExtensions::get(element, "ScriptEditor"),
            picker_flyout: RefCell::new(None),
            close_tab_flyout: RefCell::new(None),
            _closing_guard: RefCell::new(None),
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(0),
            next_tab_id: Cell::new(0),
            ports: RefCell::new(Vec::new()),
            pending_opens: RefCell::new(std::collections::HashMap::new()),
            syncing: Cell::new(false),
            quit_guard_passed: Cell::new(false),
            open_progress_guard: RefCell::new(None),
            scripts: RefCell::new(Vec::new()),
            port_preset_done: Cell::new(false),
            notifier: RefCell::new(None),
        });

        this.apply_ui_strings();
        this.fill_static_selectors();
        MainController::setup_script_page(&this);
        // ISSUE #34 (one insertion point): the terminal page of the bottom
        // pane — keyboard direct-send through the existing SendBytes path.
        super::tools_terminal::attach(element);
        // ISSUE #20 (one insertion point): the net tools page of the bottom pane.
        tools_net::attach(element);
        // ISSUE #21 (one insertion point): the MQTT tools page of the bottom pane.
        super::tools_mqtt::attach(element);
        // ISSUE #24 (one insertion point): the plot page of the bottom pane.
        super::tools_plot::attach(element);
        // ISSUE #22 (one insertion point): the codec toolbox page of the bottom
        // pane — synchronous pure `core::codec` calls, no pump messages.
        super::tools_codec::attach(element);
        // ISSUE #23 (one insertion point): the serial monitor page of the bottom pane.
        super::tools_monitor::attach(element);
        // ISSUE #25 (one insertion point): the settings page of the bottom pane —
        // the window handle is the live theme switch's target.
        tools_settings::attach(element, window);
        wire_events(&this);
        start_pump(&this);
        this.setup_tab_strip();
        this.install_quit_guard();
        MainController::autodemo_persist_check(&this);
        MainController::autodemo_settings_check();

        // The first tab: unbound, ready for the port picker (the `+` flow).
        this.add_tab();

        // Kick off the first port listing.
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    /// Every visible string of the window, from the dictionaries (the XAML carries
    /// none, so both locales ship from the same document). Re-runs on a live
    /// language switch (ticket #25).
    fn apply_ui_strings(&self) {
        self.window.set_title(Some(t!("title").to_string()));
        self.close_tab.set_content(Some(Rc::new("✕".to_string())));
        self.new_tab.set_content(Some(Rc::new("+".to_string())));

        // Log toolbar (§3.3).
        self.follow_toggle.set_content(Some(Rc::new(t!("follow").to_string())));
        self.timestamps_toggle.set_content(Some(Rc::new(t!("timestamps").to_string())));
        self.hide_sent.set_content(Some(Rc::new(t!("hide_sent").to_string())));
        self.clear_log.set_content(Some(Rc::new(t!("clear_log").to_string())));
        self.filter_box.set_placeholder_text(Some(&t!("filter_hint")));
        let items = self.view_mode_selector.items();
        items.clear();
        for key in ViewMode::COMBO_LABELS {
            let value: BoxedValue = Rc::new(t!(key).to_string());
            items.add(Some(value));
        }

        // Chip + banner.
        self.banner_close_all.set_content(Some(Rc::new(t!("quit_close_all").to_string())));

        // Session toolbar + send bar.
        self.refresh_button.set_content(Some(Rc::new(t!("refresh").to_string())));
        self.send_button.set_content(Some(Rc::new(t!("send").to_string())));
        self.send_box.set_placeholder_text(Some(&t!("send_hint")));
        self.port_caption.set_text(Some(&t!("port")));
        self.baud_caption.set_text(Some(&t!("baud")));
        self.encoding_caption.set_text(Some(&t!("encoding")));
        self.hex_send.set_content(Some(Rc::new(t!("send_hex").to_string())));

        // Mock drawer.
        self.mock_drawer_button.set_content(Some(Rc::new(t!("mock_drawer").to_string())));
        self.mock_step.set_content(Some(Rc::new(t!("mock_step").to_string())));
        self.mock_inject.set_content(Some(Rc::new(t!("mock_inject").to_string())));
        self.mock_restart.set_content(Some(Rc::new(t!("mock_restart").to_string())));
        self.mock_inject_box.set_placeholder_text(Some(&t!("mock_inject_hint")));
        let speeds = self.mock_speed.items();
        speeds.clear();
        for speed in MOCK_SPEEDS {
            let value: BoxedValue = Rc::new(format!("{speed}×"));
            speeds.add(Some(value));
        }

        // Quick-send panel.
        self.quick_send_title.set_text(Some(&t!("quick_send")));
        self.quick_label_caption.set_text(Some(&t!("quick_label")));
        self.quick_payload_caption.set_text(Some(&t!("quick_payload")));
        self.quick_hex_check.set_content(Some(Rc::new(t!("quick_hex").to_string())));
        self.quick_add.set_content(Some(Rc::new(t!("quick_add").to_string())));
        self.quick_save.set_content(Some(Rc::new(t!("quick_save").to_string())));
        self.quick_delete.set_content(Some(Rc::new(t!("quick_delete").to_string())));
        self.quick_label_box.set_placeholder_text(Some(&t!("quick_label_hint")));
        self.quick_payload_box.set_placeholder_text(Some(&t!("quick_payload_hint")));

        // The script page (the tab header rides the property — the XAML carries
        // no strings).
        ui_patterns::tab_header(&self.script_tab, &t!("script_tab"));
        self.script_file_caption.set_text(Some(&t!("script_file")));
        self.script_new.set_content(Some(Rc::new(t!("script_new").to_string())));
        self.script_save.set_content(Some(Rc::new(t!("script_save").to_string())));
        self.script_delete.set_content(Some(Rc::new(t!("script_delete").to_string())));
        self.script_run.set_content(Some(Rc::new(t!("script_run").to_string())));
        self.script_stop.set_content(Some(Rc::new(t!("script_stop").to_string())));
        self.script_name_box.set_placeholder_text(Some(&t!("script_name_hint")));
        self.tx_hook_caption.set_text(Some(&t!("tx_hook")));
        self.tx_hook_box.set_placeholder_text(Some(&t!("tx_hook_hint")));
        self.tx_hook_enable.set_content(Some(Rc::new(t!("tx_hook_enable").to_string())));
        self.script_editor.set_placeholder_text(Some(&t!("script_editor_hint")));

        // The Open/Close label tracks the active tab's port state.
        let active = self.active_vm();
        self.set_open_state(active.map(|vm| vm.borrow().state).unwrap_or(PortState::Closed));
        self.sync_chip_banner();
        self.sync_hook_badge();
        self.sync_tab_headers();
    }

    fn fill_static_selectors(&self) {
        // Baud rates (the persisted launch default preselects — ticket #25).
        let items = self.baud_selector.items();
        for rate in BAUD_RATES {
            let value: BoxedValue = Rc::new(rate.to_string());
            items.add(Some(value));
        }
        let wanted = super::settings::global()
            .map(|settings| settings.resolved_baud())
            .unwrap_or(115_200);
        let default = BAUD_RATES
            .iter()
            .position(|rate| *rate == wanted)
            .unwrap_or_else(|| BAUD_RATES.iter().position(|rate| *rate == 115_200).unwrap_or(0));
        self.baud_selector.set_selected_index(default as i32);

        // Encodings (one entry per TextEncoding, persisted default first).
        let items = self.encoding_selector.items();
        for encoding in TextEncoding::all() {
            let value: BoxedValue = Rc::new(encoding.display_name().to_string());
            items.add(Some(value));
        }
        let wanted = super::settings::global()
            .map(|settings| settings.resolved_encoding())
            .unwrap_or(TextEncoding::Utf8);
        let index = TextEncoding::all()
            .iter()
            .position(|encoding| *encoding == wanted)
            .unwrap_or(0);
        self.encoding_selector.set_selected_index(index as i32);

        // View modes (labels set in apply_ui_strings for the live language).
        self.view_mode_selector.set_selected_index(0);
        self.mock_speed.set_selected_index(2); // 1×
    }

    /// The controller's own `Rc` (the pump holds it; every path that needs a
    /// handle clone runs after `start_pump` — the attach order guarantees it).
    fn self_handle(&self) -> Option<Rc<Self>> {
        PUMP.with(|pump| pump.borrow().controller.clone())
    }

    /// The selected baud rate, defaulting to 115200 while nothing is selected.
    fn selected_baud(&self) -> u32 {
        let index = self.baud_selector.selected_index();
        if index >= 0 {
            BAUD_RATES.get(index as usize).copied().unwrap_or(115_200)
        } else {
            115_200
        }
    }

    // ---- The session tab strip (§3.6) ----------------------------------------

    /// Wires the tab strip's events and mounts the picker flyout.
    fn setup_tab_strip(&self) {
        let this = self.self_handle().expect("controller handle (pump running)");
        self.session_tabs.selection_changed(move |_, _| {
            let index = this.session_tabs.selected_index();
            if index >= 0 {
                this.activate(index as usize);
            }
        });
    }

    /// The `+` flow: a fresh unbound tab, then the port picker flyout (the
    /// picker's buttons open the port into the new tab). The tab strip's
    /// selection change drives [`Self::activate`].
    fn add_tab(&self) -> Rc<RefCell<SessionVm>> {
        let tab_id = self.next_tab_id.get();
        self.next_tab_id.set(tab_id + 1);
        let vm = Rc::new(RefCell::new(SessionVm::new(tab_id)));
        self.tabs.borrow_mut().push(Rc::clone(&vm));

        let index = self.tabs.borrow().len() - 1;
        let item = TabItem::new();
        let header = vm.borrow().tab_caption();
        ui_patterns::tab_header(&item, &header);
        let boxed: BoxedValue = Control::boxed(item.clone());
        self.session_tabs.items().add(Some(boxed));
        self.session_tabs.set_selected_index(index as i32);
        self.sync_tab_headers();
        vm
    }

    /// The active tab's view-model.
    fn active_vm(&self) -> Option<Rc<RefCell<SessionVm>>> {
        self.tabs.borrow().get(self.active.get()).cloned()
    }

    /// Switches the shared controls to tab `index` (the toolbar, log pane,
    /// quick-send and bottom pane switch with the active tab — §3.6).
    fn activate(&self, index: usize) {
        if index >= self.tabs.borrow().len() {
            return;
        }
        self.active.set(index);
        self.sync_from_active();
    }

    /// Applies the active VM's presentation state to the shared controls and
    /// re-renders its view. The `syncing` guard keeps the control-event
    /// handlers (checkbox toggles, selection changes) from re-entering; the
    /// state is snapshotted in one borrow scope, then applied (the appliers
    /// re-borrow the tab).
    fn sync_from_active(&self) {
        self.syncing.set(true);
        let Some(vm) = self.active_vm() else {
            self.syncing.set(false);
            return;
        };
        let (follow, view_mode, filter, timestamps, hide_sent) = {
            let vm = vm.borrow();
            (vm.follow, vm.view_mode, vm.filter.clone(), vm.timestamps, vm.hide_sent)
        };
        // Log toolbar.
        self.follow_toggle.set_is_checked(Some(follow));
        self.view_mode_selector.set_selected_index(view_mode.index());
        self.filter_box.set_text(Some(&filter));
        self.timestamps_toggle.set_is_checked(Some(timestamps));
        self.hide_sent.set_is_checked(Some(hide_sent));

        // Chip + banner + open/close.
        let (state, open_pending, encoding, speed_index, quick_page, hook_armed) = {
            let vm = vm.borrow();
            (vm.state, vm.open_pending, vm.encoding, vm.mock_speed_index, vm.quick_page, vm.hook_armed)
        };
        self.set_open_state(state);
        self.open_close.set_is_enabled(!open_pending);

        // Session toolbar: encoding + the port listing (bound port preselected).
        let encoding_index = TextEncoding::all()
            .iter()
            .position(|candidate| *candidate == encoding)
            .unwrap_or(0) as i32;
        self.encoding_selector.set_selected_index(encoding_index);
        self.show_ports_in_selector();
        self.mock_speed.set_selected_index(speed_index as i32);
        let (paused, framed, mock, drawer_open) = {
            let vm = vm.borrow();
            (
                vm.mock_paused,
                vm.port.as_ref().is_some_and(|info| {
                    matches!(info.kind, rcom::core::port::PortKind::Mock(MockScenario::Framed))
                }),
                vm.is_mock(),
                vm.mock_drawer_open,
            )
        };
        self.mock_pause.set_content(Some(Rc::new(
            if paused { t!("mock_resume").to_string() } else { t!("mock_pause").to_string() },
        )));
        self.mock_trigger.set_content(Some(Rc::new(
            if framed { t!("mock_trigger_corrupt").to_string() } else { t!("mock_trigger_unplug").to_string() },
        )));
        self.mock_drawer_button.set_is_visible(mock);
        self.mock_drawer.set_is_visible(mock && drawer_open);

        // Send bar.
        self.hex_send.set_is_checked(Some(false));

        // Quick-send: the tab's own store/page/editing state.
        self.quick_pager.set_selected_page_index(quick_page as i32);
        self.update_quick_page_label();
        self.sync_hook_badge();
        MainController::rebuild_quick_entries(self);

        // The script page's per-session hook row.
        self.tx_hook_enable.set_is_checked(Some(hook_armed));

        // Re-render the log from the model.
        self.render_view();
        self.syncing.set(false);
        self.update_shown_status();
    }

    /// Re-renders the tab captions (the ●/○ state markers move with the chip).
    fn sync_tab_headers(&self) {
        let tabs = self.tabs.borrow();
        for (index, vm) in tabs.iter().enumerate() {
            let Some(item) = self.session_tab_item(index) else { continue };
            ui_patterns::tab_header(&item, &vm.borrow().tab_caption());
        }
    }

    /// The TabItem of tab `index`, if the strip has it mounted.
    fn session_tab_item(&self, index: usize) -> Option<Ref<TabItem>> {
        self.session_tabs
            .container_from_index(index as i32)
            .and_then(|container| container.cast::<TabItem>())
    }

    /// The ✕ handler: a tab with an open port confirms first (§4.1's
    /// destructive list — "close session tab with open ports"); a closed tab
    /// closes straight away.
    fn close_tab_clicked(&self) {
        if self.open_session_count() == 0 {
            self.close_active_tab();
            return;
        }
        let flyout = {
            let mut mounted = self.close_tab_flyout.borrow_mut();
            match mounted.as_ref() {
                Some(flyout) => flyout.clone(),
                None => {
                    let flyout = Flyout::new();
                    let panel = StackPanel::new();
                    panel.set_orientation(Orientation::Vertical);
                    panel.set_spacing(12.0);
                    let text = TextBlock::new();
                    text.set_text(Some(&t!("confirm_close_tab")));
                    text.set_max_width(280.0);
                    text.set_text_wrapping(ferroui_base::media::TextWrapping::Wrap);
                    panel.children().add(text.upcast::<Control>());
                    let row = StackPanel::new();
                    row.set_orientation(Orientation::Horizontal);
                    row.set_spacing(8.0);
                    row.set_horizontal_alignment(ferroui_base::layout::HorizontalAlignment::Right);
                    let cancel = Button::new();
                    cancel.set_content(Some(Rc::new(t!("cancel").to_string())));
                    let flyout_for_cancel = flyout.clone();
                    cancel.add_handler(Button::click_event(), move |_, _| {
                        flyout_for_cancel.set_is_open(false);
                    });
                    row.children().add(cancel.upcast::<Control>());
                    let confirm = Button::new();
                    confirm.set_content(Some(Rc::new(t!("close_tab").to_string())));
                    ui_patterns::accent(&confirm);
                    let this = self.self_handle().expect("controller handle (pump running)");
                    let flyout_for_confirm = flyout.clone();
                    confirm.add_handler(Button::click_event(), move |_, _| {
                        flyout_for_confirm.set_is_open(false);
                        this.close_active_tab();
                    });
                    row.children().add(confirm.upcast::<Control>());
                    panel.children().add(row.upcast::<Control>());
                    flyout.set_content(Some(Control::boxed(panel)));
                    *mounted = Some(flyout.clone());
                    flyout
                }
            }
        };
        flyout.show_at(&self.close_tab);
    }

    /// Closes the active tab: an open port rides the confirm above (Mechanism
    /// B — the tab's port closes, the session drops, never hangs).
    fn close_active_tab(&self) {
        let index = self.active.get();
        let Some(vm) = self.active_vm() else { return };
        let session_id = vm.borrow().current_session();
        let was_last = self.tabs.borrow().len() <= 1;

        if let Some(session_id) = session_id {
            core_host().commands.send(Command::CloseSession { session_id }).ok();
        }
        // Drop the tab (the model goes with it — a closed session's rows are
        // the session's story; reopening the port starts a fresh session).
        let dropped_tab_id = vm.borrow().tab_id;
        self.tabs.borrow_mut().remove(index);
        self.pending_opens
            .borrow_mut()
            .retain(|_, tab| *tab != dropped_tab_id);
        let items = self.session_tabs.items();
        items.remove_at(index);
        let count = self.tabs.borrow().len();
        if was_last {
            // Never zero tabs: the shell is the strip; a fresh unbound tab
            // takes the closed one's place.
            self.add_tab();
        } else {
            let next = index.min(count - 1);
            self.session_tabs.set_selected_index(next as i32);
            self.activate(next);
        }
        self.sync_tab_headers();
    }

    /// Opens `port` into the active tab (the picker's path): the tab binds the
    /// port when the Open event arrives, adopting the session id into its
    /// lineage.
    fn open_in_active_tab(&self, port: &PortInfo) {
        let Some(vm) = self.active_vm() else { return };
        {
            let mut vm = vm.borrow_mut();
            vm.title = port.name.clone();
            vm.port = Some(port.clone());
            vm.open_pending = true;
            // Ticket #35: the tab's quick-send set re-keys to this port's
            // override layer (the shared default set until the port owns one);
            // page state resets with it.
            vm.quick = QuickSendStore::load_for(&port.name);
            vm.quick_page = 0;
            vm.quick_editing = None;
            // A hook armed before this open flushes to the session on bind.
        }
        // The panel reflects the re-keyed set immediately.
        self.quick_pager.set_selected_page_index(0);
        self.update_quick_page_label();
        self.rebuild_quick_entries();
        let tab_id = vm.borrow().tab_id;
        self.pending_opens.borrow_mut().insert(port.name.clone(), tab_id);
        // Busy rule (§4.1): the open button greys until the open's outcome
        // lands; the >500 ms reveal arms for the long future.
        self.open_close.set_is_enabled(false);
        self.arm_open_progress();
        self.sync_tab_headers();
        core_host()
            .commands
            .send(Command::Open { name: port.name.clone(), baud: self.selected_baud() })
            .ok();
    }

    fn arm_open_progress(&self) {
        let guard = OpenProgressGuard::arm(&self.open_progress);
        *self.open_progress_guard.borrow_mut() = Some(Rc::new(guard));
    }

    fn disarm_open_progress(&self) {
        *self.open_progress_guard.borrow_mut() = None;
        self.open_progress.set_is_visible(false);
    }

    // ---- Port listing --------------------------------------------------------

    fn refresh_ports(&self) {
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    /// A fresh port listing: the active tab's selector, the `+` picker, the
    /// settings page's default-port picker.
    fn show_ports(&self, ports: Vec<PortInfo>) {
        *self.ports.borrow_mut() = ports;
        self.show_ports_in_selector();
        self.rebuild_picker();
        let ports = self.ports.borrow().clone();
        // ISSUE #25: the settings page's default-port picker shows the same
        // listing.
        tools_settings::apply_ports(&ports);
        // Ticket #25: the persisted launch-default port preselects once (the
        // first listing after launch); afterwards the user's choice wins.
        if !self.port_preset_done.get() {
            self.port_preset_done.set(true);
            if let Some(default_port) = super::settings::global().and_then(|settings| settings.default_port.clone()) {
                if let Some(index) = ports.iter().position(|info| info.name == default_port) {
                    self.port_selector.set_selected_index(index as i32);
                }
            }
        }
    }

    /// Fills the active tab's port selector (keeping the bound port
    /// preselected). The mock scenario rides the label — the picker is where a
    /// mock port explains itself (§5.7's port badge).
    fn show_ports_in_selector(&self) {
        let ports = self.ports.borrow();
        let bound = self
            .active_vm()
            .and_then(|vm| vm.borrow().port.as_ref().map(|info| info.name.clone()));
        let remember = self.port_selector.selected_index();
        let count = ports.len() as i32;
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (same guard as the settings page).
        self.port_selector.set_selected_index(-1);
        let items = self.port_selector.items();
        items.clear();
        for info in ports.iter() {
            let name = port_label(info);
            let value: BoxedValue = Rc::new(name);
            items.add(Some(value));
        }
        let kept = bound
            .and_then(|name| ports.iter().position(|info| info.name == name).map(|index| index as i32))
            .unwrap_or(if remember >= 0 && remember < count { remember } else if count > 0 { 0 } else { -1 });
        self.port_selector.set_selected_index(kept);
    }

    /// The `+` picker's flyout: one button per listed port.
    fn rebuild_picker(&self) {
        let panel = StackPanel::new();
        panel.set_orientation(Orientation::Vertical);
        panel.set_spacing(4.0);

        let title = TextBlock::new();
        title.set_text(Some(&t!("port_picker_title")));
        title.set_font_size(12.0);
        panel.children().add(title.upcast::<Control>());

        let ports = self.ports.borrow().clone();
        if ports.is_empty() {
            let empty = TextBlock::new();
            empty.set_text(Some(&t!("port_picker_empty")));
            empty.set_font_size(12.0);
            empty.set_opacity(0.6);
            panel.children().add(empty.upcast::<Control>());
        }
        for info in ports {
            let label = port_label(&info);
            let this = self.self_handle().expect("controller handle (pump running)");
            let button = Button::new();
            button.set_content(Some(Rc::new(label)));
            button.set_min_width(240.0);
            button.add_handler(Button::click_event(), move |_, _| {
                // A fresh tab per pick (the `+` flow: port picker → session).
                this.add_tab();
                this.open_in_active_tab(&info);
            });
            panel.children().add(button.upcast::<Control>());
        }
        let mut mounted = self.picker_flyout.borrow_mut();
        let flyout = match mounted.as_ref() {
            Some(flyout) => flyout.clone(),
            None => {
                let flyout = Flyout::new();
                self.new_tab.set_flyout(&flyout);
                *mounted = Some(flyout.clone());
                flyout
            }
        };
        drop(mounted);
        flyout.set_content(Some(Control::boxed(panel)));
    }

    // ---- The log view (§3.3) -------------------------------------------------

    /// Appends a rendered row to the live view (the active tab's visible line).
    fn append_rendered(&self, line: &LogLine) {
        let Some(vm) = self.active_vm() else { return };
        let (mode, encoding, timestamps, follow) = {
            let vm = vm.borrow();
            (vm.view_mode, vm.encoding, vm.timestamps, vm.follow)
        };
        let row = logview::build_row(line, mode, encoding, timestamps);
        self.log_rows.children().add(row.upcast::<Control>());
        if follow {
            self.log_scroller.scroll_to_end();
        }
    }

    /// Re-renders the whole view from the active tab's model under its
    /// presentation state — the one full re-render the encoding switch, the
    /// filter, hide-sent and the view-mode flips share (§3.3: no new traffic).
    fn render_view(&self) {
        let Some(vm) = self.active_vm() else { return };
        let (mode, encoding, timestamps, follow) = {
            let vm = vm.borrow();
            (vm.view_mode, vm.encoding, vm.timestamps, vm.follow)
        };
        let children = self.log_rows.children();
        children.clear();
        let lines: Vec<LogLine> = {
            let vm = vm.borrow();
            vm.visible_lines().into_iter().cloned().collect()
        };
        for line in &lines {
            let row = logview::build_row(line, mode, encoding, timestamps);
            children.add(row.upcast::<Control>());
        }
        if follow {
            self.log_scroller.scroll_to_end();
        }
        self.update_shown_status();
    }

    fn update_shown_status(&self) {
        let Some(vm) = self.active_vm() else { return };
        let ((shown, total), follow) = {
            let vm = vm.borrow();
            (vm.shown_status(), vm.follow)
        };
        let word = if follow { t!("follow_on") } else { t!("follow_paused") };
        let status = t!("shown_status", shown = shown, total = total);
        self.shown_status.set_text(Some(&format!("{status} · {word}")));
    }

    fn set_follow(&self, on: bool) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().follow = on;
        if on {
            self.log_scroller.scroll_to_end();
        }
        self.sync_follow_status_word();
    }

    /// The status word of the follow model: `follow on` / `PAUSED` (scrolling
    /// up auto-disengages; no pause-on-hover, §3.3).
    fn sync_follow_status_word(&self) {
        self.update_shown_status();
    }

    fn set_hide_sent(&self, on: bool) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().hide_sent = on;
        self.render_view();
    }

    fn set_timestamps(&self, on: bool) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().timestamps = on;
        self.render_view();
    }

    fn set_filter(&self, filter: String) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().filter = filter;
        self.render_view();
    }

    fn set_view_mode(&self, mode: ViewMode) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().view_mode = mode;
        self.render_view();
    }

    fn clear_log(&self) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().lines.clear();
        // Not confirmed (§4.1's not-confirmed list — the file sink keeps the
        // durable copy).
        self.render_view();
    }

    /// Applies an encoding chosen in the selector to the active tab: the raw
    /// bytes stayed in the model, so history decodes anew (contract §1.3).
    fn set_encoding(&self, encoding: TextEncoding) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().encoding = encoding;
        self.render_view();
        self.autodemo_encoding_check();
    }

    /// `UiMsg::SetEncoding` (the encoding autodemo): same path as the selector.
    fn apply_set_encoding(&self, encoding: TextEncoding) {
        let index = TextEncoding::all()
            .iter()
            .position(|candidate| *candidate == encoding)
            .map(|index| index as i32);
        let Some(index) = index else { return };
        if self.encoding_selector.selected_index() == index {
            self.set_encoding(encoding);
        } else {
            // Fires selection_changed → `set_encoding` + the check.
            self.encoding_selector.set_selected_index(index);
        }
    }

    /// Headless proof for `RCOM_AUTODEMO=encoding`: after the switch, the retained
    /// GBK bytes must decode to the sent text (the mock's echo shares them).
    fn autodemo_encoding_check(&self) {
        if AutodemoMode::from_env() != AutodemoMode::Encoding {
            return;
        }
        let Some(vm) = self.active_vm() else { return };
        let vm = vm.borrow();
        if vm.encoding != TextEncoding::Gbk {
            return;
        }
        let expected = TextEncoding::Gbk.encode("你好，rcom\r\n");
        let rendered = vm.lines.iter().any(|line| match line {
            LogLine::Data { bytes, .. } if *bytes == expected => {
                TextEncoding::Gbk.decode(bytes).contains("你好，rcom")
            }
            _ => false,
        });
        if rendered {
            tracing::info!(
                "AUTODEMO ENCODING OK: GBK bytes re-rendered as '你好，rcom' after the \
                 encoding switch (raw bytes retained per row, decode at render time)"
            );
        } else {
            tracing::error!("AUTODEMO ENCODING FAILED: the GBK row did not re-render");
        }
    }

    // ---- Send paths (§3.4/§3.5) ----------------------------------------------

    /// Send button / Enter in the send box: the box's text encoded with the
    /// session's encoding (or parsed raw under HEX — parsing switches, not
    /// presentation). A bad hex input becomes a note row with the offending
    /// text, never a silent fail (§3.4).
    fn send_box_text(&self) {
        let Some(text) = self.send_box.text() else { return };
        if text.is_empty() {
            return;
        }
        let hex_mode = self.hex_send.is_checked() == Some(true);
        let bytes = if hex_mode {
            match parse_hex(&text) {
                Ok(bytes) if !bytes.is_empty() => Ok(bytes),
                Ok(_) => Err(text.clone()),
                Err(error) => Err(format!("{text} ({error})")),
            }
        } else {
            let encoding = self.active_vm().map(|vm| vm.borrow().encoding).unwrap_or_default();
            Ok(encoding.encode(&text))
        };
        match bytes {
            Ok(bytes) => {
                let session_id = self.active_vm().and_then(|vm| vm.borrow().current_session());
                if core_host()
                    .commands
                    .send(Command::SendBytes { session_id, bytes })
                    .is_ok()
                {
                    self.send_box.set_text(None);
                }
            }
            Err(offending) => {
                let message = t!("invalid_hex_input", input = &offending).to_string();
                self.append_note(&message, true);
            }
        }
    }

    /// One quick-send entry, clicked: its payload to the same TX path as the
    /// send box — text encodes with the session's encoding, hex parses to raw
    /// bytes (the hook applies — entries ride the session TX path, §3.5). The
    /// store's current payload wins (edit-in-place writes through).
    fn send_quick_entry(&self, entry: QuickSendEntry) {
        let Some(vm) = self.active_vm() else { return };
        let (encoding, session_id) = {
            let vm = vm.borrow();
            (vm.encoding, vm.current_session())
        };
        // The row button passes the label/hex flag; the payload comes from the
        // store (the edit-in-place value).
        let payload = {
            let vm = vm.borrow();
            vm.quick
                .pages
                .get(vm.quick_page)
                .and_then(|page| page.entries.iter().find(|candidate| candidate.label == entry.label))
                .map(|candidate| candidate.payload.clone())
                .unwrap_or(entry.payload)
        };
        let entry = QuickSendEntry { payload, ..entry };
        let bytes = if entry.hex {
            parse_hex(&entry.payload)
        } else {
            Ok(encoding.encode(&entry.payload))
        };
        match bytes {
            Ok(bytes) => {
                core_host()
                    .commands
                    .send(Command::SendBytes { session_id, bytes })
                    .ok();
            }
            Err(error) => {
                let message = t!("invalid_hex_payload", error = &error).to_string();
                self.append_note(&message, true);
            }
        }
    }

    /// A note row (##/!! diagnostics) onto the active tab: the log is the
    /// durable narrative (§4.1's standing rule — every error lands as a row).
    fn append_note(&self, text: &str, error: bool) {
        let Some(vm) = self.active_vm() else { return };
        let visible = vm.borrow_mut().push_line(LogLine::Note { text: text.to_string(), error });
        if visible {
            self.append_rendered(&LogLine::Note { text: text.to_string(), error });
        }
    }

    fn set_open_state(&self, state: PortState) {
        // A session being watched is still alive from the UI's point of view:
        // `close()` must stay available to cancel the auto-reconnect (contract §3:
        // `WatchDesired → Closed | user close()`). The hotplug poller's global
        // Arrived/Removed events never touch the button.
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().state = state;
        let active = vm.borrow().is_active_state();
        let label = if active { t!("close") } else { t!("open") };
        self.open_close.set_content(Some(Rc::new(label.to_string())));
        self.sync_chip_banner();
        self.sync_tab_headers();
    }

    /// The chip + banner (contract §5.4): the chip colours with the state; the
    /// amber "waiting for port…" banner shows only in WatchDesired; the quit
    /// guard's banner shares the row (one-click-past, Mechanism B).
    fn sync_chip_banner(&self) {
        let Some(vm) = self.active_vm() else { return };
        let vm = vm.borrow();
        let (background, foreground) = chip_colors(vm.state);
        self.state_chip.set_value(
            Border::background_property(),
            Some(SolidColorBrush::with_color(background).into()),
        );
        self.state_chip_text.set_value(
            TextBlock::foreground_property(),
            Some(SolidColorBrush::with_color(foreground).into()),
        );
        let label = match vm.state {
            PortState::Open | PortState::Reopened => t!("chip_open"),
            PortState::WatchDesired => t!("chip_watch"),
            _ => t!("chip_closed"),
        };
        self.state_chip_text.set_text(Some(&label));

        let watching = vm.state == PortState::WatchDesired;
        let quit_banner = self.quit_guard_passed.get() && self.open_session_count() > 0;
        self.banner_panel.set_is_visible(watching || quit_banner);
        if watching {
            self.banner_text.set_text(Some(&t!("banner_waiting", name = &vm.title)));
            self.banner_text
                .set_value(TextBlock::foreground_property(), Some(SolidColorBrush::with_color(logview::note_color()).into()));
        } else if quit_banner {
            let count = self.open_session_count();
            self.banner_text.set_text(Some(&t!("quit_guard_text", count = count)));
            self.banner_text
                .set_value(TextBlock::foreground_property(), Some(SolidColorBrush::with_color(logview::note_color()).into()));
        }
        self.banner_close_all.set_is_visible(quit_banner);
    }

    /// How many tabs hold an active port (the quit guard's count).
    fn open_session_count(&self) -> usize {
        self.tabs
            .borrow()
            .iter()
            .filter(|vm| vm.borrow().is_active_state())
            .count()
    }

    /// The quick-send header's hook badge (§3.5): `⌁ hook` (amber on
    /// translucent amber) while the session's pre-send hook is armed, `hook
    /// off` (dim) otherwise — flips reactively with the hook state.
    fn sync_hook_badge(&self) {
        let Some(vm) = self.active_vm() else { return };
        let (text, lit) = vm.borrow().hook_badge();
        self.quick_hook_badge_text.set_text(Some(text));
        let (bg, fg) = if lit {
            (Color::from_argb(0x38, 0xF7, 0xB2, 0x4B), logview::note_color())
        } else {
            (Color::from_argb(0x14, 0xFF, 0xFF, 0xFF), Color::from_argb(0x87, 0xFF, 0xFF, 0xFF))
        };
        self.quick_hook_badge.set_value(
            Border::background_property(),
            Some(SolidColorBrush::with_color(bg).into()),
        );
        self.quick_hook_badge_text.set_value(
            TextBlock::foreground_property(),
            Some(SolidColorBrush::with_color(fg).into()),
        );
    }

    // ---- The quick-send panel -----------------------------------------------

    fn update_quick_page_label(&self) {
        let page = self.active_vm().map(|vm| vm.borrow().quick_page).unwrap_or(0);
        let label = t!("page_indicator", page = page + 1, total = PAGE_COUNT);
        self.quick_page_label.set_text(Some(&label));
    }

    /// Rebuilds the entry rows of the active tab's current page (add/rename/
    /// delete and page switches all land here).
    fn rebuild_quick_entries(&self) {
        let Some(vm) = self.active_vm() else { return };
        let entries: Vec<QuickSendEntry> = {
            let vm = vm.borrow();
            vm.quick
                .pages
                .get(vm.quick_page)
                .map(|page| page.entries.clone())
                .unwrap_or_default()
        };
        let children = self.quick_entries.children();
        children.clear();
        if entries.is_empty() {
            let hint = TextBlock::new();
            hint.set_text(Some(&t!("quick_empty")));
            children.add(hint.upcast::<Control>());
            return;
        }
        let this = self.self_handle().expect("controller handle (pump running)");
        for (index, entry) in entries.iter().enumerate() {
            children.add(Self::build_quick_entry_row(&this, index, entry).upcast::<Control>());
        }
    }

    /// One entry row (§3.5): `label │ value │ send` — **edit-in-place is the
    /// whole interaction**: the value is a TextBox, always; Enter in a value
    /// sends it; an edit writes through to the tab's store (in memory — the
    /// persisted profile updates on the save/add/delete paths), so paging
    /// re-keys the visible rows, not the values.
    fn build_quick_entry_row(this: &Rc<Self>, index: usize, entry: &QuickSendEntry) -> Ref<StackPanel> {
        let row = StackPanel::new();
        row.set_orientation(Orientation::Horizontal);
        row.set_spacing(4.0);

        let label = TextBlock::new();
        let mut label_text = entry.label.clone();
        if entry.hex {
            label_text = format!("{} {}", t!("quick_hex_mark"), label_text);
        }
        label.set_text(Some(&label_text));
        label.set_font_size(11.0);
        label.set_opacity(0.75);
        label.set_vertical_alignment(ferroui_base::layout::VerticalAlignment::Center);
        let label_column = label.clone();
        label_column.set_min_width(72.0);
        row.children().add(label.upcast::<Control>());

        let value = TextBox::new();
        value.set_text(Some(&entry.payload));
        value.set_font_family(logview::mono());
        value.set_font_size(12.0);
        {
            let this_for_edit = Rc::clone(this);
            let value_for_edit = value.clone();
            value.text_changed(move |_, _| {
                if let Some(text) = value_for_edit.text() {
                    this_for_edit.edit_quick_entry_value(index, text);
                }
            });
            let this_for_enter = Rc::clone(this);
            let entry_for_enter = entry.clone();
            let value_for_enter = value.clone();
            value.key_down(move |_, args| {
                if args.key == Key::Enter {
                    args.set_handled(true);
                    let payload = value_for_enter.text().unwrap_or_default();
                    if !payload.is_empty() {
                        let mut sent = entry_for_enter.clone();
                        sent.payload = payload;
                        this_for_enter.send_quick_entry(sent);
                    }
                }
            });
        }
        row.children().add(value.upcast::<Control>());

        let send = Button::new();
        send.set_content(Some(Rc::new(t!("send").to_string())));
        send.set_min_width(56.0);
        let this_for_send = Rc::clone(this);
        let entry_for_send = entry.clone();
        send.add_handler(Button::click_event(), move |_, _| {
            let payload = entry_for_send.payload.clone();
            let mut sent = entry_for_send.clone();
            sent.payload = payload;
            // The send reads the *edited* value (the store holds it).
            this_for_send.send_quick_entry(sent);
        });
        row.children().add(send.upcast::<Control>());

        // The edit button still loads the entry into the editor below the list
        // (rename / hex-flag / delete live there).
        let edit = Button::new();
        edit.set_content(Some(Rc::new(t!("quick_edit").to_string())));
        edit.set_min_width(32.0);
        edit.add_handler(Button::click_event(), {
            let this = Rc::clone(this);
            move |_, _| this.load_quick_entry(index)
        });
        row.children().add(edit.upcast::<Control>());

        row
    }

    /// An edit-in-place value write-through: the tab's store holds the value
    /// (in memory; the profile persists on the save/add/delete paths — §3.5's
    /// "each entry owns its value state").
    fn edit_quick_entry_value(&self, index: usize, payload: String) {
        let Some(vm) = self.active_vm() else { return };
        let page = {
            let vm = vm.borrow();
            vm.quick_page
        };
        let mut guard = vm.borrow_mut();
        if let Some(slot) = guard.quick.pages.get_mut(page) {
            if let Some(entry) = slot.entries.get_mut(index) {
                entry.payload = payload;
            }
        }
    }

    /// Loads an entry into the editor boxes for rename/delete.
    fn load_quick_entry(&self, index: usize) {
        let Some(vm) = self.active_vm() else { return };
        let entry = {
            let vm = vm.borrow();
            vm.quick
                .pages
                .get(vm.quick_page)
                .and_then(|page| page.entries.get(index).cloned())
        };
        let Some(entry) = entry else { return };
        vm.borrow_mut().quick_editing = Some(index);
        self.quick_label_box.set_text(Some(&entry.label));
        self.quick_payload_box.set_text(Some(&entry.payload));
        self.quick_hex_check.set_is_checked(Some(entry.hex));
    }

    /// The entry the editor currently describes; `None` while the payload is empty.
    fn entry_from_editor(&self) -> Option<QuickSendEntry> {
        let payload = self.quick_payload_box.text()?;
        if payload.is_empty() {
            return None;
        }
        let label = self
            .quick_label_box
            .text()
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| payload.clone());
        Some(QuickSendEntry {
            label,
            payload,
            hex: self.quick_hex_check.is_checked().unwrap_or(false),
        })
    }

    fn add_quick_entry(&self) {
        let Some(entry) = self.entry_from_editor() else { return };
        let Some(vm) = self.active_vm() else { return };
        let page = vm.borrow().quick_page;
        if let Some(slot) = vm.borrow_mut().quick.pages.get_mut(page) {
            slot.entries.push(entry);
        }
        self.persist_quick_entries();
    }

    fn save_quick_entry(&self) {
        let Some(vm) = self.active_vm() else { return };
        let (page, editing) = {
            let vm = vm.borrow();
            (vm.quick_page, vm.quick_editing)
        };
        let Some(index) = editing else { return };
        let Some(entry) = self.entry_from_editor() else { return };
        if let Some(slot) = vm.borrow_mut().quick.pages.get_mut(page) {
            if let Some(target) = slot.entries.get_mut(index) {
                *target = entry;
            }
        }
        self.persist_quick_entries();
    }

    fn delete_quick_entry(&self) {
        let Some(vm) = self.active_vm() else { return };
        let (page, editing) = {
            let vm = vm.borrow();
            (vm.quick_page, vm.quick_editing)
        };
        let Some(index) = editing else { return };
        // Quick-send entry delete is not confirmed (§4.1's not-confirmed list).
        if let Some(slot) = vm.borrow_mut().quick.pages.get_mut(page) {
            if index < slot.entries.len() {
                slot.entries.remove(index);
            }
        }
        vm.borrow_mut().quick_editing = None;
        self.quick_label_box.set_text(None);
        self.quick_payload_box.set_text(None);
        self.quick_hex_check.set_is_checked(Some(false));
        self.persist_quick_entries();
    }

    /// Persists and refreshes the panel after any mutation. A tab bound to a
    /// port writes that port's own override file (ticket #35's per-session
    /// quick-send sets); an unbound tab writes the shared default profile.
    fn persist_quick_entries(&self) {
        let Some(vm) = self.active_vm() else { return };
        let port = vm.borrow().port.as_ref().map(|info| info.name.clone());
        vm.borrow().quick.save_for(port.as_deref());
        if let Some(vm) = self.active_vm() {
            vm.borrow_mut().quick_editing = None;
        }
        self.rebuild_quick_entries();
    }

    fn show_quick_page(&self, page: usize) {
        let Some(vm) = self.active_vm() else { return };
        let page = page.min(PAGE_COUNT - 1);
        {
            let mut vm = vm.borrow_mut();
            vm.quick_page = page;
            vm.quick_editing = None;
        }
        self.quick_pager.set_selected_page_index(page as i32);
        self.update_quick_page_label();
        self.rebuild_quick_entries();
    }

    // ---- The script page (bottom pane) ---------------------------------------

    /// Editor shape + the picker's initial content + the delete confirm. The
    /// multiline flag and the monospaced font are set here, not in the XAML
    /// (not XAML-reliable at this FerroUI pin).
    fn setup_script_page(this: &Rc<Self>) {
        this.script_editor.set_accepts_return(true);
        this.script_editor
            .set_font_family(ferroui_base::media::FontFamily::new(SCRIPT_EDITOR_FONT));
        this.refresh_script_picker();
        // §4.1: exactly one accent primary action per page header — Run.
        ui_patterns::accent(&this.script_run);
        // Destructive-and-irreversible: deleting a saved script is confirmed
        // (§4.1's required list) — button-anchored flyout, one shared builder.
        ui_patterns::confirm_flyout(
            &this.script_delete,
            t!("confirm_delete_script").to_string(),
            "delete",
            {
                let this = Rc::clone(this);
                move || this.delete_script_confirmed()
            },
        );
    }

    /// Rebuilds the picker: the built-in samples first (translated display
    /// names — the sweep keeps even these out of the raw string set), then the
    /// saved scripts (`<config>/rcom/scripts/*.lua`), nothing selected.
    fn refresh_script_picker(&self) {
        // Deselect before the rebuild (see `show_ports_in_selector` — a
        // selection cannot survive an items clear at this pin).
        self.script_selector.set_selected_index(-1);
        let items = self.script_selector.items();
        items.clear();
        *self.scripts.borrow_mut() = scriptstore::list(scriptstore::scripts_dir().as_deref());
        for index in 0..scriptstore::SAMPLES.len() {
            let value: BoxedValue = Rc::new(scriptstore::sample_display_name(index));
            items.add(Some(value));
        }
        for name in self.scripts.borrow().iter() {
            let value: BoxedValue = Rc::new(name.clone());
            items.add(Some(value));
        }
        self.script_selector.set_selected_index(-1);
    }

    /// A picker selection: the script loads into the editor under its name
    /// (samples read from the binary, saved scripts from the store).
    fn load_selected_script(&self) {
        let index = self.script_selector.selected_index();
        if index < 0 {
            return;
        }
        let index = index as usize;
        let samples = scriptstore::SAMPLES.len();
        let loaded = if index < samples {
            // The sample loads under its translated display name (what the
            // pickers showed; saving it under that name is a regular save).
            let name = scriptstore::sample_display_name(index);
            let code = scriptstore::SAMPLES[index].code.to_string();
            Some((name, code))
        } else {
            let name = self.scripts.borrow().get(index - samples).cloned();
            name.and_then(|name| {
                scriptstore::load(scriptstore::scripts_dir().as_deref(), &name)
                    .map(|code| (name, code))
            })
        };
        let Some((name, code)) = loaded else {
            self.append_note(&t!("script_load_failed", name = "").to_string(), true);
            return;
        };
        self.script_name_box.set_text(Some(&name));
        self.script_editor.set_text(Some(&code));
    }

    /// New: a blank editor under a blank name (the picker selection clears).
    fn new_script(&self) {
        self.script_selector.set_selected_index(-1);
        self.script_name_box.set_text(None);
        self.script_editor.set_text(None);
    }

    /// Save(-as): the editor's code under the name box's text (typing a new name
    /// is the save-as; the sanitized name lands back in the box).
    fn save_script(&self) {
        let Some(code) = self.script_editor.text() else { return };
        if code.trim().is_empty() {
            return;
        }
        let name = self.script_name_box.text().unwrap_or_default();
        match scriptstore::save(scriptstore::scripts_dir().as_deref(), &name, &code) {
            Ok(saved) => {
                self.script_name_box.set_text(Some(&saved));
                self.append_note(&t!("script_saved", name = &saved).to_string(), false);
                self.refresh_script_picker();
            }
            Err(error) => {
                self.append_note(&t!("script_save_failed", error = &error).to_string(), true);
            }
        }
    }

    /// The confirmed delete (the flyout's verb): samples are read-only (refused
    /// with a note row, not confirmed).
    fn delete_script_confirmed(&self) {
        let Some(name) = self.script_name_box.text() else { return };
        if scriptstore::sample_by_name(&name).is_some() {
            self.append_note(&t!("script_sample_readonly").to_string(), false);
            return;
        }
        match scriptstore::delete(scriptstore::scripts_dir().as_deref(), &name) {
            Ok(()) => {
                self.append_note(&t!("script_deleted", name = &name).to_string(), false);
                self.new_script();
                self.refresh_script_picker();
            }
            Err(error) => {
                self.append_note(&t!("script_delete_failed", error = &error).to_string(), true);
            }
        }
    }

    /// Run: the editor's code to the driver — a fresh standalone VM boots with it
    /// (a previous one is stopped first); its output lands in the log as `~~` rows.
    fn run_script(&self) {
        let Some(code) = self.script_editor.text() else { return };
        if code.trim().is_empty() {
            return;
        }
        let name = self
            .script_name_box
            .text()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| t!("script_untitled").to_string());
        self.append_note(&t!("script_started", name = &name).to_string(), false);
        core_host().commands.send(Command::RunScript { code }).ok();
    }

    /// Stop: the running panel VM unwinds (instruction-count hook), timers abort.
    /// The driver confirms with a `~~` row.
    fn stop_script(&self) {
        core_host().commands.send(Command::StopScript).ok();
    }

    /// The TX hook affordance (per session): enable sends the box's conversion
    /// script to the active session (every UI send funnels through it);
    /// disable clears it. An empty script stays disabled — the AC's "default
    /// empty = disabled". Arming flips the quick-send badge; rewritten sends
    /// carry the `⌁hook` marker (the core flags them — §5.2).
    fn apply_tx_hook(&self) {
        if self.syncing.get() {
            return;
        }
        let Some(vm) = self.active_vm() else { return };
        let enabled = self.tx_hook_enable.is_checked() == Some(true);
        let script = self.tx_hook_box.text().unwrap_or_default();
        let session_id = vm.borrow().current_session();
        if enabled && script.trim().is_empty() {
            self.tx_hook_enable.set_is_checked(Some(false));
            if let Some(vm) = self.active_vm() {
                vm.borrow_mut().hook_armed = false;
            }
            self.append_note(&t!("tx_hook_empty").to_string(), false);
            core_host()
                .commands
                .send(Command::SetHook { session_id, script: None })
                .ok();
            self.sync_hook_badge();
            return;
        }
        match session_id {
            // Unbound tab: hold the hook until the open binds (flushed on the
            // Open event) — sending it manager-wide would leak to every tab.
            None => {
                if let Some(vm) = self.active_vm() {
                    let mut vm = vm.borrow_mut();
                    vm.hook_armed = enabled;
                    vm.pending_hook = Some(enabled.then_some(script));
                }
            }
            Some(session_id) => {
                if let Some(vm) = self.active_vm() {
                    vm.borrow_mut().hook_armed = enabled;
                }
                core_host()
                    .commands
                    .send(Command::SetHook {
                        session_id: Some(session_id),
                        script: enabled.then_some(script),
                    })
                    .ok();
            }
        }
        self.sync_hook_badge();
        // The confirmation rows (`tx_hook_on` / `tx_hook_off`) come back from the
        // driver as UiMsg rows.
    }

    // ---- The mock drawer (§6.1) ----------------------------------------------

    /// Playback controls against the active session's mock port. The drawer is
    /// visible only for mock sessions (keyed off `PortInfo::kind`); the actions
    /// ride `Command::MockAction` — the core resolves `mock_controls()`.
    fn mock_action(&self, action: MockAction) {
        let Some(vm) = self.active_vm() else { return };
        let Some(session_id) = vm.borrow().current_session() else { return };
        core_host()
            .commands
            .send(Command::MockAction { session_id, action })
            .ok();
    }

    fn toggle_mock_drawer(&self) {
        let Some(vm) = self.active_vm() else { return };
        let open = !vm.borrow().mock_drawer_open;
        vm.borrow_mut().mock_drawer_open = open;
        self.mock_drawer.set_is_visible(open && vm.borrow().is_mock());
    }

    fn set_mock_paused(&self, paused: bool) {
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().mock_paused = paused;
        self.mock_pause.set_content(Some(Rc::new(
            if paused { t!("mock_resume").to_string() } else { t!("mock_pause").to_string() },
        )));
        self.mock_action(MockAction::Pause(paused));
    }

    fn set_mock_speed_index(&self, index: usize) {
        let Some(speed) = MOCK_SPEEDS.get(index).copied() else { return };
        let Some(vm) = self.active_vm() else { return };
        vm.borrow_mut().mock_speed_index = index;
        self.mock_action(MockAction::Speed(speed));
    }

    fn mock_trigger_for_session(&self) {
        let Some(vm) = self.active_vm() else { return };
        let framed = vm
            .borrow()
            .port
            .as_ref()
            .is_some_and(|info| matches!(info.kind, rcom::core::port::PortKind::Mock(MockScenario::Framed)));
        let trigger = if framed {
            rcom::core::port::MockTrigger::CorruptFrame
        } else {
            rcom::core::port::MockTrigger::Unplug
        };
        self.mock_action(MockAction::Trigger(trigger));
    }

    fn mock_inject(&self) {
        let Some(text) = self.mock_inject_box.text() else { return };
        if text.is_empty() {
            return;
        }
        let Some(vm) = self.active_vm() else { return };
        let bytes = vm.borrow().encoding.encode(&text);
        self.mock_action(MockAction::InjectRx(bytes));
        self.mock_inject_box.set_text(None);
    }

    // ---- The quit guard (§4.1, Mechanism B) ----------------------------------

    /// `Window::closing` with open sessions cancels the close **once**, shows
    /// the non-modal amber banner (*"N ports open — close anyway?"* [Close
    /// all] [Stay]); a second closing passes through. One-click-past: the
    /// guard catches accidents, it does not nag.
    fn install_quit_guard(&self) {
        let this = self.self_handle().expect("controller handle (pump running)");
        let guard = self.window.closing(move |args| {
            // Programmatic closes (the smoke run's self-close, "Close all")
            // pass through: the guard catches user accidents, not the app.
            if args.is_programmatic() || this.quit_guard_passed.get() {
                return;
            }
            if this.open_session_count() == 0 {
                return;
            }
            this.quit_guard_passed.set(true);
            this.sync_chip_banner();
            args.set_cancel(true);
        });
        *self._closing_guard.borrow_mut() = Some(guard);
        let this = self.self_handle().expect("controller handle (pump running)");
        self.banner_close_all.add_handler(Button::click_event(), move |_, _| {
            core_host().commands.send(Command::CloseAll).ok();
            this.quit_guard_passed.set(true);
            this.sync_chip_banner();
            // Everything is closing: pass the guard and close for real.
            this.window.close();
        });
    }

    // ---- The pump ------------------------------------------------------------

    fn apply(&self, message: UiMsg) {
        match message {
            UiMsg::Row { session_id, text, direction } => {
                self.apply_row(session_id, text, direction)
            }
            UiMsg::OpenFailed { port, error } => self.apply_open_failed(&port, &error),
            UiMsg::Data(data) => self.apply_data(data),
            UiMsg::Ports(ports) => self.show_ports(ports),
            UiMsg::PortState { session_id, port, state, detail, row } => {
                self.apply_port_state(session_id, port, state, detail, row)
            }
            UiMsg::SetEncoding(encoding) => self.apply_set_encoding(encoding),
            // ISSUE #20 (one insertion point): the net tools page consumes its events.
            UiMsg::Net(net) => tools_net::apply_net(net),
            // ISSUE #21 (one insertion point): the MQTT tools page consumes its events.
            UiMsg::Mqtt(mqtt) => super::tools_mqtt::apply_mqtt(mqtt),
            // ISSUE #24 (one insertion point): the plot page consumes its events.
            UiMsg::Plot(plot) => super::tools_plot::apply_plot(plot),
            // ISSUE #23 (one insertion point): the monitor page consumes its events.
            UiMsg::Monitor(monitor) => super::tools_monitor::apply_monitor(monitor),
        }
    }

    /// A note row: `Some(session)` → that session's tab; `None` → the active
    /// tab (process-global rows: tool stories, the standalone script VM).
    fn apply_row(&self, session_id: Option<u64>, text: String, direction: Option<Direction>) {
        let _ = direction; // note rows carry no hex; direction stays a tag
        match session_id {
            Some(id) => {
                if let Some(vm) = self.vm_owning(id) {
                    let error = text.contains("!!");
                    self.push_line_to(&vm, LogLine::Note { text, error });
                }
            }
            None => {
                self.append_note(&text, text.contains("!!"));
            }
        }
    }

    /// A data chunk: routed to the tab whose lineage owns the session (§3.6 —
    /// a tab never processes another session's data events).
    fn apply_data(&self, data: super::host::DataRowMsg) {
        let Some(vm) = self.vm_owning(data.session_id) else { return };
        let line = LogLine::Data {
            stamp: data.stamp,
            direction: data.direction,
            bytes: data.bytes,
            hooked: data.hooked,
        };
        self.push_line_to(&vm, line);
    }

    /// An open command failed synchronously (PortBusy `AlreadyOpen`, device
    /// gone, driver error — ticket #35): the tab that asked clears its
    /// open-pending state and carries the error row (per session, never
    /// global). Without a pending entry (a demo open), the active tab carries
    /// the row.
    fn apply_open_failed(&self, port: &str, error: &str) {
        let pending = self
            .pending_opens
            .borrow_mut()
            .remove(port)
            .and_then(|tab_id| {
                self.tabs
                    .borrow()
                    .iter()
                    .find(|vm| vm.borrow().tab_id == tab_id)
                    .cloned()
            });
        let Some(vm) = pending.or_else(|| self.active_vm()) else { return };
        vm.borrow_mut().open_pending = false;
        self.disarm_open_progress();
        let message = t!("open_failed", name = port, error = error).to_string();
        let is_active = self.active_vm().is_some_and(|active| Rc::ptr_eq(&active, &vm));
        self.push_line_to(&vm, LogLine::Note { text: message, error: true });
        if is_active {
            self.open_close.set_is_enabled(!vm.borrow().open_pending);
        }
        self.sync_tab_headers();
    }

    /// Pushes a line into `vm`'s model and — when the tab is the active one
    /// and the line is visible — the live view.
    fn push_line_to(&self, vm: &Rc<RefCell<SessionVm>>, line: LogLine) {
        let visible = vm.borrow_mut().push_line(line.clone());
        let is_active = self
            .active_vm()
            .is_some_and(|active| Rc::ptr_eq(&active, vm));
        if is_active && visible {
            self.append_rendered(&line);
            self.update_shown_status();
        }
    }

    /// The tab owning session `id`, if any.
    fn vm_owning(&self, id: u64) -> Option<Rc<RefCell<SessionVm>>> {
        self.tabs
            .borrow()
            .iter()
            .find(|vm| vm.borrow().owns_session(id))
            .cloned()
    }

    /// A port lifecycle event: binds pending opens (the tab that asked adopts
    /// the new session id), adopts auto-reconnects into the watching tab,
    /// updates the chip + banner, and appends the note row (§5.4).
    fn apply_port_state(
        &self,
        session_id: u64,
        port: String,
        state: PortState,
        detail: Option<String>,
        row: String,
    ) {
        // Process-global hotplug (session_id == GLOBAL_SESSION_ID): refresh the
        // listing (the picker + selectors) and toast (error-matrix row 5).
        if session_id == GLOBAL_SESSION_ID {
            self.refresh_ports();
            match state {
                PortState::Arrived => {
                    if let Some(notifier) = self.notifier.borrow().as_ref() {
                        notifier.inform(&t!("port_hotplug").to_string(), &t!("port_arrived", name = &port));
                    }
                }
                PortState::Removed => {
                    if let Some(notifier) = self.notifier.borrow().as_ref() {
                        notifier.inform(&t!("port_hotplug").to_string(), &t!("port_removed", name = &port));
                    }
                }
                _ => {}
            }
            return;
        }

        // Binding: an Open event resolves the tab that asked (pending opens),
        // adopting the session id into its lineage. A Reopened event adopts
        // into the tab that was watching that port (one scrollback across the
        // reconnect — §6.5's "log continuity").
        if matches!(state, PortState::Open | PortState::Reopened) {
            let pending = self.pending_opens.borrow_mut().remove(&port);
            // (Owner lookup snapshots the table in its own borrow scope — the
            // auto-create below takes a mutable borrow and must not nest.)
            let owner = {
                let tabs = self.tabs.borrow();
                match pending {
                    Some(tab_id) => tabs.iter().find(|vm| vm.borrow().tab_id == tab_id).cloned(),
                    // Nobody asked (an autodemo open): a tab already showing
                    // the port adopts (no duplicate tabs, AC6); else the
                    // auto-create path below makes one (§3.6).
                    None => tabs
                        .iter()
                        .find(|vm| {
                            let vm = vm.borrow();
                            vm.title == port && !vm.owns_session(session_id)
                        })
                        .cloned(),
                }
            };
            let owner = match owner {
                Some(vm) => Some(vm),
                None => {
                    // Auto-create only when no tab shows the port at all.
                    let exists = self.tabs.borrow().iter().any(|vm| vm.borrow().title == port);
                    if exists {
                        None
                    } else {
                        Some(self.add_tab())
                    }
                }
            };
            if let Some(vm) = owner {
                let flush_hook = {
                    let mut vm = vm.borrow_mut();
                    if !vm.owns_session(session_id) {
                        vm.session_ids.push(session_id);
                    }
                    vm.title = port.clone();
                    vm.open_pending = false;
                    // A hook armed before this open flushes to the session now.
                    vm.pending_hook.take()
                };
                if let Some(script) = flush_hook {
                    core_host()
                        .commands
                        .send(Command::SetHook { session_id: Some(session_id), script })
                        .ok();
                }
            }
        } else if let Some(vm) = self.vm_owning(session_id) {
            vm.borrow_mut().open_pending = false;
        }

        // Chip + banner + note row on the owning tab (dead sessions' events
        // still route by lineage; the tab keeps its history).
        let Some(vm) = self
            .vm_owning(session_id)
            .or_else(|| {
                self.tabs
                    .borrow()
                    .iter()
                    .find(|vm| vm.borrow().title == port)
                    .cloned()
            })
        else {
            return;
        };
        vm.borrow_mut().state = state;
        self.disarm_open_progress();
        let error = row.contains("!!");
        self.push_line_to(&vm, LogLine::Note { text: row, error });
        let is_active = self
            .active_vm()
            .is_some_and(|active| Rc::ptr_eq(&active, &vm));
        if is_active {
            self.set_open_state(state);
            self.open_close.set_is_enabled(!vm.borrow().open_pending);
            let _ = detail;
        }
        self.sync_tab_headers();
    }

    // ---- Autodemo persist/settings checks (ticket #25's two-run proofs) ------

    /// The `RCOM_AUTODEMO=persist` proof, run twice: the first run saves a marker
    /// entry through the real save path; the second run finds it in the loaded
    /// profile and says so in the trace.
    fn autodemo_persist_check(this: &Rc<Self>) {
        if AutodemoMode::from_env() != AutodemoMode::Persist {
            return;
        }
        const MARKER: &str = "persist-proof";
        let added = {
            let vm = this.tabs.borrow().first().cloned();
            let Some(vm) = vm else { return };
            let mut vm = vm.borrow_mut();
            if vm.quick.pages[0].entries.iter().any(|entry| entry.label == MARKER) {
                tracing::info!(
                    "AUTODEMO PERSIST OK: quick-send entry '{MARKER}' survived the restart \
                     (profile at {})",
                    QuickSendStore::default_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<no config dir>".into())
                );
                return;
            }
            vm.quick.pages[0].entries.push(QuickSendEntry::text(MARKER, "still here\r\n"));
            vm.quick.save();
            true
        };
        if added {
            tracing::info!(
                "AUTODEMO PERSIST: marker entry '{MARKER}' saved to the TOML profile \
                 (run 1) — run again to see AUTODEMO PERSIST OK"
            );
            this.rebuild_quick_entries();
        }
    }

    /// The `RCOM_SETTINGSDEMO=1` proof (ticket #25), the same two-run pattern
    /// as `=persist`: run 1 flips the theme and the language **through the
    /// settings page's real selector paths** (live variant switch, live string
    /// re-apply) and persists; run 2 finds the flipped values in the loaded
    /// settings — which also means the startup look and locale came from the
    /// file — and says OK.
    fn autodemo_settings_check() {
        if std::env::var("RCOM_SETTINGSDEMO").as_deref() != Ok("1") {
            return;
        }
        use super::settings::{LanguageMode, ThemeMode};
        let loaded = super::settings::Settings::load();
        if loaded.theme == ThemeMode::Light && loaded.language == LanguageMode::Chinese {
            tracing::info!(
                "AUTODEMO SETTINGS OK: theme=light and language=zh survived the restart \
                 (persisted at {}; the startup look and locale came from this file; the \
                 settings tab header resolves as '{}' in that locale) — the two-run \
                 persist proof is complete",
                super::settings::Settings::default_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<no config dir>".into()),
                t!("settings_tab"),
            );
            return;
        }
        // Run 1: the flip goes through the page's real change handlers (the
        // selection-changed events fire: live theme switch, live language
        // re-apply, TOML save).
        tools_settings::autodemo_flip();
        tracing::info!(
            "AUTODEMO SETTINGS: flipped theme→light (live variant switch) and \
             language→zh (live string re-apply) through the settings page and \
             persisted (run 1) — run again to see AUTODEMO SETTINGS OK"
        );
    }
}

/// The listing label of a port: name, the description when the OS provides
/// one, and the mock scenario in the parens (the mock's self-identification —
/// keyed off `PortInfo::kind`, AC9).
fn port_label(info: &PortInfo) -> String {
    let scenario = match info.kind {
        rcom::core::port::PortKind::Mock(scenario) => Some(scenario),
        rcom::core::port::PortKind::Real => None,
    };
    match (&info.description, scenario) {
        (Some(description), Some(scenario)) => {
            format!("{}  ({description}, mock {})", info.name, scenario.id())
        }
        (Some(description), None) => format!("{}  ({description})", info.name),
        (None, Some(scenario)) => format!("{}  (mock {})", info.name, scenario.id()),
        (None, None) => info.name.clone(),
    }
}

/// Re-applies every visible string after a locale switch (the settings page's
/// language selector — ticket #25). Static labels, placeholders, captions and
/// the window title re-render; history log rows keep the language they were
/// rendered in (they are data, not chrome).
/// The active session's visible log lines as space-joined row texts
/// (`RX 70 …` per data line, the note text per note line). The demo paths
/// use this instead of walking the rendered row tree — the model is the
/// source the rows render from.
pub(crate) fn active_session_row_texts() -> Vec<String> {
    let Some(vm) = PUMP.with(|pump| pump.borrow().controller.clone())
        .and_then(|controller| controller.active_vm())
    else {
        return Vec::new();
    };
    let vm = vm.borrow();
    vm.visible_lines()
        .iter()
        .map(|line| match line {
            LogLine::Data { direction, bytes, .. } => {
                let badge = match direction {
                    Direction::Rx => "RX",
                    Direction::Tx => "TX",
                };
                format!("{badge} {}", rcom::core::codec::hex_spaced(bytes))
            }
            LogLine::Note { text, .. } => text.clone(),
        })
        .collect()
}

pub(crate) fn apply_all_strings() {
    PUMP.with(|pump| {
        if let Some(controller) = pump.borrow().controller.clone() {
            controller.apply_ui_strings();
            controller.sync_from_active();
            controller.rebuild_picker();
        }
    });
    tools_net::apply_strings();
    // ISSUE #34 (second splice point, same per-page pattern): the terminal
    // page joins the live language switch.
    super::tools_terminal::apply_strings();
    super::tools_mqtt::apply_strings();
    super::tools_plot::apply_strings();
    super::tools_codec::apply_strings();
    super::tools_monitor::apply_strings();
    tools_settings::apply_strings();
}

/// The payload preview on an entry's send button: escapes line breaks, marks hex
/// payloads, truncates. (Test-only since #35's edit-in-place replaced the static
/// button label; the unit tests pin its escaping rules.)
#[cfg(test)]
fn entry_button_text(entry: &QuickSendEntry) -> String {
    let payload = if entry.hex {
        format!("{} {}", t!("quick_hex_mark"), entry.payload)
    } else {
        entry.payload.replace('\r', "\\r").replace('\n', "\\n")
    };
    let mut chars = payload.chars();
    let mut preview: String = (&mut chars).take(28).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    format!("{}  ▸  {}", entry.label, preview)
}

/// Subscribes the control events of the window (runs on the UI thread; the handlers
/// keep the controller alive for the window's lifetime).
fn wire_events(this: &Rc<MainController>) {
    // Tab strip.
    this.new_tab.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| {
            // The picker flyout opens with the current listing (the auto-open
            // shows it; the rebuild here keeps it fresh).
            this.rebuild_picker();
        }
    });
    this.close_tab.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.close_tab_clicked()
    });

    // Log toolbar.
    this.follow_toggle.is_checked_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            this.set_follow(this.follow_toggle.is_checked() == Some(true));
        }
    });
    this.view_mode_selector.selection_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            this.set_view_mode(ViewMode::from_index(this.view_mode_selector.selected_index()));
        }
    });
    this.filter_box.text_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            let filter = this.filter_box.text().unwrap_or_default();
            this.set_filter(filter);
        }
    });
    this.timestamps_toggle.is_checked_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            this.set_timestamps(this.timestamps_toggle.is_checked() == Some(true));
        }
    });
    this.hide_sent.is_checked_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            this.set_hide_sent(this.hide_sent.is_checked() == Some(true));
        }
    });
    this.clear_log.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.clear_log()
    });

    // Follow model: scrolling up auto-disengages (no pause-on-hover, §3.3).
    this.log_scroller.scroll_changed({
        let this = Rc::clone(this);
        move |_, args| {
            if args.offset_delta().y < -0.5 {
                let Some(vm) = this.active_vm() else { return };
                if vm.borrow().follow && !this.syncing.get() {
                    vm.borrow_mut().follow = false;
                    this.follow_toggle.set_is_checked(Some(false));
                    this.sync_follow_status_word();
                }
            }
        }
    });

    // Session toolbar.
    this.open_close.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.toggle_open_close()
    });
    this.refresh_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.refresh_ports()
    });
    this.encoding_selector.selection_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            let index = this.encoding_selector.selected_index();
            if let Some(encoding) = TextEncoding::all().get(index.max(0) as usize).copied() {
                this.set_encoding(encoding);
            }
        }
    });

    // Mock drawer.
    this.mock_drawer_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.toggle_mock_drawer()
    });
    this.mock_pause.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| {
            let paused = this.active_vm().map(|vm| vm.borrow().mock_paused).unwrap_or(false);
            this.set_mock_paused(!paused);
        }
    });
    this.mock_step.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.mock_action(MockAction::Step)
    });
    this.mock_speed.selection_changed({
        let this = Rc::clone(this);
        move |_, _| {
            if this.syncing.get() {
                return;
            }
            let index = this.mock_speed.selected_index();
            if index >= 0 {
                this.set_mock_speed_index(index as usize);
            }
        }
    });
    this.mock_inject.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.mock_inject()
    });
    this.mock_inject_box.key_down({
        let this = Rc::clone(this);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                this.mock_inject();
            }
        }
    });
    this.mock_trigger.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.mock_trigger_for_session()
    });
    this.mock_restart.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.mock_action(MockAction::Restart)
    });

    // Send bar.
    this.send_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.send_box_text()
    });
    this.send_box.key_down({
        let this = Rc::clone(this);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                this.send_box_text();
            }
        }
    });

    // Quick-send pager + editor.
    this.quick_pager.selected_index_changed({
        let this = Rc::clone(this);
        move |_, args| {
            if this.syncing.get() {
                return;
            }
            this.show_quick_page(args.new_index().max(0) as usize)
        }
    });
    this.quick_add.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.add_quick_entry()
    });
    this.quick_save.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.save_quick_entry()
    });
    this.quick_delete.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.delete_quick_entry()
    });

    // The script page (ticket #19).
    this.script_selector.selection_changed({
        let this = Rc::clone(this);
        move |_, _| this.load_selected_script()
    });
    this.script_new.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.new_script()
    });
    this.script_save.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.save_script()
    });
    this.script_run.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.run_script()
    });
    this.script_stop.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.stop_script()
    });
    this.tx_hook_enable.is_checked_changed({
        let this = Rc::clone(this);
        move |_, _| this.apply_tx_hook()
    });
    // Enter in the hook box re-applies the hook while enabled.
    this.tx_hook_box.key_down({
        let this = Rc::clone(this);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                if this.tx_hook_enable.is_checked() == Some(true) {
                    this.apply_tx_hook();
                }
            }
        }
    });
}

impl MainController {
    /// Open/Close button: opens the selected port into the active tab, or
    /// closes the tab's session.
    fn toggle_open_close(&self) {
        let Some(vm) = self.active_vm() else { return };
        if vm.borrow().is_active_state() {
            if let Some(session_id) = vm.borrow().current_session() {
                core_host()
                    .commands
                    .send(Command::CloseSession { session_id })
                    .ok();
            }
            return;
        }
        let index = self.port_selector.selected_index();
        let port = self
            .ports
            .borrow()
            .get(index.max(0) as usize)
            .cloned();
        let Some(port) = port else {
            self.append_note(&t!("select_port_first").to_string(), false);
            return;
        };
        self.open_in_active_tab(&port);
    }
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle the pump uses to reach the controller, and the timer
    /// that keeps the pump running (dropping a timer stops it).
    static PUMP: RefCell<Pump> = RefCell::new(Pump { controller: None, _timer: None });
}

struct Pump {
    controller: Option<Rc<MainController>>,
    _timer: Option<Rc<DispatcherTimer>>,
}

/// Drains the core driver's messages on the UI thread: a dispatcher timer polls the
/// std channel — the sanctioned bridge, since FerroUI object handles are not `Send`
/// (verified against the pin: `DispatcherTimer::with_callback`).
fn start_pump(controller: &Rc<MainController>) {
    // The window-notification manager mounts against the window's top-level
    // (the theme styles it; §4.1 rows 4–6 only).
    let top_level: Ref<ferroui_controls::TopLevel> = controller.window.clone().upcast();
    *controller.notifier.borrow_mut() = Some(Rc::new(Notifier::mounted(&top_level)));

    PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        pump.controller = Some(Rc::clone(controller));
        pump._timer = Some(DispatcherTimer::with_callback(
            PUMP_INTERVAL,
            DispatcherPriority::BACKGROUND,
            |_| pump_tick(),
        ));
    });
}

fn pump_tick() {
    let started = std::time::Instant::now();
    let messages: Vec<UiMsg> = core_host().ui_rx.lock().expect("ui pump mutex").try_iter().collect();
    if let Some(controller) = PUMP.with(|pump| pump.borrow().controller.clone()) {
        for message in messages {
            controller.apply(message);
        }
    }
    // ISSUE #24 (one insertion point): the plot page's re-render nudge —
    // invalidates the plot view only when the plot store changed (≤ 30 fps).
    super::tools_plot::pump_tick();
    // The live-rate budget (issue #24): the tick runs every 30 ms; a plot at
    // full wire rate must never push it over. Breaches trace so the PLOTDEMO
    // smoke proves the pump stayed under budget by their absence.
    let elapsed = started.elapsed();
    if elapsed > std::time::Duration::from_millis(20) {
        tracing::warn!(target: "rcom::pump", "pump tick {:.1} ms — over the 20 ms budget",
            elapsed.as_secs_f64() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcom::core::codec::ParseHexError;

    /// A bad hex payload is reported, not sent.
    #[test]
    fn hex_parse_error_is_displayable() {
        let error = parse_hex("zz").unwrap_err();
        assert_eq!(error, ParseHexError('z'));
    }

    /// Entry button labels escape line breaks, mark hex payloads and truncate.
    #[test]
    fn entry_button_text_previews_payloads() {
        assert_eq!(
            entry_button_text(&QuickSendEntry::text("ping", "ping\r\n")),
            "ping  ▸  ping\\r\\n"
        );
        assert_eq!(
            entry_button_text(&QuickSendEntry::hex("reset", "A0 01 FF")),
            "reset  ▸  [hex] A0 01 FF"
        );
        let long = "x".repeat(40);
        let text = entry_button_text(&QuickSendEntry::text("long", long));
        assert!(text.chars().count() < 40, "{text}");
        assert!(text.ends_with('…'), "{text}");
    }

    /// The port label: the description rides when present; mock ports name
    /// their scenario (the picker's self-identification).
    #[test]
    fn port_labels_name_the_scenario() {
        let real = PortInfo {
            name: "COM3".into(),
            vid: Some(0x2e8a),
            pid: Some(0x000a),
            description: Some("Pico".into()),
            kind: rcom::core::port::PortKind::Real,
        };
        assert_eq!(port_label(&real), "COM3  (Pico)");

        let mock = PortInfo {
            name: "MOCK0".into(),
            vid: None,
            pid: None,
            description: Some("rcom mock echo device (echo)".into()),
            kind: rcom::core::port::PortKind::Mock(MockScenario::Echo),
        };
        assert_eq!(
            port_label(&mock),
            "MOCK0  (rcom mock echo device (echo), mock echo)"
        );

        let bare_mock = PortInfo {
            name: "MOCK2".into(),
            vid: None,
            pid: None,
            description: None,
            kind: rcom::core::port::PortKind::Mock(MockScenario::Framed),
        };
        assert_eq!(port_label(&bare_mock), "MOCK2  (mock framed)");
    }

    /// Chip colours: green pair for open/reopened, amber for watching, grey
    /// otherwise (§3.2's chip contract).
    #[test]
    fn chip_colors_match_the_contract() {
        let (bg, fg) = chip_colors(PortState::Open);
        assert_eq!(fg, logview::rx_color());
        assert_ne!(bg, logview::rx_color(), "the wash is translucent");
        let (_, fg) = chip_colors(PortState::WatchDesired);
        assert_eq!(fg, logview::note_color());
        let (_, fg) = chip_colors(PortState::Closed);
        assert_ne!(fg, logview::note_color());
        let (bg, fg) = chip_colors(PortState::Reopened);
        assert_eq!(fg, logview::rx_color());
        let _ = (bg, fg);
    }
}
