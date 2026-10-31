//! The log-surface prototype's tokens, mock rows and row renderer
//! (ticket #30). Throwaway inline data, no real core behind it — the same
//! ground rules as `shell_common`.
//!
//! The visual values are the dark-variant values of the `#29` token draft
//! (`docs/ui-drafts/02-visual-language.md` §9), pinned here as constants so
//! the prototype is self-contained: the real app resolves them through the
//! theme's resource keys (`SystemFillColorSuccessBrush`, …), which a
//! headless example cannot do (dynamic-resource expressions are
//! `pub(crate)` at this pin; see the decision doc §1).
//!
//! Every control, property and builder named here was checked against the
//! pinned sources (`ferroui-*-0.4.0-preview.1` in the registry cache).

use ferroui_base::layout::VerticalAlignment;
use ferroui_base::media::{Color, FontStyle, FontFamily, TextWrapping};
use ferroui_controls::TextBlock;
use ferroui_view::prelude::*;

// ---------------------------------------------------------------------------
// Tokens (docs/ui-drafts/02-visual-language.md §6, §9 — dark variant)
// ---------------------------------------------------------------------------

/// `rcom.font.mono` — the byte-surface fallback chain (one definition,
/// shared; §6 of the token draft).
pub fn mono() -> FontFamily {
    FontFamily::parse("Cascadia Mono,Consolas,Menlo,DejaVu Sans Mono").expect("monospace font family")
}

/// `rcom.font.mono.size` — 13 log, 11 timestamps/badges (§6, owner Q3).
pub const MONO_LOG_SIZE: f64 = 13.0;
pub const MONO_META_SIZE: f64 = 11.0;

/// Spacing scale `rcom.space.*` (§5): only 4px-multiple values.
pub const SPACE_1: f64 = 4.0;
pub const SPACE_2: f64 = 8.0;

/// Log semantics (§9). RX is the theme's success colour; TX is the Q4
/// proposal (readable blue on dark); notes are amber — the theme's
/// `SystemFillColorAttentionBrush` follows the accent override and would
/// read as teal, so the token draft's amber alternative is what this
/// prototype proposes (decision doc §1); garbled is the theme's critical
/// pair, text + row wash.
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
/// The theme's dark `TextFillColorSecondary` (#C5FFFFFF) — the hex line,
/// dimmer than the text line.
fn text_secondary() -> Color {
    Color::from_argb(0xC5, 0xFF, 0xFF, 0xFF)
}
/// The theme's dark `TextFillColorTertiary` (#87FFFFFF) — timestamps.
fn timestamp_color() -> Color {
    Color::from_argb(0x87, 0xFF, 0xFF, 0xFF)
}
fn text_tertiary() -> Color {
    timestamp_color()
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// The direction of a log row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Received from the port.
    Rx,
    /// Sent to the port.
    Tx,
    /// A session note (port opened, hook state, …).
    Note,
}

/// One row of the session log: timestamp, direction, hex view, decoded text,
/// garbled flag (token draft §9 `rcom.log.garbled`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRow {
    pub seq: u64,
    pub time: String,
    pub dir: Direction,
    pub hex: String,
    pub text: String,
    pub garbled: bool,
    /// Set when the Lua pre-send hook rewrote the payload: `text`/`hex` are
    /// the POST-hook bytes (contract §1.3: `Tx` is post-hook, §6 AC 3).
    pub hooked: bool,
}

impl LogRow {
    pub fn new(seq: u64, time: &str, dir: Direction, hex: &str, text: &str) -> Self {
        Self { seq, time: time.to_string(), dir, hex: hex.to_string(), text: text.to_string(), garbled: false, hooked: false }
    }

    pub fn garbled(mut self) -> Self {
        self.garbled = true;
        self
    }

    pub fn hooked(mut self) -> Self {
        self.hooked = true;
        self
    }
}

/// Bytes → the preformatted hex view (lowercase, space-separated — contract
/// §1.3's `hex` field).
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

/// Bytes → lossy text (what a real session's `text` field is at emit time).
pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A TX row for a send: the post-hook bytes, both views.
pub fn sent_row(seq: u64, time: &str, bytes: &[u8], hooked: bool) -> LogRow {
    let mut row = LogRow::new(seq, time, Direction::Tx, &to_hex(bytes), &lossy(bytes));
    if hooked {
        row = row.hooked();
    }
    row
}

/// An info/note row (no hex, distinct styling).
pub fn note_row(seq: u64, time: &str, text: &str) -> LogRow {
    LogRow::new(seq, time, Direction::Note, "", text)
}

// ---------------------------------------------------------------------------
// The mock log: ~200 rows, mixed RX/TX/notes, GBK + UTF-8 non-ASCII, long
// wrapping lines, high- and low-rate sections, one garbled row.
// ---------------------------------------------------------------------------

/// The scripted opening section (hand-written, `12:00:xx`): exercises every
/// row-anatomy feature at least once.
fn scripted_rows(seq: &mut u64) -> Vec<LogRow> {
    let mut rows = Vec::new();
    let mut t = 0u64; // seconds-since-12:00:00, ticked below
    let push = |rows: &mut Vec<LogRow>, seq: &mut u64, t: &mut u64, ms: u64, dir: Direction, bytes: &[u8], text: &str| {
        *seq += 1;
        rows.push(LogRow::new(*seq, &stamp(*t, ms), dir, &to_hex(bytes), text));
    };
    fn stamp(t: u64, ms: u64) -> String {
        format!("12:{:02}:{:02}.{:03}", t / 60, t % 60, ms)
    }

    *seq += 1;
    rows.push(note_row(*seq, &stamp(t, 104), "port COM3 opened (115200 8N1, UTF-8)"));
    t += 1;
    push(&mut rows, seq, &mut t, 318, Direction::Rx, b"<SYS>", "<SYS>");
    push(&mut rows, seq, &mut t, 319, Direction::Rx, b"Ver=1.4.2\r\n", "Ver=1.4.2");
    push(&mut rows, seq, &mut t, 402, Direction::Tx, b"<ID>\r\n", "<ID>");
    push(&mut rows, seq, &mut t, 487, Direction::Rx, b"ID=SN92741", "ID=SN92741");
    push(&mut rows, seq, &mut t, 10, Direction::Rx, br#"{"t":25.3}"#, r#"{"t":25.3}"#);
    push(&mut rows, seq, &mut t, 105, Direction::Tx, b"AT+VER?\r", "AT+VER?");
    push(&mut rows, seq, &mut t, 230, Direction::Rx, b"OK 1.4.2", "OK 1.4.2");
    // GBK device banner: hex bytes are GBK, text is the decoded banner.
    t += 1;
    *seq += 1;
    rows.push(LogRow::new(*seq, &stamp(t, 880), Direction::Rx, "C6 B7 C5 C6 A3 BA B2 E2 CA D4", "品牌：测试 (GBK)"));
    *seq += 1;
    rows.push(note_row(*seq, &stamp(t, 1004), "encoding switched to GB18030"));
    t += 1;
    *seq += 1;
    rows.push(LogRow::new(*seq, &stamp(t, 101), Direction::Rx, "CE C2 B6 C8 A3 BA 32 35 2E 33", "温度：25.3"));
    t += 1;
    // A firmware-dump-length line that has to wrap.
    let burst_hex = to_hex(&[0xA5u8, 0x5A, 0xEB, 0x90, 0x00, 0xFF, 0x20, 0x01, 0x44, 0x33, 0x22, 0x11, 0x9A, 0x4C, 0x3E, 0x08].repeat(4));
    *seq += 1;
    rows.push(LogRow::new(
        *seq,
        &stamp(t, 512),
        Direction::Rx,
        &burst_hex,
        "binary telemetry burst, 64 bytes, decoded lossy — one very long line that has to wrap to stay readable, which is the wrapping policy under test",
    ));
    t += 2;
    push(&mut rows, seq, &mut t, 0, Direction::Tx, b"RST\r\n", "RST");
    t += 1;
    push(&mut rows, seq, &mut t, 318, Direction::Rx, b"<SYS>", "<SYS>");
    t += 1;
    push(&mut rows, seq, &mut t, 750, Direction::Rx, b"Hello world", "Hello world");
    push(&mut rows, seq, &mut t, 12, Direction::Tx, b"SCAN\r", "SCAN");
    t += 1;
    push(&mut rows, seq, &mut t, 344, Direction::Rx, b"WiFi: 6 nets", "WiFi: 6 nets");
    // UTF-8 Chinese status line.
    *seq += 1;
    rows.push(LogRow::new(*seq, &stamp(t, 90), Direction::Rx, "E7 8A B6 E6 80 81 3A 20 4F 4B", "状态: OK (UTF-8)"));
    t += 1;
    push(&mut rows, seq, &mut t, 222, Direction::Tx, b"CFG?\r\n", "CFG?");
    push(&mut rows, seq, &mut t, 388, Direction::Rx, b"mode=slave", "mode=slave");
    // A garbled row: junk high bytes, decoder hit invalid sequences.
    t += 1;
    *seq += 1;
    rows.push(LogRow::new(*seq, &stamp(t, 0), Direction::Rx, &to_hex(&[0xFFu8, 0xEE, 0x01, 0x9A, 0xC3, 0x28]), "…\u{FFFD}\u{FFFD}( — decoder hit invalid sequences").garbled());
    *seq += 1;
    rows.push(note_row(*seq, &stamp(t, 900), "script `poll.lua` started"));
    // Hook demonstration: a scripted TX rewritten by the pre-send hook
    // (post-hook bytes on the wire — contract §6 AC 3).
    t += 1;
    *seq += 1;
    rows.push(sent_row(*seq, &stamp(t, 1), b"poll;CHK=2E\r\n", true));
    t += 1;
    push(&mut rows, seq, &mut t, 318, Direction::Rx, b"P1=1234", "P1=1234");
    push(&mut rows, seq, &mut t, 318, Direction::Rx, b"P2=5678", "P2=5678");
    t += 2;
    // A long decode error line that wraps.
    *seq += 1;
    rows.push(LogRow::new(
        *seq,
        &stamp(t, 664),
        Direction::Rx,
        &to_hex(b"Error: sensor 2 timeout after 5000ms, retrying with backoff 250ms"),
        "Error: sensor 2 timeout after 5000ms, retrying with backoff 250ms and jitter, giving up after three attempts, please check the wiring of sensor 2 on the bus",
    ));
    t += 1;
    push(&mut rows, seq, &mut t, 100, Direction::Tx, b"poll\r\n", "poll");
    t += 1;
    push(&mut rows, seq, &mut t, 402, Direction::Rx, b"P2=5690", "P2=5690");
    *seq += 1;
    rows.push(LogRow::new(*seq, &stamp(t, 888), Direction::Rx, "E5 BF 83 E8 B7 B3 EF BC 9A 31", "心跳：1 (UTF-8)"));
    t += 1;
    push(&mut rows, seq, &mut t, 50, Direction::Tx, b"SAVE\r", "SAVE");
    t += 1;
    push(&mut rows, seq, &mut t, 290, Direction::Rx, b"Config saved", "Config saved");
    *seq += 1;
    rows.push(LogRow::new(*seq, &stamp(t, 670), Direction::Rx, "C9 E8 B1 B8 D2 D1 B1 A3 B4 E6", "设备已保存 (GBK)"));
    *seq += 1;
    rows.push(note_row(*seq, &stamp(t, 777), "log file rotated: rcom-com3-120001.log"));
    rows
}

/// The generated section: a deterministic sensor loop (LCG, no external
/// state) that repeats the device's high-rate/low-rate rhythm until the row
/// count reaches `target`. Timestamps continue the scripted clock.
fn generated_rows(seq: &mut u64, target: usize) -> Vec<LogRow> {
    let mut rows = Vec::new();
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut t: u64 = 130_000; // 12:02:10.000 in ms
    let mut pressure = 1013;
    let mut p1 = 1240u64;
    while rows.len() < target {
        // High-rate burst: 5 rows 60 ms apart …
        for i in 0..5 {
            t += 60;
            match i {
                0 => {
                    pressure += next() % 3;
                    let text = format!(r#"{{"t":{},"p":{}}}"#, 240 + (next() % 30) / 10, pressure);
                    push_gen(&mut rows, seq, t, Direction::Rx, text.as_bytes());
                }
                1 => {
                    p1 += 1;
                    push_gen(&mut rows, seq, t, Direction::Rx, format!("P1={p1}").as_bytes());
                }
                2 => {
                    // Occasional GBK heartbeat from the device (one byte of
                    // the payload varies, decoded here to the fixed banner).
                    let n = (next() % 10) as u8;
                    let gbk: [u8; 9] = [0xD0, 0xC4, 0xCC, 0xF8, 0xA3, 0xBA, 0x30 + n % 10, 0x0D, 0x0A];
                    let text = format!("心跳：{} (GBK)", n % 10);
                    rows.push(LogRow::new(increment(seq), &stamp_ms(t), Direction::Rx, &to_hex(&gbk), &text));
                }
                _ => {
                    push_gen(&mut rows, seq, t, Direction::Rx, format!("T={},H={}", 20 + (next() % 60) / 10, 40 + next() % 30).as_bytes());
                }
            }
        }
        // … then the host polls (TX), device answers, then a quiet stretch.
        t += 400;
        push_gen(&mut rows, seq, t, Direction::Tx, b"poll\r\n");
        t += 180;
        p1 += 1;
        push_gen(&mut rows, seq, t, Direction::Rx, format!("P1={p1}").as_bytes());
        t += 120;
        // One wrapped binary burst per cycle.
        let junk: Vec<u8> = (0..40).map(|_| (next() % 256) as u8).collect();
        push_gen(&mut rows, seq, t, Direction::Rx, &junk);
        t += 1_600; // quiet stretch: low-rate section
    }
    rows
}

fn increment(seq: &mut u64) -> u64 {
    *seq += 1;
    *seq
}

fn stamp_ms(t_ms: u64) -> String {
    let s = t_ms / 1000;
    format!("12:{:02}:{:02}.{:03}", s / 60, s % 60, t_ms % 1000)
}

fn push_gen(rows: &mut Vec<LogRow>, seq: &mut u64, t: u64, dir: Direction, bytes: &[u8]) {
    rows.push(LogRow::new(increment(seq), &stamp_ms(t), dir, &to_hex(bytes), &lossy(bytes)));
}

/// The full mock log at mount time: scripted + generated, ~`target` rows.
pub fn mock_rows(target: usize) -> Vec<LogRow> {
    let mut seq = 0u64;
    let mut rows = scripted_rows(&mut seq);
    let generated_target = target.saturating_sub(rows.len());
    rows.extend(generated_rows(&mut seq, generated_target));
    rows
}

// ---------------------------------------------------------------------------
// Row rendering
// ---------------------------------------------------------------------------

fn direction_label(dir: Direction) -> &'static str {
    match dir {
        Direction::Rx => "RX",
        Direction::Tx => "TX",
        Direction::Note => "NOTE",
    }
}

fn direction_color(dir: Direction) -> Color {
    match dir {
        Direction::Rx => rx_color(),
        Direction::Tx => tx_color(),
        Direction::Note => note_color(),
    }
}

/// One log row as a control: `time | badge | content`, monospace, wrapping
/// content. Data rows show the hex line and the text line (the llcom dual
/// view); note rows one dim italic line; garbled rows a critical wash.
/// Which of hex/text shows is decided by the caller (`content_view`).
pub fn log_row_view(row: LogRow, mode: usize) -> Element {
    // Garbled rows get the critical wash; everything else renders on the
    // pane's own background (a fully transparent brush — `Option<Color>`
    // has no `IntoValue` impl at this pin, so "no wash" is alpha-0).
    let wash = if row.garbled { garbled_wash() } else { Color::from_argb(0, 0, 0, 0) };
    let content = row_content(&row, mode);
    rsx! {
        Border {
            background: wash,
            margin: (0.0, 0.0, 0.0, 1.0),
            padding: (SPACE_1, 1.0, SPACE_1, 1.0),
            Grid {
                column_definitions: "92,48,*",
                TextBlock {
                    Grid.column: 0,
                    text: row.time,
                    font_family: mono(),
                    font_size: MONO_META_SIZE,
                    foreground: timestamp_color(),
                    margin: (0.0, 2.0, SPACE_1, 0.0),
                    vertical_alignment: VerticalAlignment::Top,
                }
                Border {
                    Grid.column: 1,
                    background: badge_background(row.dir),
                    corner_radius: 3.0,
                    margin: (0.0, 1.0, SPACE_2, 0.0),
                    padding: (SPACE_1, 0.0, SPACE_1, 0.0),
                    vertical_alignment: VerticalAlignment::Top,
                    horizontal_alignment: ferroui_base::layout::HorizontalAlignment::Left,
                    TextBlock {
                        text: direction_label(row.dir),
                        font_family: mono(),
                        font_size: 10.0,
                        foreground: direction_color(row.dir),
                    }
                }
                StackPanel {
                    Grid.column: 2,
                    spacing: 0.0,
                    { content }
                }
            }
        }
    }
}

/// Translucent badge backgrounds (the direction colour at ~18% alpha).
fn badge_background(dir: Direction) -> Color {
    let c = direction_color(dir);
    Color::from_argb(0x2E, c.r, c.g, c.b)
}

/// The hex/text lines for a row under a view mode: 0 = both, 1 = hex only,
/// 2 = text only (the mode combo's indexes).
fn row_content(row: &LogRow, mode: usize) -> Element {
    let hooked = if row.hooked { " ⌁hook" } else { "" };
    let hex_line = format!("{}{}", row.hex, hooked);
    let text_line = format!("{}{}", row.text, hooked);
    match mode {
        1 => {
            if row.hex.is_empty() {
                note_line(&row.text)
            } else {
                rsx! {
                    TextBlock {
                        text: hex_line,
                        text_wrapping: TextWrapping::Wrap,
                        font_family: mono(),
                        font_size: MONO_LOG_SIZE,
                        foreground: if row.garbled { garbled_color() } else { text_secondary() },
                    }
                }
            }
        }
        2 => {
            if row.hex.is_empty() {
                note_line(&row.text)
            } else {
                rsx! {
                    TextBlock {
                        text: text_line,
                        text_wrapping: TextWrapping::Wrap,
                        font_family: mono(),
                        font_size: MONO_LOG_SIZE,
                        font_style: FontStyle::Normal,
                        foreground: if row.garbled { garbled_color() } else { text_primary() },
                    }
                }
            }
        }
        _ => {
            if row.hex.is_empty() {
                note_line(&row.text)
            } else {
                rsx! {
                    StackPanel {
                        spacing: 0.0,
                        TextBlock {
                            text: row.hex.as_str(),
                            text_wrapping: TextWrapping::Wrap,
                            font_family: mono(),
                            font_size: MONO_LOG_SIZE,
                            foreground: if row.garbled { garbled_color() } else { text_secondary() },
                        }
                        TextBlock {
                            text: text_line,
                            text_wrapping: TextWrapping::Wrap,
                            font_family: mono(),
                            font_size: MONO_LOG_SIZE,
                            foreground: if row.garbled { garbled_color() } else { text_primary() },
                        }
                    }
                }
            }
        }
    }
}

fn note_line(text: &str) -> Element {
    rsx! {
        TextBlock {
            text: format!("— {text}"),
            text_wrapping: TextWrapping::Wrap,
            font_family: mono(),
            font_size: MONO_LOG_SIZE,
            font_style: FontStyle::Italic,
            foreground: text_tertiary(),
        }
    }
}

fn text_primary() -> Color {
    Color::from_rgb(0xFF, 0xFF, 0xFF)
}
