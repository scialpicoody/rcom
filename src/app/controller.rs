//! The controller of `MainWindow`: looks the named controls up in the window's name
//! scope, wires their events to core commands, and pumps the core's messages into
//! the log view. Lives on the UI thread only (`Rc` state; every field is accessed
//! from handlers and the dispatcher-timer pump, which run there).
//!
//! Ticket #17 additions: the encoding selector (display + send), the log's row
//! model that retains raw bytes per row so an encoding switch re-renders history
//! (docs/ui-drafts/05-contract.md §1.3), and the quick-send pages panel. Visible
//! strings come from the i18n dictionaries (`locales/`, via `t!`).
//! Ticket #16 additions woven in: the hide-sent filter (a view-only projection of
//! the same row model) and the Open/Close button that stays Close while the
//! reconnect machine watches.
//! Ticket #19 additions: the bottom tabbed pane's Script page — the monospaced
//! editor (Run/Stop drive the standalone script VM through the core driver), the
//! per-name script store, the bundled samples, and the TX pre-send hook
//! affordance (a one-line conversion script + enable toggle).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::layout::Orientation;
use ferroui_base::media::FontFamily;
use ferroui_base::threading::{DispatcherPriority, DispatcherTimer};
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::primitives::HeaderedContentControl;
use ferroui_controls::{
    Button, CheckBox, ComboBox, Control, ListBox, PipsPager, StackPanel, TabItem, TextBox,
    TextBlock,
};

use super::host::{core_host, AutodemoMode, Command, UiMsg};
use super::quicksend::{QuickSendEntry, QuickSendStore, PAGE_COUNT};
use super::scriptstore;
use rcom::core::bus::{Direction, PortState};
use rcom::core::codec::{hex_spaced, parse_hex, TextEncoding};
use rcom::core::port::PortInfo;

use rust_i18n::t;

/// The baud rates the baud selector offers; 115200 is preselected (or the
/// persisted launch default — ticket #25).
pub(crate) const BAUD_RATES: [u32; 7] = [9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800];

/// How many rows the log view keeps before it drops the oldest: the virtualized
/// list keeps memory sane, llcom-style. Applies to the row model too (the model
/// holds the raw bytes the rows re-render from).
const ROW_CAP: usize = 5000;

/// How often the UI pumps messages from the core driver.
const PUMP_INTERVAL: Duration = Duration::from_millis(30);

/// The script editor's monospaced families, first available wins (the pin's
/// TextBox has no XAML-settable font family — set in code, per the #19 brief).
const SCRIPT_EDITOR_FONT: &str = "Menlo, Consolas, DejaVu Sans Mono, Courier New";

/// One line of the log view: pre-formatted text (state rows, diagnostics; the
/// `direction` stays `None` there) or a data chunk with its raw bytes — the
/// authoritative form an encoding switch re-renders from. The log *view* is a
/// filtered render of these lines (the hide-sent toggle); the core's log store and
/// the auto-save file stay complete — the UI↔Core contract's "presentation options
/// are viewmodel state, never core state".
#[derive(Debug, Clone)]
enum LogLine {
    Text { text: String, direction: Option<Direction> },
    Data { stamp: String, direction: Direction, bytes: Vec<u8> },
}

pub struct MainController {
    /// The window itself: the title re-applies with the language (ticket #25's
    /// live switch) and the settings page holds its own handle for the theme.
    window: Ref<crate::MainWindow>,
    port_selector: Ref<ComboBox>,
    baud_selector: Ref<ComboBox>,
    open_close: Ref<Button>,
    refresh_button: Ref<Button>,
    /// The hide-sent filter: view-only (contract §2.1 — presentation options are
    /// viewmodel state); the log store and auto-save file stay complete.
    hide_sent: Ref<CheckBox>,
    encoding_selector: Ref<ComboBox>,
    send_box: Ref<TextBox>,
    send_button: Ref<Button>,
    log: Ref<ListBox>,
    port_caption: Ref<TextBlock>,
    baud_caption: Ref<TextBlock>,
    encoding_caption: Ref<TextBlock>,
    // The quick-send panel.
    quick_send_title: Ref<TextBlock>,
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
    // State.
    ports: RefCell<Vec<PortInfo>>,
    is_open: Cell<bool>,
    /// Whether TX rows are filtered from the view (the store/file stay complete).
    hide_sent_on: Cell<bool>,
    encoding: Cell<TextEncoding>,
    lines: RefCell<Vec<LogLine>>,
    quick_store: RefCell<QuickSendStore>,
    quick_page: Cell<usize>,
    quick_editing: Cell<Option<usize>>,
    /// The saved script names behind the picker (samples come first, from
    /// [`scriptstore::SAMPLES`]).
    scripts: RefCell<Vec<String>>,
    /// Whether the persisted launch-default port already preselected (the
    /// first listing after launch only — afterwards the user's choice wins).
    port_preset_done: Cell<bool>,
}

impl MainController {
    /// Builds the controller for `window`: resolves the named controls of
    /// `MainWindow.xaml`, subscribes their events and starts the pump. Call on the
    /// UI thread, after the window's markup is populated.
    pub fn attach(window: &Ref<crate::MainWindow>) {
        let element: &Ref<StyledElement> = window.upcast_ref();
        // (The title applies with every string re-application; see
        // `apply_ui_strings` — the language switch retitles the window.)

        let this = Rc::new(Self {
            window: window.clone(),
            port_selector: NameScopeExtensions::get(element, "PortSelector"),
            baud_selector: NameScopeExtensions::get(element, "BaudSelector"),
            open_close: NameScopeExtensions::get(element, "OpenCloseButton"),
            refresh_button: NameScopeExtensions::get(element, "RefreshButton"),
            hide_sent: NameScopeExtensions::get(element, "HideSentToggle"),
            encoding_selector: NameScopeExtensions::get(element, "EncodingSelector"),
            send_box: NameScopeExtensions::get(element, "SendBox"),
            send_button: NameScopeExtensions::get(element, "SendButton"),
            log: NameScopeExtensions::get(element, "LogList"),
            port_caption: NameScopeExtensions::get(element, "PortCaption"),
            baud_caption: NameScopeExtensions::get(element, "BaudCaption"),
            encoding_caption: NameScopeExtensions::get(element, "EncodingCaption"),
            quick_send_title: NameScopeExtensions::get(element, "QuickSendTitle"),
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
            ports: RefCell::new(Vec::new()),
            is_open: Cell::new(false),
            hide_sent_on: Cell::new(false),
            encoding: Cell::new(TextEncoding::Utf8),
            lines: RefCell::new(Vec::new()),
            quick_store: RefCell::new(QuickSendStore::default()),
            quick_page: Cell::new(0),
            quick_editing: Cell::new(None),
            scripts: RefCell::new(Vec::new()),
            port_preset_done: Cell::new(false),
        });

        this.apply_ui_strings();
        this.fill_baud_selector();
        this.fill_encoding_selector();
        MainController::setup_quick_send(&this);
        MainController::setup_script_page(&this);
        // ISSUE #34 (one insertion point): the terminal page of the bottom
        // pane — keyboard direct-send through the existing SendBytes path.
        super::tools_terminal::attach(element);
        // ISSUE #20 (one insertion point): the net tools page of the bottom pane.
        super::tools_net::attach(element);
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
        super::tools_settings::attach(element, window);
        wire_events(&this);
        start_pump(&this);
        MainController::autodemo_persist_check(&this);
        MainController::autodemo_settings_check();

        // Kick off the first port listing.
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    /// Every visible string of the window, from the dictionaries (the XAML carries
    /// none, so both locales ship from the same document). Re-runs on a live
    /// language switch (ticket #25).
    fn apply_ui_strings(&self) {
        self.window.set_title(Some(t!("title").to_string()));
        self.refresh_button.set_content(Some(Rc::new(t!("refresh").to_string())));
        self.send_button.set_content(Some(Rc::new(t!("send").to_string())));
        self.send_box.set_placeholder_text(Some(&t!("send_hint")));
        self.port_caption.set_text(Some(&t!("port")));
        self.baud_caption.set_text(Some(&t!("baud")));
        self.encoding_caption.set_text(Some(&t!("encoding")));
        self.hide_sent.set_content(Some(Rc::new(t!("hide_sent").to_string())));

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
        self.script_tab.set_value(
            HeaderedContentControl::header_property(),
            Some(Rc::new(t!("script_tab").to_string())),
        );
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

        // The Open/Close label tracks the port state.
        self.set_open_state(if self.is_open.get() { PortState::Open } else { PortState::Closed });
    }

    fn fill_baud_selector(&self) {
        let items = self.baud_selector.items();
        for rate in BAUD_RATES {
            let value: BoxedValue = Rc::new(rate.to_string());
            items.add(Some(value));
        }
        // The persisted launch default preselects (ticket #25); 115200 stays
        // the fallback when nothing is persisted.
        let wanted = super::settings::global()
            .map(|settings| settings.resolved_baud())
            .unwrap_or(115_200);
        let default = BAUD_RATES
            .iter()
            .position(|rate| *rate == wanted)
            .unwrap_or_else(|| BAUD_RATES.iter().position(|rate| *rate == 115_200).unwrap_or(0));
        self.baud_selector.set_selected_index(default as i32);
    }

    /// The encoding selector: one entry per [`TextEncoding`], the persisted
    /// launch default preselected (UTF-8 fallback). The names are proper nouns
    /// (see [`TextEncoding::display_name`]).
    fn fill_encoding_selector(&self) {
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

    /// Open/Close button: opens the selected port at the selected baud, or closes
    /// the current session.
    fn toggle_open_close(&self) {
        if self.is_open.get() {
            core_host().commands.send(Command::Close).ok();
            return;
        }
        let index = self.port_selector.selected_index();
        let port = self
            .ports
            .borrow()
            .get(index.max(0) as usize)
            .map(|info| info.name.clone());
        let Some(port) = port else {
            self.append_line(LogLine::Text {
                text: format!("## {}", t!("select_port_first")),
                direction: None,
            });
            return;
        };
        core_host().commands
            .send(Command::Open { name: port, baud: self.selected_baud() })
            .ok();
    }

    fn refresh_ports(&self) {
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    /// Send button / Enter in the send box: the box's text encoded with the
    /// selector's encoding (the "sending honours the encoding" path).
    fn send_box_text(&self) {
        let Some(text) = self.send_box.text() else { return };
        if text.is_empty() {
            return;
        }
        let bytes = self.encoding.get().encode(&text);
        if core_host().commands.send(Command::SendBytes(bytes)).is_ok() {
            self.send_box.set_text(None);
        }
    }

    /// One quick-send entry, clicked: its payload to the same TX path as the send
    /// box — text encodes with the current encoding, hex parses to raw bytes.
    fn send_quick_entry(&self, entry: QuickSendEntry) {
        let bytes = if entry.hex {
            parse_hex(&entry.payload)
        } else {
            Ok(self.encoding.get().encode(&entry.payload))
        };
        match bytes {
            Ok(bytes) => {
                core_host().commands.send(Command::SendBytes(bytes)).ok();
            }
            Err(error) => {
                let message = t!("invalid_hex_payload", error = &error).to_string();
                self.append_line(LogLine::Text {
                    text: format!("!! {message}"),
                    direction: None,
                });
            }
        }
    }

    fn set_open_state(&self, state: PortState) {
        // A session being watched is still alive from the UI's point of view:
        // `close()` must stay available to cancel the auto-reconnect (contract §3:
        // `WatchDesired → Closed | user close()`). The hotplug poller's global
        // Arrived/Removed events never touch the button.
        let active = matches!(
            state,
            PortState::Open | PortState::Reopened | PortState::WatchDesired
        );
        self.is_open.set(active);
        let label = if active { t!("close") } else { t!("open") };
        self.open_close.set_content(Some(Rc::new(label.to_string())));
    }

    // ---- The log view: model + rendering ------------------------------------

    /// Renders one model line with the current encoding.
    fn render_line(&self, line: &LogLine) -> String {
        match line {
            LogLine::Text { text, .. } => text.clone(),
            LogLine::Data { stamp, direction, bytes } => {
                render_data_line(stamp, *direction, bytes, self.encoding.get())
            }
        }
    }

    /// The direction a line shows, if any — the hide-sent filter's input. Status
    /// lines carry none; data lines their RX/TX direction.
    fn line_direction(line: &LogLine) -> Option<Direction> {
        match line {
            LogLine::Text { direction, .. } => *direction,
            LogLine::Data { direction, .. } => Some(*direction),
        }
    }

    /// Whether a line is visible under the current filter: hiding sent rows
    /// removes the TX lines (the sent data) from the *view* only.
    fn visible(line: &LogLine, hide_sent: bool) -> bool {
        !(hide_sent && Self::line_direction(line) == Some(Direction::Tx))
    }

    /// Appends a line to the model and — when visible under the current filter —
    /// the view (dropping the oldest past the cap; a rotated-out line leaves the
    /// list with it, but only when it was visible: hidden lines never entered it).
    fn append_line(&self, line: LogLine) {
        let hide_sent = self.hide_sent_on.get();
        let show = Self::visible(&line, hide_sent);
        let rendered = self.render_line(&line);
        let mut lines = self.lines.borrow_mut();
        if lines.len() >= ROW_CAP {
            let rotated = lines.remove(0);
            if Self::visible(&rotated, hide_sent) {
                self.log.items().remove_at(0);
            }
        }
        lines.push(line);
        drop(lines);
        if show {
            let value: BoxedValue = Rc::new(rendered);
            self.log.items().add(Some(value));
            self.scroll_to_newest();
        }
    }

    fn scroll_to_newest(&self) {
        let count = self.log.item_count();
        if count > 0 {
            self.log.scroll_into_view(count - 1);
        }
    }

    /// Re-renders every retained line under the current filter — the one full
    /// re-render both presentation options share: the encoding switch (the raw
    /// bytes stayed in the model, so history decodes anew instead of staying
    /// frozen in the encoding of the moment, contract §1.3) and the hide-sent
    /// toggle (TX lines vanish from the view).
    fn render_view(&self) {
        let hide_sent = self.hide_sent_on.get();
        let lines = self.lines.borrow();
        let items = self.log.items();
        items.clear();
        for line in lines.iter().filter(|line| Self::visible(line, hide_sent)) {
            let value: BoxedValue = Rc::new(self.render_line(line));
            items.add(Some(value));
        }
        drop(lines);
        self.scroll_to_newest();
    }

    /// The hide-sent checkbox flipped: remember the filter and re-render the view
    /// from the line model. TX rows vanish from the *view* only.
    fn set_hide_sent(&self) {
        self.hide_sent_on.set(self.hide_sent.is_checked() == Some(true));
        self.render_view();
    }

    /// Applies an encoding chosen in the selector.
    fn set_encoding(&self, encoding: TextEncoding) {
        self.encoding.set(encoding);
        self.render_view();
        self.autodemo_encoding_check();
    }

    /// The selector's selection changed: look the encoding up by index.
    fn on_encoding_selected_index(&self) {
        let index = self.encoding_selector.selected_index();
        if let Some(encoding) = TextEncoding::all().get(index.max(0) as usize).copied() {
            self.set_encoding(encoding);
        }
    }

    /// `UiMsg::SetEncoding` (the encoding autodemo): same path as the selector.
    fn apply_set_encoding(&self, encoding: TextEncoding) {
        if let Some(index) = TextEncoding::all().iter().position(|candidate| *candidate == encoding) {
            let index = index as i32;
            if self.encoding_selector.selected_index() == index {
                self.set_encoding(encoding);
            } else {
                // Fires selection_changed → `set_encoding` + the check.
                self.encoding_selector.set_selected_index(index);
            }
        }
    }

    /// Headless proof for `RCOM_AUTODEMO=encoding`: after the switch, the retained
    /// GBK bytes must decode to the sent text (the mock's echo shares them).
    fn autodemo_encoding_check(&self) {
        if AutodemoMode::from_env() != AutodemoMode::Encoding {
            return;
        }
        if self.encoding.get() != TextEncoding::Gbk {
            return;
        }
        let expected = TextEncoding::Gbk.encode("你好，rcom\r\n");
        let lines = self.lines.borrow();
        let rendered = lines.iter().any(|line| match line {
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

    // ---- The quick-send panel -----------------------------------------------

    /// Loads the persisted profile and primes the pager + page 1.
    fn setup_quick_send(this: &Rc<Self>) {
        *this.quick_store.borrow_mut() = QuickSendStore::load();
        this.quick_pager.set_number_of_pages(PAGE_COUNT as i32);
        this.quick_pager.set_selected_page_index(0);
        this.update_quick_page_label();
        Self::rebuild_quick_entries(this);
    }

    /// The `RCOM_AUTODEMO=persist` proof, run twice: the first run saves a marker
    /// entry through the real save path; the second run finds it in the loaded
    /// profile and says so in the trace.
    fn autodemo_persist_check(this: &Rc<Self>) {
        if AutodemoMode::from_env() != AutodemoMode::Persist {
            return;
        }
        const MARKER: &str = "persist-proof";
        let added = {
            let mut store = this.quick_store.borrow_mut();
            if store.pages[0].entries.iter().any(|entry| entry.label == MARKER) {
                tracing::info!(
                    "AUTODEMO PERSIST OK: quick-send entry '{MARKER}' survived the restart \
                     (profile at {})",
                    QuickSendStore::default_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<no config dir>".into())
                );
                return;
            }
            store.pages[0].entries.push(QuickSendEntry::text(MARKER, "still here\r\n"));
            store.save();
            true
        };
        if added {
            tracing::info!(
                "AUTODEMO PERSIST: marker entry '{MARKER}' saved to the TOML profile \
                 (run 1) — run again to see AUTODEMO PERSIST OK"
            );
            Self::rebuild_quick_entries(this);
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
        super::tools_settings::autodemo_flip();
        tracing::info!(
            "AUTODEMO SETTINGS: flipped theme→light (live variant switch) and \
             language→zh (live string re-apply) through the settings page and \
             persisted (run 1) — run again to see AUTODEMO SETTINGS OK"
        );
    }

    fn update_quick_page_label(&self) {
        let label = t!("page_indicator", page = self.quick_page.get() + 1, total = PAGE_COUNT);
        self.quick_page_label.set_text(Some(&label));
    }

    /// Rebuilds the entry rows of the current page (add/rename/delete and page
    /// switches all land here — functional, not fancy; the interaction design
    /// lands with ticket #30).
    fn rebuild_quick_entries(this: &Rc<Self>) {
        let entries = this.current_quick_entries();
        let children = this.quick_entries.children();
        children.clear();
        if entries.is_empty() {
            let hint = TextBlock::new();
            hint.set_text(Some(&t!("quick_empty")));
            children.add(hint.upcast::<Control>());
            return;
        }
        for (index, entry) in entries.iter().enumerate() {
            children.add(Self::build_quick_entry_row(this, index, entry).upcast::<Control>());
        }
    }

    fn current_quick_entries(&self) -> Vec<QuickSendEntry> {
        self.quick_store
            .borrow()
            .pages
            .get(self.quick_page.get())
            .map(|page| page.entries.clone())
            .unwrap_or_default()
    }

    /// One entry row: the send button (label ▸ payload) plus the edit button that
    /// loads the entry into the editor below the list.
    fn build_quick_entry_row(this: &Rc<Self>, index: usize, entry: &QuickSendEntry) -> Ref<StackPanel> {
        let row = StackPanel::new();
        row.set_orientation(Orientation::Horizontal);
        row.set_spacing(4.0);

        let send = Button::new();
        send.set_content(Some(Rc::new(entry_button_text(entry))));
        send.set_min_width(240.0);
        let payload = entry.clone();
        send.add_handler(Button::click_event(), {
            let this = Rc::clone(this);
            move |_, _| this.send_quick_entry(payload.clone())
        });

        let edit = Button::new();
        edit.set_content(Some(Rc::new(t!("quick_edit").to_string())));
        edit.set_min_width(32.0);
        edit.add_handler(Button::click_event(), {
            let this = Rc::clone(this);
            move |_, _| this.load_quick_entry(index)
        });

        row.children().add(send.upcast::<Control>());
        row.children().add(edit.upcast::<Control>());
        row
    }

    /// Loads an entry into the editor boxes for rename/delete.
    fn load_quick_entry(&self, index: usize) {
        let entry = self.current_quick_entries().into_iter().nth(index);
        let Some(entry) = entry else { return };
        self.quick_editing.set(Some(index));
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

    fn add_quick_entry(this: &Rc<Self>) {
        let Some(entry) = this.entry_from_editor() else { return };
        if let Some(page) = this.quick_store.borrow_mut().pages.get_mut(this.quick_page.get()) {
            page.entries.push(entry);
        }
        Self::persist_quick_entries(this);
    }

    fn save_quick_entry(this: &Rc<Self>) {
        let Some(index) = this.quick_editing.get() else { return };
        let Some(entry) = this.entry_from_editor() else { return };
        if let Some(page) = this.quick_store.borrow_mut().pages.get_mut(this.quick_page.get()) {
            if let Some(slot) = page.entries.get_mut(index) {
                *slot = entry;
            }
        }
        Self::persist_quick_entries(this);
    }

    fn delete_quick_entry(this: &Rc<Self>) {
        let Some(index) = this.quick_editing.get() else { return };
        if let Some(page) = this.quick_store.borrow_mut().pages.get_mut(this.quick_page.get()) {
            if index < page.entries.len() {
                page.entries.remove(index);
            }
        }
        this.quick_editing.set(None);
        this.quick_label_box.set_text(None);
        this.quick_payload_box.set_text(None);
        this.quick_hex_check.set_is_checked(Some(false));
        Self::persist_quick_entries(this);
    }

    /// Persists and refreshes the panel after any mutation.
    fn persist_quick_entries(this: &Rc<Self>) {
        this.quick_store.borrow().save();
        this.quick_editing.set(None);
        Self::rebuild_quick_entries(this);
    }

    fn show_quick_page(this: &Rc<Self>, page: usize) {
        let page = page.min(PAGE_COUNT - 1);
        this.quick_page.set(page);
        this.quick_editing.set(None);
        this.update_quick_page_label();
        Self::rebuild_quick_entries(this);
    }

    // ---- The script page (bottom pane) ---------------------------------------

    /// Editor shape + the picker's initial content. The multiline flag and the
    /// monospaced font are set here, not in the XAML (not XAML-reliable at this
    /// FerroUI pin).
    fn setup_script_page(this: &Rc<Self>) {
        this.script_editor.set_accepts_return(true);
        this.script_editor.set_font_family(FontFamily::new(SCRIPT_EDITOR_FONT));
        this.script_editor.set_placeholder_text(Some(&t!("script_editor_hint")));
        this.refresh_script_picker();
    }

    /// Rebuilds the picker: the built-in samples first (translated display
    /// names — the sweep keeps even these out of the raw string set), then the
    /// saved scripts (`<config>/rcom/scripts/*.lua`), nothing selected.
    fn refresh_script_picker(&self) {
        // Deselect before the rebuild (see `show_ports` — a selection cannot
        // survive an items clear at this pin).
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
            self.append_line(LogLine::Text {
                text: format!("!! {}", t!("script_load_failed", name = "")),
                direction: None,
            });
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
                self.append_line(LogLine::Text {
                    text: format!("## {}", t!("script_saved", name = &saved)),
                    direction: None,
                });
                self.refresh_script_picker();
            }
            Err(error) => {
                self.append_line(LogLine::Text {
                    text: format!("!! {}", t!("script_save_failed", error = &error)),
                    direction: None,
                });
            }
        }
    }

    /// Delete: the script the editor shows (samples are read-only).
    fn delete_script(&self) {
        let Some(name) = self.script_name_box.text() else { return };
        if scriptstore::sample_by_name(&name).is_some() {
            self.append_line(LogLine::Text {
                text: format!("## {}", t!("script_sample_readonly")),
                direction: None,
            });
            return;
        }
        match scriptstore::delete(scriptstore::scripts_dir().as_deref(), &name) {
            Ok(()) => {
                self.append_line(LogLine::Text {
                    text: format!("## {}", t!("script_deleted", name = &name)),
                    direction: None,
                });
                self.new_script();
                self.refresh_script_picker();
            }
            Err(error) => {
                self.append_line(LogLine::Text {
                    text: format!("!! {}", t!("script_delete_failed", error = &error)),
                    direction: None,
                });
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
        self.append_line(LogLine::Text {
            text: format!("~~ {}", t!("script_started", name = &name)),
            direction: None,
        });
        core_host().commands.send(Command::RunScript { code }).ok();
    }

    /// Stop: the running panel VM unwinds (instruction-count hook), timers abort.
    /// The driver confirms with a `~~` row.
    fn stop_script(&self) {
        core_host().commands.send(Command::StopScript).ok();
    }

    /// The TX hook affordance: enable sends the box's conversion script to the
    /// session (every UI send funnels through it); disable clears it. An empty
    /// script stays disabled — the AC's "default empty = disabled".
    fn apply_tx_hook(&self) {
        let enabled = self.tx_hook_enable.is_checked() == Some(true);
        let script = self.tx_hook_box.text().unwrap_or_default();
        if enabled && script.trim().is_empty() {
            self.tx_hook_enable.set_is_checked(Some(false));
            self.append_line(LogLine::Text {
                text: format!("## {}", t!("tx_hook_empty")),
                direction: None,
            });
            core_host().commands.send(Command::SetHook(None)).ok();
            return;
        }
        core_host()
            .commands
            .send(Command::SetHook(enabled.then_some(script)))
            .ok();
        // The confirmation rows (`tx_hook_on` / `tx_hook_off`) come back from the
        // driver as UiMsg rows.
    }

    // ---- The pump -----------------------------------------------------------

    fn apply(&self, message: UiMsg) {
        match message {
            UiMsg::Row { text, direction } => self.append_line(LogLine::Text { text, direction }),
            UiMsg::Data(data) => self.append_line(LogLine::Data {
                stamp: data.stamp,
                direction: data.direction,
                bytes: data.bytes,
            }),
            UiMsg::Ports(ports) => self.show_ports(ports),
            UiMsg::PortState(state) => self.set_open_state(state),
            UiMsg::SetEncoding(encoding) => self.apply_set_encoding(encoding),
            // ISSUE #20 (one insertion point): the net tools page consumes its events.
            UiMsg::Net(net) => super::tools_net::apply_net(net),
            // ISSUE #21 (one insertion point): the MQTT tools page consumes its events.
            UiMsg::Mqtt(mqtt) => super::tools_mqtt::apply_mqtt(mqtt),
            // ISSUE #24 (one insertion point): the plot page consumes its events.
            UiMsg::Plot(plot) => super::tools_plot::apply_plot(plot),
            // ISSUE #23 (one insertion point): the monitor page consumes its events.
            UiMsg::Monitor(monitor) => super::tools_monitor::apply_monitor(monitor),
        }
    }

    fn show_ports(&self, ports: Vec<PortInfo>) {
        let remember = self.port_selector.selected_index();
        let count = ports.len() as i32;
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (same guard as the settings page).
        self.port_selector.set_selected_index(-1);
        let items = self.port_selector.items();
        items.clear();
        for info in &ports {
            let name = match (&info.description, info.is_mock) {
                (Some(description), false) => format!("{}  ({description})", info.name),
                _ => info.name.clone(),
            };
            let value: BoxedValue = Rc::new(name);
            items.add(Some(value));
        }
        // ISSUE #25: the settings page's default-port picker shows the same
        // listing.
        super::tools_settings::apply_ports(&ports);
        *self.ports.borrow_mut() = ports;
        // Ticket #25: the persisted launch-default port preselects once (the
        // first listing after launch); afterwards the user's choice wins.
        let preset = if self.port_preset_done.get() {
            None
        } else {
            self.port_preset_done.set(true);
            super::settings::global().and_then(|settings| settings.default_port.clone())
        };
        // Keep the previous selection when it still exists, else the first port
        // — unless the launch default names one of the listed ports.
        let kept = preset
            .and_then(|name| {
                self.ports
                    .borrow()
                    .iter()
                    .position(|info| info.name == name)
                    .map(|index| index as i32)
            })
            .unwrap_or(if remember >= 0 && remember < count {
                remember
            } else if count > 0 {
                0
            } else {
                -1
            });
        self.port_selector.set_selected_index(kept);
    }
}

/// Re-applies every visible string after a locale switch (the settings page's
/// language selector — ticket #25). Static labels, placeholders, captions and
/// the window title re-render; history log rows keep the language they were
/// rendered in (they are data, not chrome).
pub(crate) fn apply_all_strings() {
    PUMP.with(|pump| {
        if let Some(controller) = pump.borrow().controller.clone() {
            controller.apply_ui_strings();
        }
    });
    super::tools_net::apply_strings();
    // ISSUE #34 (second splice point, same per-page pattern): the terminal
    // page joins the live language switch.
    super::tools_terminal::apply_strings();
    super::tools_mqtt::apply_strings();
    super::tools_plot::apply_strings();
    super::tools_codec::apply_strings();
    super::tools_monitor::apply_strings();
    super::tools_settings::apply_strings();
}

/// The payload preview on an entry's send button: escapes line breaks, marks hex
/// payloads, truncates.
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

/// Renders one data line: timestamp, direction, hex, text decoded with `encoding`.
/// (The host's `format_data_row` is its UTF-8 tracing twin.)
fn render_data_line(stamp: &str, direction: Direction, bytes: &[u8], encoding: TextEncoding) -> String {
    let direction = match direction {
        Direction::Rx => "RX",
        Direction::Tx => "TX",
    };
    let hex = if bytes.len() > crate::app::host::ROW_HEX_BYTES {
        format!(
            "{} …(+{} bytes)",
            hex_spaced(&bytes[..crate::app::host::ROW_HEX_BYTES]),
            bytes.len() - crate::app::host::ROW_HEX_BYTES
        )
    } else {
        hex_spaced(bytes)
    };
    let text_full = encoding.decode(bytes);
    let text = if text_full.chars().count() > crate::app::host::ROW_TEXT_CHARS {
        format!("{} …", text_full.chars().take(crate::app::host::ROW_TEXT_CHARS).collect::<String>())
    } else {
        text_full
    };
    format!("{stamp}  {:>2}  {}  \"{}\"", direction, hex, text.replace('"', "'"))
}

/// Subscribes the control events of the window (runs on the UI thread; the handlers
/// keep the controller alive for the window's lifetime).
fn wire_events(this: &Rc<MainController>) {
    this.open_close.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.toggle_open_close()
    });

    this.refresh_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.refresh_ports()
    });

    // The hide-sent toggle re-renders the view (TX rows filtered out).
    this.hide_sent.is_checked_changed({
        let this = Rc::clone(this);
        move |_, _| this.set_hide_sent()
    });

    this.send_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.send_box_text()
    });

    // Enter in the send box sends.
    this.send_box.key_down({
        let this = Rc::clone(this);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                this.send_box_text();
            }
        }
    });

    // The encoding selector re-renders the log and re-aims sends.
    this.encoding_selector.selection_changed({
        let this = Rc::clone(this);
        move |_, _| this.on_encoding_selected_index()
    });

    // The quick-send pager switches pages.
    this.quick_pager.selected_index_changed({
        let this = Rc::clone(this);
        move |_, args| MainController::show_quick_page(&this, args.new_index().max(0) as usize)
    });

    // The quick-send editor.
    this.quick_add.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| MainController::add_quick_entry(&this)
    });
    this.quick_save.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| MainController::save_quick_entry(&this)
    });
    this.quick_delete.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| MainController::delete_quick_entry(&this)
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
    this.script_delete.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.delete_script()
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
    if elapsed > Duration::from_millis(20) {
        tracing::warn!(target: "rcom::pump", "pump tick {:.1} ms — over the 20 ms budget",
            elapsed.as_secs_f64() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcom::core::codec::ParseHexError;

    const STAMP: &str = "03:04:05.123";

    /// A data line shows the direction, the hex and the decoded text — and decodes
    /// with the *given* encoding: GBK bytes render 你好 under GBK.
    #[test]
    fn data_line_decodes_with_the_selected_encoding() {
        let bytes = TextEncoding::Gbk.encode("你好");
        let gbk = render_data_line(STAMP, Direction::Tx, &bytes, TextEncoding::Gbk);
        assert_eq!(gbk, "03:04:05.123  TX  c4 e3 ba c3  \"你好\"");
        // The same raw bytes under UTF-8 (the pre-switch view) are lossy garbage.
        let utf8 = render_data_line(STAMP, Direction::Tx, &bytes, TextEncoding::Utf8);
        assert!(utf8.contains('\u{fffd}'), "{utf8}");
        assert!(utf8.contains("c4 e3 ba c3"), "{utf8}");
    }

    /// Oversized chunks abbreviate, embedded quotes stay readable.
    #[test]
    fn data_line_abbreviates_and_escapes() {
        let long = vec![0x61; crate::app::host::ROW_HEX_BYTES + 10];
        let line = render_data_line(STAMP, Direction::Rx, &long, TextEncoding::Utf8);
        assert!(line.contains("…(+10 bytes)"), "{line}");
        let quoted = render_data_line(STAMP, Direction::Tx, b"he\"y", TextEncoding::Utf8);
        assert!(quoted.contains("he'y"), "{quoted}");
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

    /// A bad hex payload is reported, not sent.
    #[test]
    fn hex_parse_error_is_displayable() {
        let error = parse_hex("zz").unwrap_err();
        assert_eq!(error, ParseHexError('z'));
    }

    /// The hide-sent filter drops exactly the TX lines — sent data (modelled as
    /// `Data` rows) and direction-tagged text — while RX and status lines stay.
    #[test]
    fn hide_sent_filters_only_tx_lines() {
        let tx = LogLine::Data { stamp: STAMP.into(), direction: Direction::Tx, bytes: vec![1] };
        let rx = LogLine::Data { stamp: STAMP.into(), direction: Direction::Rx, bytes: vec![2] };
        let status = LogLine::Text { text: "## x".into(), direction: None };

        assert!(!MainController::visible(&tx, true), "TX hidden while hide-sent");
        assert!(MainController::visible(&rx, true), "RX stays visible");
        assert!(MainController::visible(&status, true), "status stays visible");
        // Filter off: everything shows.
        assert!(MainController::visible(&tx, false));
        assert!(MainController::visible(&rx, false));
        assert!(MainController::visible(&status, false));
    }
}
