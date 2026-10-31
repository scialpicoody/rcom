//! The log view (design-ui §3.3): the per-session row model and the row
//! renderer. The model keeps raw bytes authoritative (an encoding switch
//! re-renders history without new traffic); the renderer builds one control
//! per row — `time │ direction chip │ hex+text` — the #30 prototype's
//! approved anatomy, ported to the app's imperative construction.

use ferroui_base::layout::{HorizontalAlignment, VerticalAlignment};
use ferroui_base::media::{Color, FontFamily, FontStyle, TextWrapping};
use ferroui_base::{CornerRadius, Ref, Thickness};
use ferroui_base::media::SolidColorBrush;
use ferroui_controls::{Border, Control, Grid, StackPanel, TextBlock};

use rcom::core::bus::Direction;
use rcom::core::codec::{hex_spaced, TextEncoding};

/// `rcom.font.mono` — the byte-surface fallback chain (one shared definition;
/// §1.8 of the token table).
pub fn mono() -> FontFamily {
    FontFamily::parse("Cascadia Mono,Consolas,Menlo,DejaVu Sans Mono").expect("monospace font family")
}

/// `rcom.font.mono.size` — 13 on byte surfaces, 11 timestamps/badges (§1.8).
pub const MONO_LOG_SIZE: f64 = 13.0;
pub const MONO_META_SIZE: f64 = 11.0;

/// Spacing scale `rcom.space.*` (§1.7).
pub const SPACE_1: f64 = 4.0;
pub const SPACE_2: f64 = 8.0;

// Log semantics colours (§1.6, the dark values the #30 prototype rendered;
// the theme-resource-driven variants ride with the settings polish later).
pub fn rx_color() -> Color {
    Color::from_rgb(0x6C, 0xCB, 0x5F)
}
pub fn tx_color() -> Color {
    Color::from_rgb(0x4E, 0xA3, 0xE8)
}
pub fn note_color() -> Color {
    Color::from_rgb(0xF7, 0xB2, 0x4B)
}
pub fn garbled_color() -> Color {
    Color::from_rgb(0xFF, 0x99, 0xA4)
}
fn garbled_wash() -> Color {
    Color::from_rgb(0x44, 0x27, 0x26)
}
fn text_secondary() -> Color {
    Color::from_argb(0xC5, 0xFF, 0xFF, 0xFF)
}
fn timestamp_color() -> Color {
    Color::from_argb(0x87, 0xFF, 0xFF, 0xFF)
}
fn text_tertiary() -> Color {
    timestamp_color()
}

/// A solid brush of `color` (properties take `Rc<dyn IBrush>`, not colours;
/// `Ref<SolidColorBrush>` upcasts via its `From` impl).
fn brush(color: Color) -> std::rc::Rc<dyn ferroui_base::media::IBrush> {
    SolidColorBrush::with_color(color).into()
}

/// One line of a session's log: a data chunk (raw bytes authoritative; the
/// hook marker rides along for post-hook TX rows) or a note/diagnostic line
/// (`error` renders the `!!` rows in the critical pair). The log *view* is a
/// filtered render of these lines under the session's presentation state; the
/// file sink and the log store stay complete.
#[derive(Debug, Clone)]
pub enum LogLine {
    Data {
        stamp: String,
        direction: Direction,
        bytes: Vec<u8>,
        hooked: bool,
    },
    Note {
        text: String,
        error: bool,
    },
}

impl LogLine {
    /// The direction a line shows, if any — the hide-sent filter's input.
    pub fn direction(&self) -> Option<Direction> {
        match self {
            LogLine::Data { direction, .. } => Some(*direction),
            LogLine::Note { .. } => None,
        }
    }

    /// Whether a line is visible under the session's presentation state:
    /// hide-sent removes TX rows from the *view* only, the substring filter
    /// matches the decoded text and the hex view alike (a hex needle finds the
    /// raw frame when the text is garbage — §3.3).
    pub fn visible(&self, needle: &str, hide_sent: bool, encoding: TextEncoding) -> bool {
        if hide_sent && self.direction() == Some(Direction::Tx) {
            return false;
        }
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_lowercase();
        match self {
            LogLine::Data { bytes, .. } => {
                encoding.decode(bytes).to_lowercase().contains(&needle)
                    || hex_spaced(bytes).contains(&needle)
            }
            LogLine::Note { text, .. } => text.to_lowercase().contains(&needle),
        }
    }

    /// Whether the row renders garbled: the decoder hit invalid sequences for
    /// the *current* encoding (`encoding_rs` had_errors — the row model's
    /// `garbled` flag, recomputed per render so an encoding switch stays
    /// honest about what is on screen).
    pub fn garbled(&self, encoding: TextEncoding) -> bool {
        match self {
            LogLine::Data { bytes, .. } => encoding.had_errors(bytes),
            LogLine::Note { .. } => false,
        }
    }
}

/// The log view mode (§3.3): which of the hex/text lines one row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Both,
    HexOnly,
    TextOnly,
}

impl ViewMode {
    /// The combo's indexes (`text + hex` / `hex only` / `text only`).
    pub const COMBO_LABELS: [&'static str; 3] = ["view_mode_both", "view_mode_hex", "view_mode_text"];

    pub fn from_index(index: i32) -> Self {
        match index {
            1 => ViewMode::HexOnly,
            2 => ViewMode::TextOnly,
            _ => ViewMode::Both,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            ViewMode::Both => 0,
            ViewMode::HexOnly => 1,
            ViewMode::TextOnly => 2,
        }
    }
}

/// Builds one log row control: `time │ badge │ content`, monospace, wrapping
/// content. Data rows show the hex line and the decoded text line (the llcom
/// dual view) under `mode`; note rows one dim italic `— ` line; garbled rows
/// the critical pair over the dark wash; hooked TX rows the ` ⌁hook` suffix
/// on the text line. `show_timestamps` off blanks the time column (§3.3's
/// timestamps checkbox) — the column stays, alignment survives.
pub fn build_row(
    line: &LogLine,
    mode: ViewMode,
    encoding: TextEncoding,
    show_timestamps: bool,
) -> Ref<Border> {
    let garbled = line.garbled(encoding);
    let row = Border::new();
    row.set_value(
        Border::background_property(),
        Some(brush(if garbled { garbled_wash() } else { Color::from_argb(0, 0, 0, 0) })),
    );
    row.set_margin(Thickness::uniform(0.0));
    row.set_padding(Thickness::new(SPACE_1, 1.0, SPACE_1, 1.0));

    let grid = Grid::new();
    if let Ok(columns) = ferroui_controls::ColumnDefinitions::parse("92,48,*") {
        grid.set_column_definitions(columns);
    }

    // Time column.
    let stamp = TextBlock::new();
    if let LogLine::Data { stamp: text, .. } = line {
        if show_timestamps {
            stamp.set_text(Some(text));
        }
    }
    style_meta_text(&stamp, timestamp_color());

    // Direction badge chip: translucent direction colour at ~18% alpha behind
    // 100%-alpha text (the label survives colour-blindness; §1.6).
    let badge = Border::new();
    let badge_label = TextBlock::new();
    match line {
        LogLine::Data { direction, .. } => {
            let (label, color) = match direction {
                Direction::Rx => ("RX", rx_color()),
                Direction::Tx => ("TX", tx_color()),
            };
            badge_label.set_text(Some(label));
            style_meta_text(&badge_label, color);
            badge.set_value(
                Border::background_property(),
                Some(brush(Color::from_argb(0x2E, color.r, color.g, color.b))),
            );
        }
        LogLine::Note { error, .. } => {
            let (label, color) = if *error { ("ERR", garbled_color()) } else { ("NOTE", note_color()) };
            badge_label.set_text(Some(label));
            style_meta_text(&badge_label, color);
            badge.set_value(
                Border::background_property(),
                Some(brush(Color::from_argb(0x2E, color.r, color.g, color.b))),
            );
        }
    }
    badge.set_corner_radius(CornerRadius::uniform(3.0));
    badge.set_margin(Thickness::new(0.0, 1.0, SPACE_2, 0.0));
    badge.set_padding(Thickness::new(SPACE_1, 0.0, SPACE_1, 0.0));
    badge.set_vertical_alignment(VerticalAlignment::Top);
    badge.set_horizontal_alignment(HorizontalAlignment::Left);
    badge.set_child(Some(badge_label.upcast::<ferroui_controls::Control>()));

    // Content column: hex + text lines under the view mode, or the note line.
    let content = StackPanel::new();
    content.set_spacing(0.0);
    match line {
        LogLine::Data { bytes, hooked, .. } => {
            let hex = hex_spaced(bytes);
            let mut text = encoding.decode(bytes);
            if *hooked {
                text.push_str(" ⌁hook");
            }
            let content_color = if garbled { garbled_color() } else { text_secondary() };
            let text_color = if garbled { garbled_color() } else { Color::from_rgb(0xFF, 0xFF, 0xFF) };
            if mode != ViewMode::TextOnly {
                let hex_line = TextBlock::new();
                hex_line.set_text(Some(&hex));
                style_log_text(&hex_line, content_color);
                content.children().add(hex_line.upcast::<ferroui_controls::Control>());
            }
            if mode != ViewMode::HexOnly {
                let text_line = TextBlock::new();
                text_line.set_text(Some(&text));
                style_log_text(&text_line, text_color);
                content.children().add(text_line.upcast::<ferroui_controls::Control>());
            }
        }
        LogLine::Note { text, .. } => {
            let note_line = TextBlock::new();
            note_line.set_text(Some(&format!("— {text}")));
            style_log_text(&note_line, text_tertiary());
            note_line.set_value(TextBlock::font_style_property(), FontStyle::Italic);
            content.children().add(note_line.upcast::<ferroui_controls::Control>());
        }
    }

    Grid::set_column(&stamp.clone().upcast::<Control>(), 0);
    grid.children().add(stamp.upcast::<Control>());
    Grid::set_column(&badge.clone().upcast::<Control>(), 1);
    grid.children().add(badge.upcast::<Control>());
    Grid::set_column(&content.clone().upcast::<Control>(), 2);
    grid.children().add(content.upcast::<Control>());
    row.set_child(Some(grid.upcast::<ferroui_controls::Control>()));
    row
}

fn style_meta_text(text: &Ref<TextBlock>, color: Color) {
    text.set_font_family(mono());
    text.set_font_size(MONO_META_SIZE);
    text.set_value(TextBlock::foreground_property(), Some(brush(color)));
    text.set_margin(Thickness::new(0.0, 2.0, SPACE_1, 0.0));
    text.set_vertical_alignment(VerticalAlignment::Top);
}

fn style_log_text(text: &Ref<TextBlock>, color: Color) {
    text.set_font_family(mono());
    text.set_font_size(MONO_LOG_SIZE);
    text.set_value(TextBlock::foreground_property(), Some(brush(color)));
    text.set_text_wrapping(TextWrapping::Wrap);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroui_base::StyledElement;

    const STAMP: &str = "03:04:05.123";

    fn data(bytes: &[u8], direction: Direction) -> LogLine {
        LogLine::Data {
            stamp: STAMP.into(),
            direction,
            bytes: bytes.to_vec(),
            hooked: false,
        }
    }

    /// Hide-sent drops exactly the TX lines; RX and notes stay.
    #[test]
    fn hide_sent_filters_only_tx() {
        let tx = data(b"a", Direction::Tx);
        let rx = data(b"b", Direction::Rx);
        let note = LogLine::Note { text: "x".into(), error: false };
        assert!(!tx.visible("", true, TextEncoding::Utf8));
        assert!(rx.visible("", true, TextEncoding::Utf8));
        assert!(note.visible("", true, TextEncoding::Utf8));
        assert!(tx.visible("", false, TextEncoding::Utf8));
    }

    /// The substring filter matches decoded text AND the hex view — a hex
    /// needle finds the raw frame when the text is garbage.
    #[test]
    fn filter_matches_text_and_hex() {
        let line = data("hello".as_bytes(), Direction::Rx);
        assert!(line.visible("ell", false, TextEncoding::Utf8));
        assert!(line.visible("68 65", false, TextEncoding::Utf8), "hex needle");
        assert!(!line.visible("6865", false, TextEncoding::Utf8), "the hex view is space-separated");
        assert!(!line.visible("zzz", false, TextEncoding::Utf8));

        // Garbage bytes: the text needle fails, the hex needle finds the frame.
        let junk = data(&[0x80, 0x81, 0x82], Direction::Rx);
        assert!(!junk.visible("temp", false, TextEncoding::Utf8));
        assert!(junk.visible("80 81", false, TextEncoding::Utf8));

        // Case-insensitive on both surfaces ("AbC" = 41 62 43).
        let mixed = data(b"AbC", Direction::Rx);
        assert!(mixed.visible("aBc", false, TextEncoding::Utf8));
        assert!(mixed.visible("41 62", false, TextEncoding::Utf8));

        // Notes filter on their text.
        let note = LogLine::Note { text: "script started: demo".into(), error: false };
        assert!(note.visible("started", false, TextEncoding::Utf8));
        assert!(!note.visible("stopped", false, TextEncoding::Utf8));
    }

    /// Garbled detection: invalid sequences for the current encoding flag the
    /// row; valid bytes never do; notes never do.
    #[test]
    fn garbled_follows_the_current_encoding() {
        let gbk_bytes = TextEncoding::Gbk.encode("你好");
        let line = LogLine::Data {
            stamp: STAMP.into(),
            direction: Direction::Rx,
            bytes: gbk_bytes.clone(),
            hooked: false,
        };
        assert!(!line.garbled(TextEncoding::Gbk), "valid GBK");
        assert!(line.garbled(TextEncoding::Utf8), "the same bytes are invalid UTF-8");

        let clean = data(b"plain ascii", Direction::Rx);
        assert!(!clean.garbled(TextEncoding::Utf8));

        let note = LogLine::Note { text: "x".into(), error: true };
        assert!(!note.garbled(TextEncoding::Utf8));
    }

    /// The hook marker lands on the text line only (§3.3), appended to the
    /// decoded text.
    #[test]
    fn hooked_rows_render_the_marker_on_the_text_line() {
        let line = LogLine::Data {
            stamp: STAMP.into(),
            direction: Direction::Tx,
            bytes: b"ABC".to_vec(),
            hooked: true,
        };
        let row = build_row(&line, ViewMode::Both, TextEncoding::Utf8, true);
        let texts = collect_texts(&row.upcast::<StyledElement>());
        // text+hex: the hex line has no marker, the text line does.
        assert!(texts.iter().any(|t| t.contains("41 42 43") && !t.contains("⌁hook")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("ABC ⌁hook")), "{texts:?}");

        // hex-only: the marker never shows (it lives on the text line).
        let row = build_row(&line, ViewMode::HexOnly, TextEncoding::Utf8, true);
        let texts = collect_texts(&row.upcast::<StyledElement>());
        assert!(!texts.iter().any(|t| t.contains("⌁hook")), "{texts:?}");
    }

    /// View modes: `hex only` hides the text line, `text only` the hex line,
    /// `both` stacks them; timestamps off blanks the time column.
    #[test]
    fn view_modes_render_mutually_exclusive_lines() {
        let line = data(b"hi", Direction::Rx);
        let both = collect_texts(&build_row(&line, ViewMode::Both, TextEncoding::Utf8, true).upcast::<StyledElement>());
        assert!(both.iter().any(|t| t.contains("68 69")));
        assert!(both.iter().any(|t| t.contains("hi")));
        assert!(both.iter().any(|t| t == STAMP));

        let hex = collect_texts(&build_row(&line, ViewMode::HexOnly, TextEncoding::Utf8, true).upcast::<StyledElement>());
        assert!(hex.iter().any(|t| t.contains("68 69")));
        assert!(!hex.iter().any(|t| t == "hi"));

        let text = collect_texts(&build_row(&line, ViewMode::TextOnly, TextEncoding::Utf8, true).upcast::<StyledElement>());
        assert!(text.iter().any(|t| t == "hi"));
        assert!(!text.iter().any(|t| t.contains("68 69")));

        let no_stamp = collect_texts(&build_row(&line, ViewMode::Both, TextEncoding::Utf8, false).upcast::<StyledElement>());
        assert!(!no_stamp.iter().any(|t| t == STAMP), "timestamps off blanks the time column");
    }

    /// Note rows render one dim italic line with the `— ` prefix; error notes
    /// carry the ERR badge.
    #[test]
    fn note_rows_render_distinct() {
        let note = LogLine::Note { text: "MOCK0 opened".into(), error: false };
        let texts = collect_texts(&build_row(&note, ViewMode::Both, TextEncoding::Utf8, true).upcast::<StyledElement>());
        assert!(texts.iter().any(|t| t.contains("— MOCK0 opened")), "{texts:?}");
        assert!(texts.iter().any(|t| t == "NOTE"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("68 ")), "notes have no hex");

        let error = LogLine::Note { text: "boom".into(), error: true };
        let texts = collect_texts(&build_row(&error, ViewMode::Both, TextEncoding::Utf8, true).upcast::<StyledElement>());
        assert!(texts.iter().any(|t| t == "ERR"), "{texts:?}");
    }

    /// Walks the built tree collecting TextBlock texts (headless controls).
    fn collect_texts(element: &Ref<StyledElement>) -> Vec<String> {
        let mut texts = Vec::new();
        walk(element, &mut texts);
        texts
    }

    fn walk(element: &Ref<StyledElement>, texts: &mut Vec<String>) {
        if let Some(text_block) = element.cast::<TextBlock>() {
            if let Some(text) = text_block.text() {
                texts.push(text);
            }
        }
        for child in element.logical_children().to_vec() {
            walk(&child, texts);
        }
    }
}
