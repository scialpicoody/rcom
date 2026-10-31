//! The terminal page (issue #34): the bottom pane's keyboard direct-send tab.
//!
//! The page turns the keyboard into the port (design.md: "Terminal page
//! captures raw keys incl. ctrl+letters"): every keydown on the capture
//! surface is mapped to wire bytes and sent through the session's normal TX
//! path (`Command::SendBytes` → `manager.write` → the Lua pre-send hook — the
//! same seam the send bar and quick-send ride, so a rewriting hook is visible
//! on the terminal's TX rows too). Traffic renders in the session log (the
//! page is an input surface, not a second log — design-ui §4.3); the page's
//! own view is the **local echo** of the captured keys (toggle, off by
//! default), rendered apart from the log rows.
//!
//! Key-capture mechanics, verified against the `ferroui-base` 0.4.0-preview.1
//! sources (the ferro-dev bar — nothing here is guessed):
//!
//! - Key events are routed to the **focused element** and bubble
//!   (`KeyboardDevice::process_raw_event` → `InputElement::key_down_event`),
//!   carrying `Key`, `KeyModifiers` (ALT/CONTROL/SHIFT/META bitflags) and the
//!   platform-resolved `key_symbol` (the layout-aware unicode character).
//! - A plain `Control` can take keyboard focus (`Focusable` is a settable
//!   styled property; the focus manager's global pointer-press handler focuses
//!   the nearest focusable element under the pointer) and, unlike a
//!   `TextBox`, has **no class handling of its own** — Enter, Tab, Escape and
//!   the arrows all reach this page's handler unmodified (a `TextBox` would
//!   move the caret / insert a newline first).
//! - Tab and arrow navigation is an instance handler on the *window*
//!   (`KeyboardNavigationHandler::attach`), i.e. **after** the focused
//!   element's handlers in the bubble route — marking the event handled keeps
//!   focus navigation out of the way while the capture surface holds focus.
//! - While the session is closed the capture surface is disabled
//!   (`FocusManager::can_focus` requires `is_effectively_enabled`), so the
//!   page cannot steal keys; while another control holds focus the events
//!   simply go there — the deliberate design-ui §4.3 exception to "controls
//!   stay enabled".
//! - The open/closed state rides the Open/Close button's content property
//!   observable (the controller's `set_open_state` writes that label on every
//!   `PortState` message; `get_observable` fires the current value on
//!   subscribe and on every change — no shared-file edits needed).
//!
//! The key→bytes table (also rendered as the page's mapping note):
//! `Ctrl+A–Z → 0x01–0x1A` (virtual-key based, Shift ignored — terminal
//! convention); `Ctrl+symbol → byte & 0x1F` for the classic set
//! (`` @[\]^_ `` + lowercase + `` {|}~ ``, e.g. `Ctrl+[ → 0x1B`, `Ctrl+@ → 0x00`);
//! `Enter → CR | LF | CRLF` per the header selector (CR default);
//! `Tab → 0x09`, `Shift+Tab → ESC [ Z`; `Escape → 0x1B`; `Backspace → 0x7F`
//! (VT; `Ctrl+H` still gives 0x08 through the ctrl mapping); arrows and
//! Home/End → VT CSI (`ESC [ A`…`ESC [ F`); Insert/Delete/PageUp/PageDown →
//! `ESC [ n ~`; `F1–F4 → ESC O P…S` (SS3), `F5–F12 → ESC [ n ~`; printable
//! characters from the platform `key_symbol` (ASCII as one byte, other
//! characters as their UTF-8 bytes — an honest pass-through), falling back to
//! the virtual key (US layout) when the symbol is absent. **Alt/Meta combos
//! send nothing** — they stay with the app (menus, Cmd shortcuts).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::InputElementImpl;
use ferroui_base::input::{Key, KeyModifiers};
use ferroui_base::interactivity::InteractiveImpl;
use ferroui_base::layout::LayoutableImpl;
use ferroui_base::media::immutable::{ImmutablePen, ImmutableSolidColorBrush};
use ferroui_base::media::{
    Color, DrawingContext, FlowDirection, FormattedText, IBrush, IPen, Typeface,
};
use ferroui_base::reactive::{IDisposable, ObservableExt};
use ferroui_base::threading::{DispatcherPriority, DispatcherTimer};
use ferroui_base::utilities::CultureInfo;
use ferroui_base::{
    ferro_class, ferro_class_info, ferro_impl_classes, instantiate, FerroObjectExtensions,
    FerroObjectImpl, Point, Rect, Ref, Size, StyledElement, StyledElementImpl, VisualImpl,
};
use ferroui_controls::{
    Button, CheckBox, ComboBox, ContentControl, Control, ControlImpl, Grid, ListBox,
    StackPanel, TabItem, TextBlock,
};

use rust_i18n::t;

use super::host::{core_host, Command};
use rcom::core::codec::hex_spaced;
use rcom::core::port::mock::MOCK_PORT_NAME;

// ---- The key→bytes mapping (pure; unit-tested headlessly) ------------------

/// What Enter emits (the header selector; CR is the serial default — llcom
/// parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewlineMode {
    Cr,
    Lf,
    CrLf,
}

impl NewlineMode {
    /// The mode a selector index stands for (out-of-range → `Cr`).
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => NewlineMode::Lf,
            2 => NewlineMode::CrLf,
            _ => NewlineMode::Cr,
        }
    }

    /// The selector label (byte names, proper nouns — not translated).
    pub fn label(self) -> &'static str {
        match self {
            NewlineMode::Cr => "CR",
            NewlineMode::Lf => "LF",
            NewlineMode::CrLf => "CRLF",
        }
    }

    /// The wire bytes Enter emits in this mode.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            NewlineMode::Cr => &[0x0D],
            NewlineMode::Lf => &[0x0A],
            NewlineMode::CrLf => &[0x0D, 0x0A],
        }
    }
}

/// One keyboard event distilled to what the mapping reads: the virtual key,
/// the modifiers and the platform-resolved unicode symbol (layout-aware) —
/// exactly what a `KeyEventArgs` carries.
#[derive(Debug, Clone)]
pub struct KeyStroke {
    pub key: Key,
    pub modifiers: KeyModifiers,
    pub symbol: Option<String>,
}

impl KeyStroke {
    pub fn new(key: Key, modifiers: KeyModifiers, symbol: Option<&str>) -> Self {
        Self { key, modifiers, symbol: symbol.map(str::to_string) }
    }
}

/// The VT CSI sequence `ESC [ n ~` (Insert/Delete/PageUp/PageDown/F5+); the
/// parameter is decimal without leading zeros (`ESC [ 3 ~`, `ESC [ 1 5 ~`).
fn csi_tilde(n: u8) -> Vec<u8> {
    let mut out = vec![0x1B, b'['];
    if n >= 10 {
        out.push(b'0' + n / 10);
    }
    out.push(b'0' + n % 10);
    out.push(b'~');
    out
}

/// The VT SS3 sequence `ESC O c` (F1–F4).
fn ss3(c: u8) -> Vec<u8> {
    vec![0x1B, b'O', c]
}

/// Maps one keystroke to the wire bytes it stands for, or `None` when the
/// page sends nothing for it (modifier keys alone, Alt/Meta combos, keys with
/// no honest byte mapping). See the module docs for the full table.
pub fn map_key(stroke: &KeyStroke, newline: NewlineMode) -> Option<Vec<u8>> {
    // Alt/Meta combos stay with the app (menus, Cmd shortcuts) — the
    // terminal never prefixes ESC for Alt at this honesty bar.
    if stroke.modifiers.intersects(KeyModifiers::META | KeyModifiers::ALT) {
        return None;
    }
    if stroke.modifiers.contains(KeyModifiers::CONTROL) {
        return ctrl_byte(stroke.key, stroke.symbol.as_deref());
    }
    match stroke.key {
        Key::Return => Some(newline.bytes().to_vec()),
        Key::LineFeed => Some(vec![0x0A]),
        Key::Tab => {
            if stroke.modifiers.contains(KeyModifiers::SHIFT) {
                Some(vec![0x1B, b'[', b'Z']) // backtab
            } else {
                Some(vec![0x09])
            }
        }
        Key::Escape => Some(vec![0x1B]),
        Key::Back => Some(vec![0x7F]), // Backspace → DEL (VT); Ctrl+H → 0x08 via the ctrl mapping
        Key::Up => Some(vec![0x1B, b'[', b'A']),
        Key::Down => Some(vec![0x1B, b'[', b'B']),
        Key::Right => Some(vec![0x1B, b'[', b'C']),
        Key::Left => Some(vec![0x1B, b'[', b'D']),
        Key::Home => Some(vec![0x1B, b'[', b'H']),
        Key::End => Some(vec![0x1B, b'[', b'F']),
        Key::Insert => Some(csi_tilde(2)),
        Key::Delete => Some(csi_tilde(3)),
        Key::PageUp => Some(csi_tilde(5)),
        Key::PageDown => Some(csi_tilde(6)),
        Key::F1 => Some(ss3(b'P')),
        Key::F2 => Some(ss3(b'Q')),
        Key::F3 => Some(ss3(b'R')),
        Key::F4 => Some(ss3(b'S')),
        Key::F5 => Some(csi_tilde(15)),
        Key::F6 => Some(csi_tilde(17)),
        Key::F7 => Some(csi_tilde(18)),
        Key::F8 => Some(csi_tilde(19)),
        Key::F9 => Some(csi_tilde(20)),
        Key::F10 => Some(csi_tilde(21)),
        Key::F11 => Some(csi_tilde(23)),
        Key::F12 => Some(csi_tilde(24)),
        _ => printable(stroke.key, stroke.modifiers, stroke.symbol.as_deref()),
    }
}

/// The Ctrl+key branch: letters by virtual key (0x01–0x1A, Shift ignored —
/// `Ctrl+Shift+C` is still 0x03), symbols by the layout-resolved character
/// through the classic `& 0x1F` mapping (`Ctrl+[ → 0x1B`, `Ctrl+@ → 0x00`).
/// Digits are excluded — their control assignments are not mnemonic.
fn ctrl_byte(key: Key, symbol: Option<&str>) -> Option<Vec<u8>> {
    if (Key::A..=Key::Z).contains(&key) {
        return Some(vec![(key as i32 - Key::A as i32 + 1) as u8]);
    }
    let ch = symbol.and_then(|s| s.chars().next())? as u32;
    if (0x40..=0x7F).contains(&ch) {
        Some(vec![(ch as u8) & 0x1F])
    } else {
        None
    }
}

/// The printable branch: the platform symbol wins (Shift, layouts and
/// AltGr all resolve correctly there); the virtual key is the fallback
/// (US layout). Non-ASCII symbols go out as their UTF-8 bytes.
fn printable(key: Key, modifiers: KeyModifiers, symbol: Option<&str>) -> Option<Vec<u8>> {
    if let Some(symbol) = symbol {
        let mut chars = symbol.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let mut buffer = [0u8; 4];
            return Some(ch.encode_utf8(&mut buffer).as_bytes().to_vec());
        }
    }
    match key {
        Key::Space => Some(vec![0x20]),
        key if (Key::A..=Key::Z).contains(&key) => {
            let letter = b'a' + (key as i32 - Key::A as i32) as u8;
            Some(vec![if modifiers.contains(KeyModifiers::SHIFT) { letter - 32 } else { letter }])
        }
        key if (Key::D0..=Key::D9).contains(&key) => {
            let digit = b'0' + (key as i32 - Key::D0 as i32) as u8;
            Some(vec![digit])
        }
        _ => None,
    }
}

/// The local-echo rendering of one mapped chunk: printable ASCII verbatim,
/// the line/tab/escape controls named, everything else as `\xNN`.
pub fn render_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        match byte {
            0x0D => out.push_str("\\r"),
            0x0A => out.push_str("\\n"),
            0x09 => out.push_str("\\t"),
            0x1B => out.push_str("\\e"),
            0x20..=0x7E => out.push(byte as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out
}

/// One echo row of the page's view: the outgoing bytes with their rendering —
/// distinct from the session log's `stamp TX hex "text"` rows (this is the
/// page's own local-echo view, not a second log).
pub fn format_echo_row(bytes: &[u8]) -> String {
    format!("→ {}  {}", hex_spaced(bytes), render_bytes(bytes))
}

// ---- The capture surface (custom control; the PlotView pattern) ------------

/// The look of the capture strip (the plot's oscilloscope dark field, so the
/// terminal reads as a byte surface under either theme; active border = the
/// app ramp's `SystemAccentColorLight1` #15BBD3).
const STRIP_BG: (u8, u8, u8) = (24, 26, 32);
const STRIP_BORDER_IDLE: (u8, u8, u8) = (58, 62, 70);
const STRIP_BORDER_ACTIVE: (u8, u8, u8) = (21, 187, 211);
const STRIP_TEXT: (u8, u8, u8) = (196, 200, 208);

fn brush(rgb: (u8, u8, u8)) -> Rc<dyn IBrush> {
    Rc::new(ImmutableSolidColorBrush::new(Color::from_rgb(rgb.0, rgb.1, rgb.2)))
}

fn pen(rgb: (u8, u8, u8)) -> Rc<dyn IPen> {
    Rc::new(ImmutablePen::with_brush(
        Some(Rc::new(ImmutableSolidColorBrush::new(Color::from_rgb(rgb.0, rgb.1, rgb.2)))),
        1.0,
    ))
}

/// The keyboard-capture strip: a plain focusable `Control` that renders its
/// hint (click-to-capture idle / capturing active). No template, no class
/// key handling — every key reaches the page's handler while focused.
#[repr(C)]
pub struct TerminalCaptureView {
    base: Control,
    /// The hint line to draw (the page keeps it in sync with locale + focus).
    hint: Rc<RefCell<String>>,
    /// Whether the strip currently holds keyboard focus.
    active: Cell<bool>,
}

ferro_class!(TerminalCaptureView: Control);
ferro_class_info!(TerminalCaptureView { new: TerminalCaptureView::new });
// The Impl traits with nothing overridden (the PlotView pattern);
// `VisualImpl` is implemented by hand below — listing it here would collide.
ferro_impl_classes!(TerminalCaptureView: FerroObjectImpl, StyledElementImpl, LayoutableImpl, InteractiveImpl, InputElementImpl, ControlImpl);

impl VisualImpl for TerminalCaptureView {
    fn render(this: &Self, dc: &mut DrawingContext) {
        let bounds = this.bounds();
        let (width, height) = (bounds.width, bounds.height);
        if width < 8.0 || height < 8.0 {
            return;
        }
        dc.fill_rectangle(&brush(STRIP_BG), Rect::from_size(Size::new(width, height)), 4.0);
        let border = if this.active.get() { STRIP_BORDER_ACTIVE } else { STRIP_BORDER_IDLE };
        dc.draw_rectangle_outline(&pen(border), Rect::from_size(Size::new(width, height)), 4.0);
        let hint = this.hint.borrow().clone();
        if hint.is_empty() {
            return;
        }
        let text = FormattedText::new(
            &hint,
            CultureInfo::invariant_culture(),
            FlowDirection::LeftToRight,
            Typeface::default_typeface(),
            12.0,
            Some(brush(STRIP_TEXT)),
        );
        dc.draw_text(&text, Point::new(10.0, (height - text.height()) / 2.0));
    }
}

impl TerminalCaptureView {
    /// Creates the class data; see [`ferroui_base::FerroObject::construct`].
    pub fn construct() -> Self {
        Self {
            base: Control::construct(),
            hint: Rc::new(RefCell::new(String::new())),
            active: Cell::new(false),
        }
    }

    pub fn new() -> Ref<Self> {
        instantiate(Self::construct())
    }

    /// The handle the page uses to keep the drawn hint in sync.
    pub fn hint_handle(&self) -> Rc<RefCell<String>> {
        Rc::clone(&self.hint)
    }

    fn set_active(&self, active: bool) {
        self.active.set(active);
        self.invalidate_visual();
    }
}

// ---- The page --------------------------------------------------------------

/// How many rows the local-echo view keeps (the newest visible; the session
/// log has its own, larger cap).
const ECHO_ROW_CAP: usize = 500;

/// One page instance: the resolved controls plus its state.
struct TerminalPage {
    tab: Ref<TabItem>,
    newline_caption: Ref<TextBlock>,
    newline: Ref<ComboBox>,
    echo: Ref<CheckBox>,
    status: Ref<TextBlock>,
    echo_view: Ref<ListBox>,
    empty: Ref<StackPanel>,
    empty_caption: Ref<TextBlock>,
    empty_hint: Ref<TextBlock>,
    /// The key→bytes table rendered under the strip (the AC's "document the
    /// mapping table in the page's status area").
    mapping_note: Ref<TextBlock>,
    capture_host: Ref<Grid>,
    capture: Ref<TerminalCaptureView>,
    /// The session log view (the controller's `LogList`, resolved in the same
    /// name scope; used read-only by the demo's wire-echo proof).
    /// The Open/Close button: its content ("Close" while the session counts
    /// as active) is the open-state signal this page reads.
    open_close: Ref<Button>,
    /// The session's open state, mirrored from the Open/Close button's label.
    open: Cell<bool>,
    /// The autodemo's ticking timer (dropped when the demo finishes).
    demo_timer: RefCell<Option<Rc<DispatcherTimer>>>,
    /// The demo's tick counter.
    demo_tick: Cell<u32>,
}

impl TerminalPage {
    /// Applies the locale strings (attach time; re-run on a live switch — the
    /// capture strip's drawn hint included).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("terminal_tab").to_string())));
        self.newline_caption.set_text(Some(&t!("terminal_newline")));
        self.echo.set_content(Some(Rc::new(t!("terminal_echo").to_string())));
        self.empty_caption.set_text(Some(&t!("terminal_empty_caption")));
        self.empty_hint.set_text(Some(&t!("terminal_empty_hint")));
        self.mapping_note.set_text(Some(&t!("terminal_mapping_note")));
        self.refresh_status();
        self.sync_capture_hint();
    }

    /// The newline mode the selector stands for.
    fn newline_mode(&self) -> NewlineMode {
        NewlineMode::from_index(self.newline.selected_index())
    }

    /// The status line: latest state only (§4.1) — closed → the empty-state
    /// copy; open → the capture instruction with the Enter mode.
    fn refresh_status(&self) {
        let text = if !self.open.get() {
            t!("terminal_status_closed").to_string()
        } else {
            t!("terminal_status_ready", nl = self.newline_mode().label()).to_string()
        };
        self.status.set_text(Some(&text));
    }

    /// The capture strip's drawn hint (idle vs capturing wording).
    fn sync_capture_hint(&self) {
        let hint = if self.capture.active.get() {
            t!("terminal_capture_active").to_string()
        } else {
            t!("terminal_capture_hint").to_string()
        };
        *self.capture.hint_handle().borrow_mut() = hint;
        self.capture.invalidate_visual();
    }

    /// The empty-state overlay: visible while the session is closed (the
    /// §4.1/§4.3 one-caption empty state; gone once the port is open).
    fn sync_empty_state(&self) {
        self.empty.set_is_visible(!self.open.get());
    }

    /// The Open/Close button's content changed: the label reads "Close" while
    /// the session counts as active (`Open | Reopened | WatchDesired` — the
    /// controller's own rule). Locale-correct by construction: the controller
    /// writes the label from the same `t!` this check compares against.
    fn sync_open_state(&self) {
        let label = self
            .open_close
            .content()
            .as_ref()
            .and_then(|value| value.downcast_ref::<String>().cloned());
        let open = label.is_some_and(|label| label == t!("close").to_string());
        if open == self.open.get() {
            return;
        }
        self.open.set(open);
        // The deliberate empty-state exception (§4.3): the capture input is
        // disabled while the session is closed; the session toolbar's open
        // control is the way out.
        self.capture.set_is_enabled(open);
        self.refresh_status();
        self.sync_empty_state();
        self.sync_capture_hint();
    }

    /// The capture surface's keydown: map → send through the session TX path
    /// (`Command::SendBytes`, hook applies) → local echo when on. Returns
    /// whether the page consumed the key (the handler marks it handled, which
    /// keeps the window's focus navigation out of the way too).
    fn on_key(&self, stroke: &KeyStroke) -> bool {
        if !self.open.get() {
            return false; // disabled surfaces cannot hold focus; belt and braces
        }
        let Some(bytes) = map_key(stroke, self.newline_mode()) else {
            return false; // modifier-only / Alt-Meta / unmapped: not the terminal's key
        };
        core_host().commands.send(Command::SendBytes { session_id: None, bytes: bytes.clone() }).ok();
        if self.echo.is_checked() == Some(true) {
            self.append_echo_row(&bytes);
        }
        true
    }

    /// One row into the echo view (capped; newest visible, auto-scrolled —
    /// the net page's list mechanics).
    fn append_echo_row(&self, bytes: &[u8]) {
        let items = self.echo_view.items();
        if items.count() >= ECHO_ROW_CAP {
            items.remove_at(0);
        }
        let value: ferroui_base::BoxedValue = Rc::new(format_echo_row(bytes));
        items.add(Some(value));
        let count = self.echo_view.item_count();
        if count > 0 {
            self.echo_view.scroll_into_view(count - 1);
        }
    }

    // ---- The RCOM_TERMINALDEMO=1 autodemo (headless proof) ------------------

    /// Starts the headless demo on a dispatcher timer (the sanctioned
    /// UI-thread bridge; the pump's pattern): open MOCK0 → synthesize the key
    /// events for `ping` + Return + Ctrl+C through the page's real capture
    /// path → verify the wire echo in the session log and the local echo in
    /// the page view.
    fn start_demo(self: &Rc<Self>) {
        tracing::info!(
            "RCOM_TERMINALDEMO=1: driving the terminal demo path (open {MOCK_PORT_NAME}, \
             synthesize keys 'ping' + Return + Ctrl+C, expect 70 69 6e 67 0d 03 on the wire)"
        );
        let timer = DispatcherTimer::with_callback(
            Duration::from_millis(250),
            DispatcherPriority::BACKGROUND,
            {
                let page = Rc::clone(self);
                move |_| page.demo_tick()
            },
        );
        *self.demo_timer.borrow_mut() = Some(timer);
    }

    fn demo_tick(self: &Rc<Self>) {
        self.demo_tick.set(self.demo_tick.get() + 1);
        match self.demo_tick.get() {
            1 => {
                // The same command the Open button sends.
                core_host()
                    .commands
                    .send(Command::Open { name: MOCK_PORT_NAME.into(), baud: 115_200 })
                    .ok();
            }
            4 => {
                // Local echo on through the real toggle path, then the keys —
                // each through `on_key`, exactly what a focused capture strip
                // does per keydown (symbol-free: the virtual-key fallback path).
                self.echo.set_is_checked(Some(true));
                for stroke in [
                    KeyStroke::new(Key::P, KeyModifiers::NONE, None),
                    KeyStroke::new(Key::I, KeyModifiers::NONE, None),
                    KeyStroke::new(Key::N, KeyModifiers::NONE, None),
                    KeyStroke::new(Key::G, KeyModifiers::NONE, None),
                    KeyStroke::new(Key::Return, KeyModifiers::NONE, None),
                    KeyStroke::new(Key::C, KeyModifiers::CONTROL, None),
                ] {
                    if !self.on_key(&stroke) {
                        tracing::error!(
                            "AUTODEMO TERMINAL FAILED: the page refused {stroke:?} (port open?)"
                        );
                    }
                }
            }
            8 => {
                self.demo_verify();
                *self.demo_timer.borrow_mut() = None; // dropping the timer stops it
            }
            _ => {}
        }
    }

    /// The proof: the per-key sends came back over MOCK0's echo — the session
    /// log shows the `70 69 6e 67 0d 03` aggregate as RX rows — and the
    /// page's echo view carries the six local-echo rows.
    fn demo_verify(&self) {
        let needles: Vec<String> =
            [0x70u8, 0x69, 0x6e, 0x67, 0x0d, 0x03].iter().map(|b| rx_needle(*b)).collect();
        let missing = self.find_missing_rows(&needles);
        let echo_rows = self.echo_view.item_count();
        if missing.is_empty() && echo_rows >= needles.len() as i32 {
            tracing::info!(
                "AUTODEMO TERMINAL OK: synthesized keys p i n g Return Ctrl+C reached the \
                 wire as 70 69 6e 67 0d 03 — per-key sends through Command::SendBytes (the \
                 Lua pre-send hook seam), MOCK0 echoed byte-exact (RX rows in the session \
                 log), and the local-echo view carries {echo_rows} rows"
            );
        } else {
            tracing::error!(
                "AUTODEMO TERMINAL FAILED: missing echo rows {missing:?} (echo rows: {echo_rows})"
            );
        }
    }

    /// The session log's rows at the model level (the controller's active
    /// session VM — the source the rendered rows come from), so the proof
    /// reads what the shell shows without walking the row tree.
    fn find_missing_rows(&self, needles: &[String]) -> Vec<String> {
        let rows = super::controller::active_session_row_texts();
        tracing::debug!(target: "rcom::terminal_demo", "scan: {} model rows", rows.len());
        let mut missing: Vec<String> = needles.to_vec();
        for row in &rows {
            missing.retain(|needle| !row.contains(needle.as_str()));
            if missing.is_empty() {
                break;
            }
        }
        missing
    }
}

/// The RX-row needle for one echo byte: the #37 shell's log row is
/// `[time][badge "RX"][hex][text]` cells collected by `find_missing_rows`
/// as space-joined texts, so `RX 70` pins the echo of `0x70` (and a TX row's
/// badge is "TX", so the send itself never matches).
fn rx_needle(byte: u8) -> String {
    format!("RX {byte:02x}")
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle to the page (the event handlers and the demo timer
    /// run there; the pattern of the controller's PUMP and the other pages).
    static PAGE: RefCell<Option<Rc<TerminalPage>>> = RefCell::new(None);

    /// The property-observable subscriptions (the open-state signal and the
    /// tab-selected focus nudge). Kept outside the page so the subscription
    /// closures can hold the page `Rc` without a cycle; app-lifetime, like
    /// the pages themselves.
    static SUBSCRIPTIONS: RefCell<Vec<Rc<dyn IDisposable>>> = RefCell::new(Vec::new());
}

/// Resolves the named controls of the terminal tab, builds the capture strip,
/// sets the strings and wires the events. One insertion point in the
/// controller's `attach`.
pub fn attach(element: &Ref<StyledElement>) {
    // The capture strip: a plain focusable control (built here, not in the
    // XAML — the PlotView pattern). It takes keyboard focus on click (the
    // focus manager focuses the nearest focusable element under the pointer);
    // while the session is closed it is disabled and cannot.
    let capture = TerminalCaptureView::new();
    capture.set_focusable(true);

    let page = Rc::new(TerminalPage {
        tab: NameScopeExtensions::get(element, "TerminalTab"),
        newline_caption: NameScopeExtensions::get(element, "TerminalNewlineCaption"),
        newline: NameScopeExtensions::get(element, "TerminalNewlineSelector"),
        echo: NameScopeExtensions::get(element, "TerminalEchoCheck"),
        status: NameScopeExtensions::get(element, "TerminalStatus"),
        echo_view: NameScopeExtensions::get(element, "TerminalEchoView"),
        empty: NameScopeExtensions::get(element, "TerminalEmpty"),
        empty_caption: NameScopeExtensions::get(element, "TerminalEmptyCaption"),
        empty_hint: NameScopeExtensions::get(element, "TerminalEmptyHint"),
        mapping_note: NameScopeExtensions::get(element, "TerminalMappingNote"),
        capture_host: NameScopeExtensions::get(element, "TerminalCaptureHost"),
        open_close: NameScopeExtensions::get(element, "OpenCloseButton"),
        capture,
        open: Cell::new(false),
        demo_timer: RefCell::new(None),
        demo_tick: Cell::new(0),
    });

    // The capture strip into its host: it stretches with the pane.
    page.capture_host.children().add(page.capture.clone().upcast::<Control>());

    page.apply_ui_strings();
    page.sync_empty_state();

    // The newline selector: CR / LF / CRLF (byte names, untranslated), CR
    // preselected — the serial default.
    let items = page.newline.items();
    for mode in [NewlineMode::Cr, NewlineMode::Lf, NewlineMode::CrLf] {
        let value: ferroui_base::BoxedValue = Rc::new(mode.label().to_string());
        items.add(Some(value));
    }
    page.newline.set_selected_index(0);

    // The capture surface's keydown: the page maps and sends; consumed keys
    // are marked handled so the window's focus navigation (an instance
    // handler further up the bubble route) stays out of the way.
    page.capture.key_down({
        let page = Rc::clone(&page);
        move |_, args| {
            let stroke = KeyStroke::new(args.key, args.key_modifiers, args.key_symbol.as_deref());
            if page.on_key(&stroke) {
                args.set_handled(true);
            }
        }
    });
    // Focus feedback: the strip redraws its border + wording, the status line
    // follows.
    page.capture.got_focus({
        let page = Rc::clone(&page);
        move |_, _| {
            page.capture.set_active(true);
            page.sync_capture_hint();
        }
    });
    page.capture.lost_focus({
        let page = Rc::clone(&page);
        move |_, _| {
            page.capture.set_active(false);
            page.sync_capture_hint();
        }
    });
    // The newline mode rides the status line's Enter wording.
    page.newline.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.refresh_status()
    });

    // The open-state signal: the Open/Close button's content (fires the
    // current value on subscribe, then on every change — the controller's
    // `set_open_state` writes it per `UiMsg::PortState`). The trait call is
    // explicit: `Interactive` also has an inherent `get_observable` (for
    // routed events), which would otherwise win the method resolution.
    SUBSCRIPTIONS.with(|cell| {
        let open_close: &ferroui_base::FerroObject = &page.open_close;
        cell.borrow_mut().push(
            FerroObjectExtensions::get_observable(open_close, ContentControl::content_property())
                .subscribe_fn({
                    let page = Rc::clone(&page);
                    move |_: Option<ferroui_base::BoxedValue>| page.sync_open_state()
                }),
        );
    });

    // Selecting the Terminal tab focuses the capture strip (when the session
    // is open) — the keyboard lands where typing goes; focus() failing while
    // the content tree settles is harmless (the click path always works).
    SUBSCRIPTIONS.with(|cell| {
        let tab: &ferroui_base::FerroObject = &page.tab;
        cell.borrow_mut().push(
            FerroObjectExtensions::get_observable(tab, TabItem::is_selected_property())
                .subscribe_fn({
                    let page = Rc::clone(&page);
                    move |selected: bool| {
                        if selected && page.open.get() {
                            page.capture.focus();
                        }
                    }
                }),
        );
    });

    let demo = std::env::var("RCOM_TERMINALDEMO").as_deref() == Ok("1");

    PAGE.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&page)));

    if demo {
        page.start_demo();
    }
}

/// Re-applies the locale strings after a language switch (the settings page's
/// live apply — ticket #25).
pub fn apply_strings() {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.apply_ui_strings();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(key: Key, modifiers: KeyModifiers) -> KeyStroke {
        KeyStroke::new(key, modifiers, None)
    }

    /// Ctrl+letters arrive as the control bytes 0x01–0x1A, Shift ignored
    /// (the AC's headline row).
    #[test]
    fn ctrl_letters_map_to_control_bytes() {
        assert_eq!(map_key(&stroke(Key::C, KeyModifiers::CONTROL), NewlineMode::Cr), Some(vec![0x03]));
        assert_eq!(map_key(&stroke(Key::A, KeyModifiers::CONTROL), NewlineMode::Cr), Some(vec![0x01]));
        assert_eq!(map_key(&stroke(Key::Z, KeyModifiers::CONTROL), NewlineMode::Cr), Some(vec![0x1A]));
        assert_eq!(
            map_key(&stroke(Key::C, KeyModifiers::CONTROL | KeyModifiers::SHIFT), NewlineMode::Cr),
            Some(vec![0x03])
        );
        // Right-ctrl is the same modifier flag.
        assert_eq!(
            map_key(&stroke(Key::T, KeyModifiers::CONTROL), NewlineMode::Cr),
            Some(vec![0x14])
        );
    }

    /// The classic Ctrl+symbol mapping through the layout symbol
    /// (`Ctrl+[ → 0x1B`, `Ctrl+@ → NUL`), digits excluded.
    #[test]
    fn ctrl_symbols_map_through_the_layout_symbol() {
        assert_eq!(
            map_key(&KeyStroke::new(Key::OemOpenBrackets, KeyModifiers::CONTROL, Some("[")), NewlineMode::Cr),
            Some(vec![0x1B])
        );
        assert_eq!(
            map_key(&KeyStroke::new(Key::Oem3, KeyModifiers::CONTROL, Some("@")), NewlineMode::Cr),
            Some(vec![0x00])
        );
        // A digit with ctrl: no honest mnemonic mapping — nothing sent.
        assert_eq!(
            map_key(&KeyStroke::new(Key::D2, KeyModifiers::CONTROL, Some("2")), NewlineMode::Cr),
            None
        );
    }

    /// Enter honours the newline selector (CR default); Ctrl+J's LineFeed and
    /// Shift+Tab's backtab are their own bytes.
    #[test]
    fn enter_variants_follow_the_selector() {
        let enter = stroke(Key::Return, KeyModifiers::NONE);
        assert_eq!(map_key(&enter, NewlineMode::Cr), Some(vec![0x0D]));
        assert_eq!(map_key(&enter, NewlineMode::Lf), Some(vec![0x0A]));
        assert_eq!(map_key(&enter, NewlineMode::CrLf), Some(vec![0x0D, 0x0A]));
        assert_eq!(map_key(&stroke(Key::LineFeed, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x0A]));
        assert_eq!(map_key(&stroke(Key::Tab, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x09]));
        assert_eq!(
            map_key(&stroke(Key::Tab, KeyModifiers::SHIFT), NewlineMode::Cr),
            Some(vec![0x1B, b'[', b'Z'])
        );
        // Out-of-range selector indices fall back to CR.
        assert_eq!(NewlineMode::from_index(-1), NewlineMode::Cr);
        assert_eq!(NewlineMode::from_index(9), NewlineMode::Cr);
    }

    /// Printable ASCII via the layout symbol (Shift-correct), with the
    /// virtual-key fallback when the symbol is absent.
    #[test]
    fn printable_keys_map_to_ascii() {
        assert_eq!(
            map_key(&KeyStroke::new(Key::P, KeyModifiers::NONE, Some("p")), NewlineMode::Cr),
            Some(b"p".to_vec())
        );
        assert_eq!(
            map_key(&KeyStroke::new(Key::P, KeyModifiers::SHIFT, Some("P")), NewlineMode::Cr),
            Some(b"P".to_vec())
        );
        assert_eq!(map_key(&stroke(Key::P, KeyModifiers::NONE), NewlineMode::Cr), Some(b"p".to_vec()));
        assert_eq!(
            map_key(&stroke(Key::P, KeyModifiers::SHIFT), NewlineMode::Cr),
            Some(b"P".to_vec())
        );
        assert_eq!(map_key(&stroke(Key::D7, KeyModifiers::NONE), NewlineMode::Cr), Some(b"7".to_vec()));
        assert_eq!(map_key(&stroke(Key::Space, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x20]));
        // A layout symbol the virtual key cannot name (e.g. '?' on Shift+/)
        // still lands — and non-ASCII passes through as UTF-8.
        assert_eq!(
            map_key(&KeyStroke::new(Key::OemQuestion, KeyModifiers::SHIFT, Some("?")), NewlineMode::Cr),
            Some(b"?".to_vec())
        );
        assert_eq!(
            map_key(&KeyStroke::new(Key::None, KeyModifiers::NONE, Some("你")), NewlineMode::Cr),
            Some("你".as_bytes().to_vec())
        );
    }

    /// The VT-ish special keys: Escape, Backspace, arrows/Home/End as CSI,
    /// Insert/Delete/PgUp/PgDn and F-keys as `ESC [ n ~` / SS3.
    #[test]
    fn special_keys_map_to_vt_bytes() {
        assert_eq!(map_key(&stroke(Key::Escape, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B]));
        assert_eq!(map_key(&stroke(Key::Back, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x7F]));
        assert_eq!(map_key(&stroke(Key::Up, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B, b'[', b'A']));
        assert_eq!(map_key(&stroke(Key::Down, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B, b'[', b'B']));
        assert_eq!(map_key(&stroke(Key::Right, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B, b'[', b'C']));
        assert_eq!(map_key(&stroke(Key::Left, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B, b'[', b'D']));
        assert_eq!(map_key(&stroke(Key::Home, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B, b'[', b'H']));
        assert_eq!(map_key(&stroke(Key::End, KeyModifiers::NONE), NewlineMode::Cr), Some(vec![0x1B, b'[', b'F']));
        assert_eq!(
            map_key(&stroke(Key::Delete, KeyModifiers::NONE), NewlineMode::Cr),
            Some(vec![0x1B, b'[', b'3', b'~'])
        );
        assert_eq!(
            map_key(&stroke(Key::PageUp, KeyModifiers::NONE), NewlineMode::Cr),
            Some(vec![0x1B, b'[', b'5', b'~'])
        );
        assert_eq!(
            map_key(&stroke(Key::F1, KeyModifiers::NONE), NewlineMode::Cr),
            Some(vec![0x1B, b'O', b'P'])
        );
        assert_eq!(
            map_key(&stroke(Key::F5, KeyModifiers::NONE), NewlineMode::Cr),
            Some(vec![0x1B, b'[', b'1', b'5', b'~'])
        );
    }

    /// Alt/Meta combos stay with the app; bare modifiers map to nothing.
    #[test]
    fn alt_meta_and_bare_modifiers_send_nothing() {
        assert_eq!(map_key(&stroke(Key::P, KeyModifiers::ALT), NewlineMode::Cr), None);
        assert_eq!(map_key(&stroke(Key::C, KeyModifiers::META), NewlineMode::Cr), None);
        assert_eq!(map_key(&stroke(Key::LeftShift, KeyModifiers::NONE), NewlineMode::Cr), None);
        assert_eq!(map_key(&stroke(Key::None, KeyModifiers::NONE), NewlineMode::Cr), None);
    }

    /// The demo's aggregate: the `ping\r` + Ctrl+C keystrokes map to exactly
    /// `70 69 6e 67 0d 03` (the wire bytes the autodemo proves).
    #[test]
    fn the_ping_ctrl_c_sequence_maps_to_the_demo_bytes() {
        let mut wire: Vec<u8> = Vec::new();
        for key in [Key::P, Key::I, Key::N, Key::G] {
            wire.extend(map_key(&stroke(key, KeyModifiers::NONE), NewlineMode::Cr).unwrap());
        }
        wire.extend(map_key(&stroke(Key::Return, KeyModifiers::NONE), NewlineMode::Cr).unwrap());
        wire.extend(map_key(&stroke(Key::C, KeyModifiers::CONTROL), NewlineMode::Cr).unwrap());
        assert_eq!(wire, vec![0x70, 0x69, 0x6e, 0x67, 0x0d, 0x03]);
    }

    /// The echo rendering: printable verbatim, the controls named, the rest
    /// hex-escaped; the row carries the spaced hex.
    #[test]
    fn echo_rows_render_bytes_readably() {
        assert_eq!(render_bytes(b"ping"), "ping");
        assert_eq!(render_bytes(&[0x0D]), "\\r");
        assert_eq!(render_bytes(&[0x03]), "\\x03");
        assert_eq!(render_bytes(&[0x1B, b'[', b'A']), "\\e[A");
        assert_eq!(render_bytes(&[0x0D, 0x0A]), "\\r\\n");
        assert_eq!(format_echo_row(&[0x70, 0x0d]), "→ 70 0d  p\\r");
    }

    /// The demo's RX needle pins the echo row shape (badge `RX` + hex cells,
    /// space-joined by the scan).
    #[test]
    fn rx_needles_match_the_log_row_shape() {
        let row = "RX 70 p";
        assert!(row.contains(&rx_needle(0x70)));
        assert!(!row.contains(&rx_needle(0x69)));
        // A TX row (the send itself) is not mistaken for the echo.
        let tx_row = "TX 70 p";
        assert!(!tx_row.contains(&rx_needle(0x70)));
    }
}
