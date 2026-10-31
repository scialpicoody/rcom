//! The log surface prototype (ticket #30): the daily-driver view rendered
//! with realistic mock rows, for a human to react to.
//!
//! What renders (see `docs/ui-drafts/03-log-interaction.md` for the
//! decisions behind every piece):
//!
//! 1. **Row anatomy** — `time | direction badge | content` per row,
//!    monospace (`rcom.font.mono`), hex + decoded text stacked (the llcom
//!    dual view), long lines wrap, note rows distinct, one garbled row in
//!    the critical wash.
//! 2. **Auto-scroll** — a "follow" toggle (default on); scrolling up
//!    disengages it automatically. New rows arrive live from a mock
//!    simulator timer at mixed rates.
//! 3. **Filter + view modes** — a substring filter box and a
//!    text+hex / hex-only / text-only combo.
//! 4. **Send bar** — text field (Enter sends), HEX toggle, send; appends
//!    TX rows (mock, goes nowhere).
//! 5. **Quick-send panel** — 10 pages (`PipsPager`), inline-editable
//!    entries (edit-in-place, no dialog), a pre-send-hook indicator badge;
//!    with the hook checked, sends are rewritten (`;CHK=xx` suffix, the
//!    mock hook) and the TX row shows the post-hook bytes with a `⌁hook`
//!    marker (contract §1.3 / §6 AC 3).
//!
//! Styled with the WinUI theme (compact density) and the #29 tokens: the
//! seven `SystemAccentColor*` resources are overridden with the teal ramp
//! on the window (§3 of the token draft), so the theme's accent brushes
//! resolve teal inside this window.
//!
//! ```text
//! cargo run --example log_prototype
//! FERROUI_SMOKE_EXIT_MS=3000 cargo run --example log_prototype
//! cargo run --example log_prototype -- --selfcheck   # headless tree check
//! ```

#[path = "log_prototype_common/mod.rs"]
mod common;

use common::{mono, Direction, LogRow, SPACE_2};
use ferroui_base::input::Key;
use ferroui_base::layout::{HorizontalAlignment, Orientation, VerticalAlignment};
use ferroui_base::media::{Color, FontWeight};
use ferroui_base::styling::ThemeVariant;
use ferroui_base::threading::{DispatcherPriority, DispatcherTimer};
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::primitives::ScrollBarVisibility;
use ferroui_controls::{Dock, ScrollViewer, TextBlock};
use ferroui_themes_winui::{DensityStyle, WinUITheme};
use ferroui_view::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

/// Mock rows mounted at start: enough to scroll, few enough to mount fast.
const INITIAL_ROWS: usize = 200;
/// Entries per quick-send page (llcom's layout: 6 rows of label + value).
const ENTRIES_PER_PAGE: usize = 6;
const QUICK_SEND_PAGES: usize = 10;

/// The needles the selfcheck looks for in the "both" view.
const SELFCHECK_BOTH: &[&str] = &[
    "<SYS>",                                   // a decoded text line
    "3C 53 59 53 3E",                          // the same row's hex line
    "品牌：测试 (GBK)",                          // GBK row
    "状态: OK (UTF-8)",                          // UTF-8 row
    "which is the wrapping policy under test", // the wrapped long line
    "Quick send",                              // panel header
    "read temp",                               // page-1 quick-send entry label
    "NOTE",                                    // note-row badge
    "RX",                                      // direction badge
];

fn main() {
    if std::env::args().any(|arg| arg == "--selfcheck") {
        std::process::exit(selfcheck());
    }
    ferroui_view::launch_themed(
        // The WinUI theme, compact density (§5 of the token draft).
        || {
            let theme = WinUITheme::new();
            theme.set_density_style(DensityStyle::Compact);
            theme.as_style()
        },
        move || {
            rsx! {
                Window {
                    title: "rcom — log surface prototype (#30, mock)",
                    width: 1280.0,
                    height: 860.0,
                    { content(common::mock_rows(INITIAL_ROWS), true, 0, "") }
                }
            }
        },
        // The #29 dark default (§4 of the token draft) and accent override
        // (§3), applied to the window before the main loop runs: the window
        // is a theme-variant root, so the requested variant becomes the
        // actual one for the whole tree. Without the variant the window
        // follows the OS — on a light-mode Mac the log renders light and the
        // dark-tuned row colours are unreadable (observed on this host).
        |window| {
            window.set_value(ThemeVariant::requested_theme_variant_property(), Some(ThemeVariant::dark()));
            let dict = window.resources();
            for (key, color) in accent_ramp() {
                let boxed: BoxedValue = Rc::new(color);
                dict.set(key, Some(boxed));
            }
        },
    );
}

/// `SystemAccentColor*` — the teal ramp of `docs/ui-drafts/02-visual-language.md` §3.
fn accent_ramp() -> [(&'static str, Color); 7] {
    [
        ("SystemAccentColor", Color::from_rgb(0x0E, 0x7C, 0x8C)),
        ("SystemAccentColorDark1", Color::from_rgb(0x09, 0x4E, 0x58)),
        ("SystemAccentColorDark2", Color::from_rgb(0x05, 0x2D, 0x33)),
        ("SystemAccentColorDark3", Color::from_rgb(0x00, 0x04, 0x05)),
        ("SystemAccentColorLight1", Color::from_rgb(0x15, 0xBB, 0xD3)),
        ("SystemAccentColorLight2", Color::from_rgb(0x3B, 0xD5, 0xEB)),
        ("SystemAccentColorLight3", Color::from_rgb(0x77, 0xE2, 0xF1)),
    ]
}

/// The whole surface. `live` starts the mock row simulator (the smoke run
/// and the demo want rows arriving; the selfcheck mounts without it);
/// `initial_mode`/`initial_filter` preset the view mode (selfcheck).
fn content(init: Vec<LogRow>, live: bool, initial_mode: i32, initial_filter: &str) -> Element {
    // --- state -----------------------------------------------------------
    let rows = signal(init);
    let seq = signal(rows.with(|v| v.len() as u64));
    let follow = signal(true); // auto-scroll (§2 of the decision doc)
    let view_mode = signal(initial_mode); // 0 both, 1 hex only, 2 text only
    let filter = signal(initial_filter.to_string()); // substring filter
    let draft = signal(String::new()); // send-bar text
    let hex_send = signal(false); // send-bar HEX toggle
    let hook_on = signal(false); // pre-send hook active
    let page = signal(0i32); // quick-send page (0..10)

    // Quick-send entries: every value is its own signal, so editing in
    // place persists across page switches.
    let entries: Vec<(&'static str, Signal<String>)> = quick_send_pages()
        .into_iter()
        .flatten()
        .map(|(label, value)| (label, signal(value.to_string())))
        .collect();

    // --- derived ---------------------------------------------------------
    let filtered = memo(move |_| {
        let needle = filter.get().to_lowercase();
        rows.with(|v| v.iter().filter(|row| matches(row, &needle)).cloned().collect::<Vec<LogRow>>())
    });
    let shown = memo(move |_| filtered.get().len());
    let status = memo(move |_| {
        format!(
            "shown {} / {} rows · view {} · follow {} · UTF-8 (mock — no port)",
            shown.get(),
            rows.with(|v| v.len()),
            match view_mode.get() {
                1 => "hex only",
                2 => "text only",
                _ => "text + hex",
            },
            if follow.get() { "on" } else { "PAUSED" },
        )
    });
    let page_label = memo(move |_| format!("{}/{}", page.get() + 1, QUICK_SEND_PAGES));
    // The hook badge follows the hook switch reactively (colour cross-fade
    // is the theme's; the values here are the token draft's amber note pair
    // against a translucent wash).
    let badge_text = memo(move |_| if hook_on.get() { "⌁ hook" } else { "hook off" });
    let badge_bg = memo(move |_| {
        if hook_on.get() {
            Color::from_argb(0x38, 0xF7, 0xB2, 0x4B)
        } else {
            Color::from_argb(0x14, 0xFF, 0xFF, 0xFF)
        }
    });
    let badge_fg = memo(move |_| {
        if hook_on.get() {
            common::note_color()
        } else {
            Color::from_argb(0x87, 0xFF, 0xFF, 0xFF)
        }
    });

    // --- auto-scroll ------------------------------------------------------
    let scroller = NodeRef::<ScrollViewer>::new();
    // Follow every batch of new rows (and filter/mode changes that grow the
    // list) by jumping to the end — no smooth scrolling: log rows never
    // animate (§7 of the token draft).
    effect(move || {
        let count = filtered.get().len();
        if follow.get() && count > 0 {
            if let Some(s) = scroller.get_untracked() {
                s.scroll_to_end();
            }
        }
    });

    // --- the mock row simulator ------------------------------------------
    if live {
        let tick = Rc::new(Cell::new(0u64));
        // Mixed rates: a 12-tick cycle of 250 ms — 5 busy ticks (2 rows
        // each), 3 medium ticks (1 row), 4 quiet ticks. Kept alive for the
        // process lifetime (one leaked timer per run; the process exits
        // with the window, smoke runs included).
        let (t, rows_t, seq_t) = (tick.clone(), rows, seq);
        let timer = DispatcherTimer::with_callback(
            std::time::Duration::from_millis(250),
            DispatcherPriority::NORMAL,
            move |_| {
                let n = t.get();
                t.set(n + 1);
                if rows_t.with(|v| v.len()) > INITIAL_ROWS + 800 {
                    return; // cap the demo
                }
                let emits = match n % 12 {
                    0..=4 => 2,
                    5..=7 => 1,
                    _ => 0,
                };
                for i in 0..emits {
                    let line = live_line(n * 4 + i as u64);
                    append_rx(rows_t, seq_t, &line);
                }
            },
        );
        std::mem::forget(timer);
    }

    // --- layout ------------------------------------------------------------
    rsx! {
        DockPanel {
            last_child_fill: true,

            // status bar
            Border {
                DockPanel.dock: Dock::Bottom,
                border_thickness: (0.0, 1.0, 0.0, 0.0),
                border_brush: Color::from_argb(0x0F, 0xFF, 0xFF, 0xFF),
                padding: (SPACE_2, 3.0, SPACE_2, 3.0),
                DockPanel {
                    last_child_fill: true,
                    TextBlock {
                        DockPanel.dock: Dock::Right,
                        text: "quick-send values edit in place · every control is mock",
                        font_size: 11.0,
                        opacity: 0.6,
                        vertical_alignment: VerticalAlignment::Center,
                    }
                    TextBlock {
                        text: status,
                        font_family: mono(),
                        font_size: 11.0,
                        opacity: 0.8,
                        vertical_alignment: VerticalAlignment::Center,
                    }
                }
            }

            // send bar
            Border {
                DockPanel.dock: Dock::Bottom,
                border_thickness: (0.0, 1.0, 0.0, 0.0),
                border_brush: Color::from_argb(0x0F, 0xFF, 0xFF, 0xFF),
                padding: (SPACE_2, SPACE_2, SPACE_2, SPACE_2),
                Grid {
                    column_definitions: "Auto,*,Auto,Auto",
                    CheckBox {
                        Grid.column: 0,
                        content: "HEX",
                        is_checked: hex_send.two_way(),
                        margin: (0.0, 0.0, SPACE_2, 0.0),
                        vertical_alignment: VerticalAlignment::Center,
                    }
                    TextBox {
                        Grid.column: 1,
                        text: draft.two_way(),
                        placeholder_text: "text, or hex pairs when HEX is checked (e.g. 01 A0 FF) — Enter sends",
                        font_family: mono(),
                        font_size: 13.0,
                        on_key_down: move |e| {
                            if e.key == Key::Return {
                                e.set_handled(true);
                                do_send(rows, seq, &draft.get(), hex_send.get(), hook_on.get());
                            }
                        },
                    }
                    CheckBox {
                        Grid.column: 2,
                        content: "pre-send hook",
                        is_checked: hook_on.two_way(),
                        margin: (SPACE_2, 0.0, SPACE_2, 0.0),
                        vertical_alignment: VerticalAlignment::Center,
                    }
                    Button {
                        Grid.column: 3,
                        content: "Send",
                        min_width: 88.0,
                        on_click: move |_| {
                            do_send(rows, seq, &draft.get(), hex_send.get(), hook_on.get());
                        },
                    }
                }
            }

            // main area: log pane + quick-send
            Grid {
                column_definitions: "*,300",
                // log pane
                Grid {
                    Grid.column: 0,
                    row_definitions: "Auto,*,Auto",
                    margin: (SPACE_2, SPACE_2, 0.0, SPACE_2),
                    // log toolbar
                    StackPanel {
                        Grid.row: 0,
                        orientation: Orientation::Horizontal,
                        spacing: SPACE_2,
                        margin: (0.0, 0.0, 0.0, SPACE_2),
                        CheckBox {
                            content: "follow",
                            is_checked: follow.two_way(),
                            vertical_alignment: VerticalAlignment::Center,
                        }
                        ComboBox {
                            min_width: 120.0,
                            selected_index: view_mode.two_way(),
                            vertical_alignment: VerticalAlignment::Center,
                            ComboBoxItem { content: "text + hex" }
                            ComboBoxItem { content: "hex only" }
                            ComboBoxItem { content: "text only" }
                        }
                        TextBox {
                            text: filter.two_way(),
                            placeholder_text: "filter (substring)",
                            font_family: mono(),
                            font_size: 12.0,
                            min_width: 220.0,
                            vertical_alignment: VerticalAlignment::Center,
                        }
                        Button {
                            content: "Clear",
                            padding: (10.0, 2.0, 10.0, 2.0),
                            vertical_alignment: VerticalAlignment::Center,
                            on_click: move |_| {
                                rows.update(|v| v.clear());
                            },
                        }
                    }
                    // the log
                    ScrollViewer {
                        Grid.row: 1,
                        node_ref: scroller,
                        vertical_scroll_bar_visibility: ScrollBarVisibility::Auto,
                        // scrolling up pauses the follow (the llcom
                        // muscle-memory path); re-engage via the checkbox.
                        on_scroll_changed: move |e| {
                            if e.offset_delta().y < -0.5 && follow.get() {
                                follow.set(false);
                            }
                        },
                        StackPanel {
                            { log_rows(filtered, view_mode) }
                        }
                    }
                }
                // quick-send panel
                Border {
                    Grid.column: 1,
                    border_thickness: (1.0, 0.0, 0.0, 0.0),
                    border_brush: Color::from_argb(0x0F, 0xFF, 0xFF, 0xFF),
                    padding: (SPACE_2, SPACE_2, SPACE_2, SPACE_2),
                    margin: (0.0, SPACE_2, SPACE_2, SPACE_2),
                    DockPanel {
                        last_child_fill: true,
                        // header: title + hook badge + pager + page label
                        StackPanel {
                            DockPanel.dock: Dock::Top,
                            orientation: Orientation::Vertical,
                            spacing: SPACE_2,
                            margin: (0.0, 0.0, 0.0, SPACE_2),
                            DockPanel {
                                last_child_fill: true,
                                Border {
                                    DockPanel.dock: Dock::Right,
                                    // the TX-hook indicator: lit while the
                                    // pre-send hook is active.
                                    background: badge_bg,
                                    corner_radius: 3.0,
                                    padding: (6.0, 1.0, 6.0, 1.0),
                                    vertical_alignment: VerticalAlignment::Center,
                                    TextBlock {
                                        text: badge_text,
                                        font_family: mono(),
                                        font_size: 10.0,
                                        foreground: badge_fg,
                                    }
                                }
                                TextBlock {
                                    text: "Quick send",
                                    font_size: 14.0,
                                    font_weight: FontWeight::SemiBold,
                                    vertical_alignment: VerticalAlignment::Center,
                                }
                            }
                            DockPanel {
                                last_child_fill: true,
                                TextBlock {
                                    DockPanel.dock: Dock::Right,
                                    text: page_label,
                                    font_family: mono(),
                                    font_size: 11.0,
                                    opacity: 0.7,
                                    vertical_alignment: VerticalAlignment::Center,
                                }
                                PipsPager {
                                    number_of_pages: QUICK_SEND_PAGES as i32,
                                    max_visible_pips: QUICK_SEND_PAGES as i32,
                                    selected_page_index: page.two_way(),
                                    orientation: Orientation::Horizontal,
                                    horizontal_alignment: HorizontalAlignment::Left,
                                }
                            }
                        }
                        // entries of the selected page, editable in place
                        StackPanel {
                            spacing: 4.0,
                            { quick_send_rows(page, entries, rows, seq, hook_on) }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Keyed children (the two `for_each` lists)
// ---------------------------------------------------------------------------

/// The log rows as keyed children. The view mode is folded into the item
/// (and the key): `for_each` keeps a child whose key AND value stay, so a
/// mode flip has to re-key every row to get the mounted rows re-rendered —
/// reading the mode inside the view closure alone would leave them stale.
/// Key layout: `seq * 4 + mode` (mode is 0..=2).
fn log_rows(filtered: Memo<Vec<LogRow>>, mode: Signal<i32>) -> Element {
    for_each(
        move || {
            let m = mode.get() as usize;
            filtered.get().into_iter().map(|row| (m, row)).collect::<Vec<_>>()
        },
        |item| (item.1.seq as usize) * 4 + item.0,
        |item| common::log_row_view(item.1, item.0),
    )
}

/// The six quick-send entries of the selected page as keyed children. Each
/// entry owns its value signal, so an edit-in-place survives page switches
/// (the signal handles are the stable part; the key is the entry index).
fn quick_send_rows(
    page: Signal<i32>,
    entries: Vec<(&'static str, Signal<String>)>,
    rows: Signal<Vec<LogRow>>,
    seq: Signal<u64>,
    hook_on: Signal<bool>,
) -> Element {
    for_each(
        move || {
            let p = page.get() as usize;
            entries_page(&entries, p)
        },
        |entry| entry.0,
        move |entry| quick_send_entry(entry, rows, seq, hook_on),
    )
}

// ---------------------------------------------------------------------------
// Send path (mock): the shape the real session's `write()` has — post-hook
// bytes land in the TX row (contract §2.2 `TxReceipt`, §6 AC 3).
// ---------------------------------------------------------------------------

/// The mock pre-send hook: appends `;CHK=xx` (XOR checksum) before any
/// trailing CRLF, the way a llcom hook rewrites TX.
fn apply_hook(bytes: &[u8]) -> Vec<u8> {
    let xor = bytes.iter().fold(0u8, |acc, b| acc ^ b);
    let mut out = bytes.to_vec();
    let chk = format!(";CHK={xor:02X}").into_bytes();
    let at = if out.ends_with(b"\r\n") { out.len() - 2 } else { out.len() };
    let tail = out.split_off(at);
    out.extend_from_slice(&chk);
    out.extend_from_slice(&tail);
    out
}

fn do_send(rows: Signal<Vec<LogRow>>, seq: Signal<u64>, raw: &str, hex_mode: bool, hooked: bool) {
    if raw.is_empty() {
        return;
    }
    let bytes: Vec<u8> = if hex_mode {
        let parsed: Result<Vec<u8>, _> = raw.split_whitespace().map(|t| u8::from_str_radix(t, 16)).collect();
        match parsed {
            Ok(b) if !b.is_empty() => b,
            _ => {
                append_note(rows, seq, &format!("invalid hex payload: {raw:?}"));
                return;
            }
        }
    } else {
        raw.as_bytes().to_vec()
    };
    let (bytes, hooked) = if hooked { (apply_hook(&bytes), true) } else { (bytes, false) };
    let time = now_stamp();
    let s = seq.get_untracked() + 1;
    seq.set(s);
    rows.update(|v| v.push(common::sent_row(s, &time, &bytes, hooked)));
}

fn append_rx(rows: Signal<Vec<LogRow>>, seq: Signal<u64>, text: &str) {
    let time = now_stamp();
    let s = seq.get_untracked() + 1;
    seq.set(s);
    rows.update(|v| v.push(LogRow::new(s, &time, Direction::Rx, &common::to_hex(text.as_bytes()), text)));
}

fn append_note(rows: Signal<Vec<LogRow>>, seq: Signal<u64>, text: &str) {
    let time = now_stamp();
    let s = seq.get_untracked() + 1;
    seq.set(s);
    rows.update(|v| v.push(common::note_row(s, &time, text)));
}

/// Local wall-clock stamp (the real session's `LogEvent.at`, rendered
/// local — contract §1.3).
fn now_stamp() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Substring match for the filter: text always, hex always (a hex needle
/// with spaces matches the hex view, a text needle the decoded line).
fn matches(row: &LogRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    row.text.to_lowercase().contains(needle) || row.hex.to_lowercase().contains(needle)
}

/// The live simulator's lines: a slow sensor frame cycling by tick.
fn live_line(n: u64) -> String {
    match n % 6 {
        0 => format!(r#"{{"t":24.{}}}"#, 0 + (n % 10)),
        1 => format!("T=2{}.{}", 1 + n % 5, n % 10),
        2 => format!("P2={}", 5600 + n),
        3 => format!("H={}", 40 + n % 20),
        4 => "OK".to_string(),
        _ => format!("T=2{}.{}", 1 + n % 5, n % 10),
    }
}

// ---------------------------------------------------------------------------
// Quick-send panel
// ---------------------------------------------------------------------------

type QsEntry = (usize, &'static str, Signal<String>);

/// The 10 pages × 6 entries (label, initial value). Page 1 is hand-written;
/// the rest are generated device-command patterns.
fn quick_send_pages() -> Vec<Vec<(&'static str, &'static str)>> {
    let page1: Vec<(&'static str, &'static str)> = vec![
        ("ping", "ping\r\n"),
        ("poll", "poll\r\n"),
        ("read temp", "{\"get\":\"temp\"}"),
        ("reset", "RST\r\n"),
        ("version", "AT+VER?\r\n"),
        ("scan", "SCAN\r\n"),
    ];
    let mut pages = vec![page1];
    for p in 1..QUICK_SEND_PAGES {
        let mut page = Vec::new();
        for i in 0..ENTRIES_PER_PAGE {
            let label: &'static str = Box::leak(format!("p{} cmd {}", p + 1, i + 1).into_boxed_str());
            let value: &'static str = match i % 3 {
                0 => Box::leak(format!("AT+P{p}={i}\r\n").into_boxed_str()),
                1 => Box::leak(format!("{{\"cmd\":\"read\",\"ch\":{}}}", p * 10 + i).into_boxed_str()),
                _ => Box::leak(format!("01 {p:02X} {i:02X}").into_boxed_str()),
            };
            page.push((label, value));
        }
        pages.push(page);
    }
    pages
}

fn entries_page(entries: &[(&'static str, Signal<String>)], page: usize) -> Vec<QsEntry> {
    let start = page * ENTRIES_PER_PAGE;
    let end = (start + ENTRIES_PER_PAGE).min(entries.len());
    entries[start..end].iter().enumerate().map(|(i, (label, sig))| (start + i, *label, *sig)).collect()
}

/// One quick-send row: label | editable value | send. Editing is in place —
/// the TextBox is the entry (no dialog), values survive page switches.
fn quick_send_entry(entry: QsEntry, rows: Signal<Vec<LogRow>>, seq: Signal<u64>, hook_on: Signal<bool>) -> Element {
    let (idx, label, value) = entry;
    rsx! {
        Grid {
            column_definitions: "92,*,Auto",
            margin: (0.0, 0.0, 0.0, 2.0),
            TextBlock {
                Grid.column: 0,
                text: label,
                font_size: 11.0,
                foreground: Color::from_argb(0xC5, 0xFF, 0xFF, 0xFF),
                margin: (0.0, 0.0, SPACE_2, 0.0),
                vertical_alignment: VerticalAlignment::Center,
            }
            TextBox {
                Grid.column: 1,
                text: value.two_way(),
                font_family: mono(),
                font_size: 12.0,
                on_key_down: move |e| {
                    if e.key == Key::Return {
                        e.set_handled(true);
                        do_send(rows, seq, &value.get(), false, hook_on.get());
                    }
                },
            }
            Button {
                Grid.column: 2,
                content: format!("send {idx:02}"),
                font_size: 11.0,
                padding: (8.0, 2.0, 8.0, 2.0),
                margin: (SPACE_2, 0.0, 0.0, 0.0),
                on_click: move |_| {
                    do_send(rows, seq, &value.get(), false, hook_on.get());
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Headless verification (--selfcheck): mounts the log pane in all three
// view modes without a window and asserts the mock rows and mode behaviour.
// ---------------------------------------------------------------------------

fn selfcheck() -> i32 {
    let mut ok = true;

    // 1. the default "both" view: rows, dual view, badges, quick-send.
    let both = mount_and_walk(0, "");
    println!("selfcheck [both]: {} timestamped lines, {} texts", both.timestamps, both.texts.len());
    ok &= both.timestamps >= 150;
    for needle in SELFCHECK_BOTH {
        let found = both.texts.iter().any(|t| t.contains(needle));
        println!("selfcheck [both]: needle {needle:?} -> {}", verdict(found));
        ok &= found;
    }

    // 2. hex-only mode: hex lines render, decoded text lines do not.
    let hex_only = mount_and_walk(1, "");
    let hex_there = hex_only.texts.iter().any(|t| t.contains("3C 53 59 53 3E"));
    let text_gone = !hex_only.texts.iter().any(|t| t.contains("<SYS>"));
    println!("selfcheck [hex]: hex line -> {}, text line hidden -> {}", verdict(hex_there), verdict(text_gone));
    ok &= hex_there && text_gone;

    // 3. text-only mode: the mirror image.
    let text_only = mount_and_walk(2, "");
    let text_there = text_only.texts.iter().any(|t| t.contains("<SYS>"));
    let hex_gone = !text_only.texts.iter().any(|t| t.contains("3C 53 59 53 3E"));
    println!("selfcheck [text]: text line -> {}, hex line hidden -> {}", verdict(text_there), verdict(hex_gone));
    ok &= text_there && hex_gone;

    // 4. the filter: rows shrink to the matches.
    let filtered = mount_and_walk(0, "P1=");
    let filter_ok = filtered.timestamps > 0 && filtered.timestamps < both.timestamps;
    println!("selfcheck [filter]: \"P1=\" shows {} of {} lines -> {}", filtered.timestamps, both.timestamps, verdict(filter_ok));
    ok &= filter_ok;

    println!("selfcheck [log_prototype]: {}", if ok { "OK" } else { "FAILED" });
    if ok { 0 } else { 1 }
}

fn verdict(found: bool) -> &'static str {
    if found {
        "found"
    } else {
        "MISSING"
    }
}

struct Walk {
    texts: Vec<String>,
    timestamps: usize,
}

fn mount_and_walk(mode: i32, filter: &str) -> Walk {
    let init = common::mock_rows(INITIAL_ROWS);
    let needle = filter.to_string();
    walk_one(move || content(init, false, mode, &needle))
}

fn walk_one(build: impl FnOnce() -> Element) -> Walk {
    let (root, owner) = build().mount_root();
    let mut texts: Vec<String> = Vec::new();
    walk_logical(&root.upcast::<StyledElement>(), &mut texts);
    owner.dispose();
    let timestamps = texts.iter().filter(|t| t.starts_with("12:")).count();
    Walk { texts, timestamps }
}

/// Collects the text of every `TextBlock` in the logical tree.
fn walk_logical(element: &Ref<StyledElement>, texts: &mut Vec<String>) {
    if let Some(text_block) = element.cast::<TextBlock>() {
        if let Some(text) = text_block.text() {
            texts.push(text);
        }
    }
    for child in element.logical_children().to_vec() {
        walk_logical(&child, texts);
    }
}
