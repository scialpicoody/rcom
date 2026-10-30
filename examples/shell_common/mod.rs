//! The mock content the three shell candidates (#28) share: throwaway inline
//! data and widget helpers, no real core behind them.
//!
//! Every shell renders the same four pieces, arranged its own way: the session
//! log (~50 rows, mixed RX/TX, wrapping and non-ASCII rows), the send bar, the
//! quick-send panel (6 entries + a 1/10 page indicator), the port toolbar, and
//! the bottom pane (Script / Tools / Plot tab placeholders).
//!
//! Every control, property and builder named here was checked against the
//! pinned sources (`ferroui-*-0.4.0-preview.1` in the registry cache).

use ferroui_base::layout::{HorizontalAlignment, Orientation, VerticalAlignment};
use ferroui_base::media::{Color, FontFamily, TextWrapping};
use ferroui_base::{Ref, StyledElement};
use ferroui_controls::{Dock, TextBlock};
use ferroui_view::prelude::*;

/// The fonts of the log and the send box: a fallback chain that lands on a
/// monospace face on Windows, macOS and Linux (the comma-separated name list
/// is the form `FontFamily::parse` takes; see the font manager tests of
/// `ferroui-base`).
pub fn monospace() -> FontFamily {
    FontFamily::parse("Cascadia Mono,Consolas,Menlo,DejaVu Sans Mono").expect("monospace font family")
}

/// The direction of a log row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Received from the port.
    Rx,
    /// Sent to the port.
    Tx,
    /// A session note (port opened, encoding changed, ...).
    Info,
}

/// One row of the session log: the timestamp, the bytes as HEX, the decoded
/// text, and whether the line is long enough to need wrapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRow {
    pub seq: u64,
    pub time: String,
    pub dir: Direction,
    pub hex: String,
    pub text: String,
    pub wrap: bool,
}

impl LogRow {
    fn new(seq: u64, time: &str, dir: Direction, hex: &str, text: &str) -> Self {
        Self { seq, time: time.to_string(), dir, hex: hex.to_string(), text: text.to_string(), wrap: false }
    }

    /// A deliberately long row (a firmware dump line) that has to wrap.
    fn wrapped(seq: u64, time: &str, dir: Direction, hex: String, text: String) -> Self {
        Self { seq, time: time.to_string(), dir, hex, text, wrap: true }
    }
}

/// The colour of a row's direction marker.
fn direction_color(dir: Direction) -> Color {
    match dir {
        Direction::Rx => Color::from_rgb(0x16, 0x7e, 0x3d),
        Direction::Tx => Color::from_rgb(0x0b, 0x57, 0xd0),
        Direction::Info => Color::from_rgb(0x8a, 0x53, 0x00),
    }
}

fn direction_label(dir: Direction) -> &'static str {
    match dir {
        Direction::Rx => "RX",
        Direction::Tx => "TX",
        Direction::Info => "--",
    }
}

/// The session log every shell shows: ~50 rows of a fictional sensor board on
/// COM3, mixed directions, a few long lines, a few non-ASCII rows (one GBK,
/// two UTF-8).
pub fn mock_rows() -> Vec<LogRow> {
    let mut rows: Vec<LogRow> = Vec::new();
    let mut seq = 0u64;
    fn push(rows: &mut Vec<LogRow>, seq: &mut u64, time: &str, dir: Direction, hex: &str, text: &str) {
        *seq += 1;
        rows.push(LogRow::new(*seq, time, dir, hex, text));
    }

    push(&mut rows, &mut seq, "12:00:01.104", Direction::Info, "", "port COM3 opened (115200 8N1, UTF-8)");
    push(&mut rows, &mut seq, "12:00:01.318", Direction::Rx, "3C 53 59 53 3E", "<SYS>");
    push(&mut rows, &mut seq, "12:00:01.319", Direction::Rx, "56 65 72 3D 31 2E 34 2E 32 0D 0A", "Ver=1.4.2");
    push(&mut rows, &mut seq, "12:00:01.402", Direction::Tx, "3C 49 44 3E 0D 0A", "<ID>");
    push(&mut rows, &mut seq, "12:00:01.487", Direction::Rx, "49 44 3D 53 4E 39 32 37 34 31", "ID=SN92741");
    push(&mut rows, &mut seq, "12:00:02.010", Direction::Rx, "7B 22 74 22 3A 32 35 2E 33 7D", "{\"t\":25.3}");
    push(&mut rows, &mut seq, "12:00:02.105", Direction::Tx, "41 54 2B 56 45 52 3F 0D", "AT+VER?");
    push(&mut rows, &mut seq, "12:00:02.230", Direction::Rx, "4F 4B 20 31 2E 34 2E 32", "OK 1.4.2");
    push(&mut rows, &mut seq, "12:00:03.880", Direction::Rx, "C6 B7 C5 C6 A3 BA B2 E2 CA D4", "品牌：测试 (GBK)");
    push(&mut rows, &mut seq, "12:00:04.004", Direction::Info, "", "encoding switched to GB18030");
    push(&mut rows, &mut seq, "12:00:04.101", Direction::Rx, "CE C2 B6 C8 A3 BA 32 35 2E 33", "温度：25.3");
    // A firmware-dump-length line, forced to wrap.
    seq += 1;
    rows.push(LogRow::wrapped(
        seq,
        "12:00:05.512",
        Direction::Rx,
        "A5 5A EB 90 00 FF 20 01 44 33 22 11 9A 4C 3E 08 77 41 C2 55 A5 5A EB 90 00 FF 20 01 44 33 22 11 9A 4C 3E 08 77 41 C2 55 A5 5A EB 90 00 FF 20 01 44 33 22 11 9A 4C 3E 08 77 41 C2 55".to_string(),
        "……binary telemetry burst, 60 bytes, decoded lossy, one very long line that has to wrap to stay readable……".to_string(),
    ));
    push(&mut rows, &mut seq, "12:00:06.000", Direction::Tx, "52 53 54 0D 0A", "RST");
    push(&mut rows, &mut seq, "12:00:06.318", Direction::Rx, "3C 53 59 53 3E", "<SYS>");
    push(&mut rows, &mut seq, "12:00:07.750", Direction::Rx, "48 65 6C 6C 6F 20 77 6F 72 6C 64", "Hello world");
    push(&mut rows, &mut seq, "12:00:08.012", Direction::Tx, "53 43 41 4E 0D", "SCAN");
    push(&mut rows, &mut seq, "12:00:08.344", Direction::Rx, "57 69 46 69 3A 20 36 20 6E 65 74 73", "WiFi: 6 nets");
    push(&mut rows, &mut seq, "12:00:09.090", Direction::Rx, "E7 8A B6 E6 80 81 3A 20 4F 4B", "状态: OK (UTF-8)");
    push(&mut rows, &mut seq, "12:00:10.222", Direction::Tx, "43 46 47 3F 0D 0A", "CFG?");
    push(&mut rows, &mut seq, "12:00:10.388", Direction::Rx, "6D 6F 64 65 3D 73 6C 61 76 65", "mode=slave");
    push(&mut rows, &mut seq, "12:00:11.512", Direction::Rx, "00 01 02 03 04 05 06 07", "........");
    push(&mut rows, &mut seq, "12:00:12.900", Direction::Info, "", "script `poll.lua` started");
    push(&mut rows, &mut seq, "12:00:13.001", Direction::Tx, "70 6F 6C 6C 0D", "poll");
    push(&mut rows, &mut seq, "12:00:13.318", Direction::Rx, "50 31 3D 31 32 33 34", "P1=1234");
    push(&mut rows, &mut seq, "12:00:14.318", Direction::Rx, "50 32 3D 35 36 37 38", "P2=5678");
    push(&mut rows, &mut seq, "12:00:15.001", Direction::Tx, "70 6F 6C 6C 0D", "poll");
    push(&mut rows, &mut seq, "12:00:15.319", Direction::Rx, "50 31 3D 31 32 33 35", "P1=1235");
    // Another long line.
    seq += 1;
    rows.push(LogRow::wrapped(
        seq,
        "12:00:16.664",
        Direction::Rx,
        "45 72 72 6F 72 3A 20 73 65 6E 73 6F 72 20 32 20 74 69 6D 65 6F 75 74 20 61 66 74 65 72 20 35 30 30 30 6D 73 2C 20 72 65 74 72 79 69 6E 67 20 77 69 74 68 20 62 61 63 6B 6F 66 66 20 32 35 30 6D 73".to_string(),
        "Error: sensor 2 timeout after 5000ms, retrying with backoff 250ms and jitter, giving up after three attempts, please check the wiring of sensor 2 on the bus".to_string(),
    ));
    push(&mut rows, &mut seq, "12:00:17.100", Direction::Tx, "70 6F 6C 6C 0D", "poll");
    push(&mut rows, &mut seq, "12:00:17.402", Direction::Rx, "50 32 3D 35 36 39 30", "P2=5690");
    push(&mut rows, &mut seq, "12:00:18.888", Direction::Rx, "E5 BF 83 E8 B7 B3 EF BC 9A 31", "心跳：1 (UTF-8)");
    push(&mut rows, &mut seq, "12:00:20.050", Direction::Info, "", "plot tap `t` → curve A");
    push(&mut rows, &mut seq, "12:00:21.318", Direction::Rx, "7B 22 68 22 3A 36 30 7D", "{\"h\":60}");
    push(&mut rows, &mut seq, "12:00:22.002", Direction::Tx, "70 6F 6C 6C 0D", "poll");
    push(&mut rows, &mut seq, "12:00:22.337", Direction::Rx, "50 31 3D 31 32 34 30", "P1=1240");
    push(&mut rows, &mut seq, "12:00:23.511", Direction::Rx, "4F 4B", "OK");
    push(&mut rows, &mut seq, "12:00:24.000", Direction::Tx, "53 41 56 45 0D", "SAVE");
    push(&mut rows, &mut seq, "12:00:24.290", Direction::Rx, "43 6F 6E 66 69 67 20 73 61 76 65 64", "Config saved");
    push(&mut rows, &mut seq, "12:00:25.670", Direction::Rx, "C9 E8 B1 B8 D2 D1 B1 A3 B4 E6", "设备已保存 (GBK)");
    push(&mut rows, &mut seq, "12:00:26.318", Direction::Rx, "3C 2F 53 59 53 3E", "</SYS>");
    push(&mut rows, &mut seq, "12:00:27.777", Direction::Info, "", "log file rotated: rcom-com3-120001.log");
    push(&mut rows, &mut seq, "12:00:28.318", Direction::Rx, "7B 22 74 22 3A 32 35 2E 31 7D", "{\"t\":25.1}");
    push(&mut rows, &mut seq, "12:00:29.001", Direction::Tx, "70 6F 6C 6C 0D", "poll");
    push(&mut rows, &mut seq, "12:00:29.318", Direction::Rx, "50 31 3D 31 32 34 31", "P1=1241");
    push(&mut rows, &mut seq, "12:00:30.552", Direction::Rx, "50 32 3D 35 36 39 33", "P2=5693");
    push(&mut rows, &mut seq, "12:00:31.318", Direction::Rx, "FF EE 01", "...");
    push(&mut rows, &mut seq, "12:00:32.004", Direction::Tx, "43 4C 53 0D", "CLS");
    push(&mut rows, &mut seq, "12:00:33.910", Direction::Info, "", "auto-scroll paused by filter (2 hidden)");
    rows
}

/// A TX row the send bar appends when its button is clicked (the same shape
/// the real session would log).
fn sent_row(seq: u64, time: &str, text: &str) -> LogRow {
    let hex: Vec<String> = text.bytes().map(|b| format!("{b:02X}")).collect();
    LogRow::new(seq, time, Direction::Tx, &hex.join(" "), text)
}

/// An Info row for the shells to log their own mock events with (only
/// shell B uses it today; the other shells share this module).
#[allow(dead_code)]
pub fn info_row(seq: u64, time: &str, text: &str) -> LogRow {
    LogRow::new(seq, time, Direction::Info, "", text)
}

/// The "clock" of the mock: a monotonically advancing fake timestamp
/// (interior-mutable, so event handlers — `Fn` closures — can tick it).
struct MockClock {
    ms: std::cell::Cell<u64>,
}

impl MockClock {
    fn new() -> Self {
        Self { ms: std::cell::Cell::new(34_000) }
    }

    fn tick(&self) -> String {
        self.ms.set(self.ms.get() + 137);
        let whole = self.ms.get() / 1000;
        let frac = self.ms.get() % 1000;
        format!("12:{:02}:{:02}.{:03}", whole / 60, whole % 60, frac)
    }
}

/// One log row as a control: a grid line — the direction marker, then the
/// HEX line and the decoded text (monospace; long rows wrap).
pub fn log_row_view(row: LogRow) -> Element {
    rsx! {
        Grid {
            column_definitions: "36,*",
            margin: (0.0, 1.0, 0.0, 1.0),
            TextBlock {
                Grid.column: 0,
                text: direction_label(row.dir),
                font_family: monospace(),
                font_size: 11.0,
                foreground: direction_color(row.dir),
                vertical_alignment: VerticalAlignment::Top,
            }
            if row.hex.is_empty() {
                TextBlock {
                    Grid.column: 1,
                    text: row.text,
                    text_wrapping: TextWrapping::Wrap,
                    opacity: 0.7,
                }
            } else {
                StackPanel {
                    Grid.column: 1,
                    spacing: 0.0,
                    TextBlock {
                        text: format!("{}  {}", row.time, row.hex),
                        text_wrapping: if row.wrap { TextWrapping::Wrap } else { TextWrapping::NoWrap },
                        font_family: monospace(),
                        font_size: 12.0,
                    }
                    TextBlock {
                        text: row.text,
                        text_wrapping: TextWrapping::Wrap,
                        font_family: monospace(),
                        font_size: 12.0,
                    }
                }
            }
        }
    }
}

/// The log view: the rows as a list. The rows follow `rows`; Send appends.
pub fn log_view(rows: Signal<Vec<LogRow>>) -> Element {
    for_each(
        move || rows.get(),
        |row| row.seq as usize,
        |row| log_row_view(row),
    )
}

/// The send bar: a text box (monospace) and the send button. Clicking send
/// appends a TX row to `rows`, as the real session would.
pub fn send_bar(rows: Signal<Vec<LogRow>>, draft: Signal<String>) -> Element {
    let seq = signal(rows.get().len() as u64 + 1000);
    let clock = MockClock::new();
    rsx! {
        DockPanel {
            last_child_fill: true,
            margin: (8.0, 8.0, 8.0, 8.0),
            Button {
                DockPanel.dock: Dock::Right,
                content: "Send",
                margin: (8.0, 0.0, 0.0, 0.0),
                padding: (16.0, 6.0, 16.0, 6.0),
                vertical_alignment: VerticalAlignment::Stretch,
                on_click: move |_| {
                    let text = draft.get();
                    if text.is_empty() {
                        return;
                    }
                    seq.update(|s| *s += 1);
                    let time = clock.tick();
                    let s = seq.get();
                    rows.update(|v| v.push(sent_row(s, &time, &text)));
                },
            }
            TextBox {
                text: draft.two_way(),
                placeholder_text: "text or hex (0F 0A) — mock: Send appends a TX row below",
                font_family: monospace(),
                vertical_alignment: VerticalAlignment::Center,
            }
        }
    }
}

/// The quick-send panel: the six page-1 entries and the 1/10 page indicator.
/// Clicking an entry puts its value into the send draft.
pub fn quick_send_panel(draft: Signal<String>) -> Element {
    let entries = quick_send_entries();
    rsx! {
        StackPanel {
            spacing: 4.0,
            margin: (8.0, 8.0, 8.0, 8.0),
            DockPanel {
                last_child_fill: true,
                TextBlock {
                    DockPanel.dock: Dock::Right,
                    text: "1/10",
                    font_family: monospace(),
                    font_size: 11.0,
                    opacity: 0.7,
                    vertical_alignment: VerticalAlignment::Center,
                }
                TextBlock {
                    text: "Quick send",
                    font_size: 13.0,
                }
            }
            PipsPager {
                number_of_pages: 10,
                selected_page_index: 0,
                orientation: Orientation::Horizontal,
                horizontal_alignment: HorizontalAlignment::Left,
            }
            for entry in entries {
                Button {
                    content: format!("{}  ▸  {:?}", entry.0, entry.1),
                    horizontal_alignment: HorizontalAlignment::Stretch,
                    horizontal_content_alignment: HorizontalAlignment::Left,
                    padding: (8.0, 4.0, 8.0, 4.0),
                    on_click: move |_| draft.set(entry.1.to_string()),
                }
            }
        }
    }
}

/// The quick-send entries of page 1 (llcom-style: a label, a value).
pub fn quick_send_entries() -> [(&'static str, &'static str); 6] {
    [
        ("ping", "ping\r\n"),
        ("poll", "poll\r\n"),
        ("read temp", "{\"get\":\"temp\"}"),
        ("reset", "RST\r\n"),
        ("ver", "AT+VER?\r\n"),
        ("scan", "SCAN\r\n"),
    ]
}

/// The port toolbar the mock sessions share: port, baud, encoding, hex mode,
/// and the open state. Buttons only log an Info row.
pub fn port_bar(rows: Signal<Vec<LogRow>>) -> Element {
    let seq = signal(rows.get().len() as u64 + 2000);
    let clock = MockClock::new();
    rsx! {
        DockPanel {
            last_child_fill: true,
            margin: (8.0, 0.0, 8.0, 0.0),
            ToggleSwitch {
                DockPanel.dock: Dock::Right,
                is_checked: true,
                on_content: "Open",
                off_content: "Closed",
                margin: (8.0, 0.0, 0.0, 0.0),
                on_click: move |_| {
                    seq.update(|s| *s += 1);
                    let time = clock.tick();
                    let s = seq.get();
                    rows.update(|v| v.push(LogRow::new(s, &time, Direction::Info, "", "port toggle (mock)")));
                },
            }
            CheckBox {
                DockPanel.dock: Dock::Right,
                content: "HEX",
                vertical_alignment: VerticalAlignment::Center,
                margin: (8.0, 0.0, 0.0, 0.0),
            }
            StackPanel {
                orientation: Orientation::Horizontal,
                spacing: 8.0,
                vertical_alignment: VerticalAlignment::Center,
                ComboBox {
                    min_width: 140.0,
                    placeholder_text: "COM3 — USB Serial",
                    ComboBoxItem { content: "COM3 — USB Serial" }
                    ComboBoxItem { content: "COM7 — IoT Board" }
                    ComboBoxItem { content: "/dev/tty.usbserial-1420" }
                }
                ComboBox {
                    min_width: 90.0,
                    placeholder_text: "115200",
                    ComboBoxItem { content: "9600" }
                    ComboBoxItem { content: "115200" }
                    ComboBoxItem { content: "921600" }
                }
                ComboBox {
                    min_width: 90.0,
                    placeholder_text: "UTF-8",
                    ComboBoxItem { content: "UTF-8" }
                    ComboBoxItem { content: "GB18030" }
                    ComboBoxItem { content: "Big5" }
                }
            }
        }
    }
}

/// The bottom pane hint: Script / Tools / Plot as tab placeholders with a
/// stub page each.
pub fn bottom_pane() -> Element {
    rsx! {
        TabControl {
            height: 140.0,
            margin: (8.0, 0.0, 8.0, 8.0),
            TabItem {
                header: "Script",
                TextBlock {
                    text: "— script editor placeholder (a monospaced TextBox until the editor lands) —",
                    opacity: 0.6,
                    margin: (8.0, 8.0, 8.0, 8.0),
                }
            }
            TabItem {
                header: "Tools",
                TextBlock {
                    text: "— encodings / net tools / monitor placeholders —",
                    opacity: 0.6,
                    margin: (8.0, 8.0, 8.0, 8.0),
                }
            }
            TabItem {
                header: "Plot",
                TextBlock {
                    text: "— plot placeholder (custom render control at the next FerroUI release) —",
                    opacity: 0.6,
                    margin: (8.0, 8.0, 8.0, 8.0),
                }
            }
        }
    }
}

/// Headless verification for the `--selfcheck` flag of the examples: mounts
/// the content element without a window (the machine this was authored on
/// cannot open FerroUI windows at all — see SHELL_CANDIDATES.md — so the
/// built tree is checked instead of a screenshot), walks the logical tree and
/// reports the log rows and the needle texts it finds.
///
/// Exits 0 when every needle is present and enough log rows rendered.
pub fn selfcheck(name: &'static str, needles: &[&str], view: impl FnOnce() -> Element) -> i32 {
    let (root, owner) = view().mount_root();
    let mut texts: Vec<String> = Vec::new();
    walk_logical(&root.upcast::<StyledElement>(), &mut texts);
    owner.dispose();

    let with_time = texts.iter().filter(|t| t.starts_with("12:00:")).count();
    println!("selfcheck [{name}]: {with_time} log lines with a timestamp, {} texts total", texts.len());
    let mut ok = with_time >= 40;
    for needle in needles {
        let found = texts.iter().any(|t| t.contains(needle));
        println!("selfcheck [{name}]: needle {:?} -> {}", needle, if found { "found" } else { "MISSING" });
        ok &= found;
    }
    if ok {
        println!("selfcheck [{name}]: OK");
        0
    } else {
        println!("selfcheck [{name}]: FAILED");
        1
    }
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
