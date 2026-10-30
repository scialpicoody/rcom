//! The controller of `MainWindow`: looks the named controls up in the window's name
//! scope, wires their events to core commands, and pumps the core's messages into
//! the log view. Lives on the UI thread only (`Rc` state; every field is accessed
//! from handlers and the dispatcher-timer pump, which run there).
//!
//! Ticket #17 additions: the encoding selector (display + send), the log's row
//! model that retains raw bytes per row so an encoding switch re-renders history
//! (docs/ui-drafts/05-contract.md §1.3), and the quick-send pages panel. Visible
//! strings come from the i18n dictionaries (`locales/`, via `t!`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::layout::Orientation;
use ferroui_base::threading::{DispatcherPriority, DispatcherTimer};
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{
    Button, CheckBox, ComboBox, Control, ListBox, PipsPager, StackPanel, TextBox, TextBlock,
};

use super::host::{core_host, AutodemoMode, Command, UiMsg};
use super::quicksend::{QuickSendEntry, QuickSendStore, PAGE_COUNT};
use rcom::core::bus::{Direction, PortState};
use rcom::core::codec::{hex_spaced, parse_hex, TextEncoding};
use rcom::core::port::PortInfo;

use rust_i18n::t;

/// The baud rates the baud selector offers; 115200 is preselected.
const BAUD_RATES: [u32; 7] = [9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800];

/// How many rows the log view keeps before it drops the oldest: the virtualized
/// list keeps memory sane, llcom-style. Applies to the row model too (the model
/// holds the raw bytes the rows re-render from).
const ROW_CAP: usize = 5000;

/// How often the UI pumps messages from the core driver.
const PUMP_INTERVAL: Duration = Duration::from_millis(30);

/// One line of the log view: pre-formatted text (state rows, diagnostics) or a
/// data chunk with its raw bytes — the authoritative form an encoding switch
/// re-renders from.
#[derive(Debug, Clone)]
enum LogLine {
    Text(String),
    Data { stamp: String, direction: Direction, bytes: Vec<u8> },
}

pub struct MainController {
    port_selector: Ref<ComboBox>,
    baud_selector: Ref<ComboBox>,
    open_close: Ref<Button>,
    refresh_button: Ref<Button>,
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
    // State.
    ports: RefCell<Vec<PortInfo>>,
    is_open: Cell<bool>,
    encoding: Cell<TextEncoding>,
    lines: RefCell<Vec<LogLine>>,
    quick_store: RefCell<QuickSendStore>,
    quick_page: Cell<usize>,
    quick_editing: Cell<Option<usize>>,
}

impl MainController {
    /// Builds the controller for `window`: resolves the named controls of
    /// `MainWindow.xaml`, subscribes their events and starts the pump. Call on the
    /// UI thread, after the window's markup is populated.
    pub fn attach(window: &Ref<crate::MainWindow>) {
        let element: &Ref<StyledElement> = window.upcast_ref();
        window.set_title(Some(t!("title").to_string()));

        let this = Rc::new(Self {
            port_selector: NameScopeExtensions::get(element, "PortSelector"),
            baud_selector: NameScopeExtensions::get(element, "BaudSelector"),
            open_close: NameScopeExtensions::get(element, "OpenCloseButton"),
            refresh_button: NameScopeExtensions::get(element, "RefreshButton"),
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
            ports: RefCell::new(Vec::new()),
            is_open: Cell::new(false),
            encoding: Cell::new(TextEncoding::Utf8),
            lines: RefCell::new(Vec::new()),
            quick_store: RefCell::new(QuickSendStore::default()),
            quick_page: Cell::new(0),
            quick_editing: Cell::new(None),
        });

        this.apply_ui_strings();
        this.fill_baud_selector();
        this.fill_encoding_selector();
        MainController::setup_quick_send(&this);
        wire_events(&this);
        start_pump(&this);
        MainController::autodemo_persist_check(&this);

        // Kick off the first port listing.
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    /// Every visible string of the window, from the dictionaries (the XAML carries
    /// none, so both locales ship from the same document).
    fn apply_ui_strings(&self) {
        self.refresh_button.set_content(Some(Rc::new(t!("refresh").to_string())));
        self.send_button.set_content(Some(Rc::new(t!("send").to_string())));
        self.send_box.set_placeholder_text(Some(&t!("send_hint")));
        self.port_caption.set_text(Some(&t!("port")));
        self.baud_caption.set_text(Some(&t!("baud")));
        self.encoding_caption.set_text(Some(&t!("encoding")));

        self.quick_send_title.set_text(Some(&t!("quick_send")));
        self.quick_label_caption.set_text(Some(&t!("quick_label")));
        self.quick_payload_caption.set_text(Some(&t!("quick_payload")));
        self.quick_hex_check.set_content(Some(Rc::new(t!("quick_hex").to_string())));
        self.quick_add.set_content(Some(Rc::new(t!("quick_add").to_string())));
        self.quick_save.set_content(Some(Rc::new(t!("quick_save").to_string())));
        self.quick_delete.set_content(Some(Rc::new(t!("quick_delete").to_string())));
        self.quick_label_box.set_placeholder_text(Some(&t!("quick_label_hint")));
        self.quick_payload_box.set_placeholder_text(Some(&t!("quick_payload_hint")));

        // The Open/Close label tracks the port state.
        self.set_open_state(if self.is_open.get() { PortState::Open } else { PortState::Closed });
    }

    fn fill_baud_selector(&self) {
        let items = self.baud_selector.items();
        for rate in BAUD_RATES {
            let value: BoxedValue = Rc::new(rate.to_string());
            items.add(Some(value));
        }
        let default = BAUD_RATES.iter().position(|rate| *rate == 115_200).unwrap_or(0);
        self.baud_selector.set_selected_index(default as i32);
    }

    /// The encoding selector: one entry per [`TextEncoding`], UTF-8 preselected.
    /// The names are proper nouns (see [`TextEncoding::display_name`]).
    fn fill_encoding_selector(&self) {
        let items = self.encoding_selector.items();
        for encoding in TextEncoding::all() {
            let value: BoxedValue = Rc::new(encoding.display_name().to_string());
            items.add(Some(value));
        }
        self.encoding_selector.set_selected_index(0);
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
            self.append_line(LogLine::Text(format!("## {}", t!("select_port_first"))));
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
                self.append_line(LogLine::Text(format!("!! {message}")));
            }
        }
    }

    fn set_open_state(&self, state: PortState) {
        self.is_open.set(state == PortState::Open);
        let label = if self.is_open.get() { t!("close") } else { t!("open") };
        self.open_close.set_content(Some(Rc::new(label.to_string())));
    }

    // ---- The log view: model + rendering ------------------------------------

    /// Renders one model line with the current encoding.
    fn render_line(&self, line: &LogLine) -> String {
        match line {
            LogLine::Text(text) => text.clone(),
            LogLine::Data { stamp, direction, bytes } => {
                render_data_line(stamp, *direction, bytes, self.encoding.get())
            }
        }
    }

    /// Appends a line to the model and the view (dropping the oldest past the cap).
    fn append_line(&self, line: LogLine) {
        let rendered = self.render_line(&line);
        let mut lines = self.lines.borrow_mut();
        let items = self.log.items();
        if lines.len() >= ROW_CAP {
            lines.remove(0);
            items.remove_at(0);
        }
        lines.push(line);
        drop(lines);
        let value: BoxedValue = Rc::new(rendered);
        items.add(Some(value));
        // Auto-scroll to the newest row (virtualization makes this cheap).
        if self.log.item_count() > 0 {
            self.log.scroll_into_view(self.log.item_count() - 1);
        }
    }

    /// Re-renders every retained line — the encoding-switch path. The raw bytes
    /// stayed in the model, so history decodes anew instead of staying frozen in
    /// the encoding of the moment (contract §1.3).
    fn re_render_rows(&self) {
        let lines = self.lines.borrow();
        let items = self.log.items();
        items.clear();
        for line in lines.iter() {
            let value: BoxedValue = Rc::new(self.render_line(line));
            items.add(Some(value));
        }
        drop(lines);
        if self.log.item_count() > 0 {
            self.log.scroll_into_view(self.log.item_count() - 1);
        }
    }

    /// Applies an encoding chosen in the selector.
    fn set_encoding(&self, encoding: TextEncoding) {
        self.encoding.set(encoding);
        self.re_render_rows();
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

    // ---- The pump -----------------------------------------------------------

    fn apply(&self, message: UiMsg) {
        match message {
            UiMsg::Row(row) => self.append_line(LogLine::Text(row)),
            UiMsg::Data(data) => self.append_line(LogLine::Data {
                stamp: data.stamp,
                direction: data.direction,
                bytes: data.bytes,
            }),
            UiMsg::Ports(ports) => self.show_ports(ports),
            UiMsg::PortState(state) => self.set_open_state(state),
            UiMsg::SetEncoding(encoding) => self.apply_set_encoding(encoding),
        }
    }

    fn show_ports(&self, ports: Vec<PortInfo>) {
        let remember = self.port_selector.selected_index();
        let count = ports.len() as i32;
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
        *self.ports.borrow_mut() = ports;
        // Keep the previous selection when it still exists, else the first port.
        let kept = if remember >= 0 && remember < count {
            remember
        } else if count > 0 {
            0
        } else {
            -1
        };
        self.port_selector.set_selected_index(kept);
    }
}

/// The payload preview on an entry's send button: escapes line breaks, marks hex
/// payloads, truncates.
fn entry_button_text(entry: &QuickSendEntry) -> String {
    let payload = if entry.hex {
        format!("[hex] {}", entry.payload)
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
    let messages: Vec<UiMsg> = core_host().ui_rx.lock().expect("ui pump mutex").try_iter().collect();
    if let Some(controller) = PUMP.with(|pump| pump.borrow().controller.clone()) {
        for message in messages {
            controller.apply(message);
        }
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
}
