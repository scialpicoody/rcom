//! Shell candidate A (#28): sidebar navigation.
//!
//! A `SplitView` rail on the left (sessions, tools, settings entries), the
//! session content on the right: header, port bar, log, send bar,
//! quick-send, and the bottom pane. Styled with the WinUI theme;
//! `FERROUI_SMOKE_EXIT_MS=<n>` closes the window after `n` ms (handled by
//! `launch_themed`).
//!
//! ```text
//! cargo run --example shell_a
//! FERROUI_SMOKE_EXIT_MS=3000 cargo run --example shell_a
//! cargo run --example shell_a -- --selfcheck   # headless tree check
//! ```

#[path = "shell_common/mod.rs"]
mod shell_common;

use ferroui_base::layout::HorizontalAlignment;
use ferroui_controls::primitives::ScrollBarVisibility;
use ferroui_controls::{SplitViewDisplayMode, SplitViewPanePlacement};
use ferroui_themes_winui::{DensityStyle, WinUITheme};
use ferroui_view::prelude::*;
use shell_common::{bottom_pane, log_view, mock_rows, port_bar, quick_send_panel, send_bar, LogRow};

/// The needles the selfcheck looks for among the texts of the tree.
const SELFCHECK_NEEDLES: &[&str] =
    &["rcom", "SESSIONS", "Quick send", "3C 53 59 53 3E", "P1=1234", "plot placeholder", "品牌：测试"];

fn main() {
    let rows = mock_rows();
    if std::env::args().any(|arg| arg == "--selfcheck") {
        std::process::exit(shell_common::selfcheck("shell_a", SELFCHECK_NEEDLES, move || {
            let rows = signal(rows.clone());
            let draft = signal(String::new());
            content(rows, draft)
        }));
    }
    ferroui_view::launch_themed(
        // The WinUI theme, compact density (a tool, not a document app).
        || {
            let theme = WinUITheme::new();
            theme.set_density_style(DensityStyle::Compact);
            theme.as_style()
        },
        move || {
            let rows = signal(rows.clone());
            let draft = signal(String::new());
            rsx! {
                Window {
                    title: "rcom — shell A: sidebar navigation (mock)",
                    width: 1200.0,
                    height: 800.0,
                    { content(rows, draft) }
                }
            }
        },
        |_window| {},
    );
}

/// The window content: the `SplitView` with the navigation pane and the
/// session area (header, port bar, log, quick-send, send bar, bottom pane).
fn content(rows: Signal<Vec<LogRow>>, draft: Signal<String>) -> Element {
    // Attribute values are expressions: the pane is built first and handed
    // to `SplitView` as an element value.
    let pane = rsx! {
        StackPanel {
            spacing: 0.0,
            TextBlock {
                text: "rcom",
                font_size: 20.0,
                margin: (12.0, 12.0, 12.0, 4.0),
            }
            TextBlock {
                text: "serial · lua · tools",
                font_size: 11.0,
                opacity: 0.6,
                margin: (12.0, 0.0, 12.0, 8.0),
            }
            Separator { margin: (0.0, 4.0, 0.0, 4.0) }
            TextBlock {
                text: "SESSIONS",
                font_size: 11.0,
                opacity: 0.6,
                margin: (12.0, 8.0, 12.0, 4.0),
            }
            Button {
                content: "COM3 ● (115200)",
                horizontal_alignment: HorizontalAlignment::Stretch,
                horizontal_content_alignment: HorizontalAlignment::Left,
                padding: (12.0, 6.0, 12.0, 6.0),
            }
            Button {
                content: "COM7 ○",
                horizontal_alignment: HorizontalAlignment::Stretch,
                horizontal_content_alignment: HorizontalAlignment::Left,
                padding: (12.0, 6.0, 12.0, 6.0),
            }
            Button {
                content: "+ new session",
                horizontal_alignment: HorizontalAlignment::Stretch,
                horizontal_content_alignment: HorizontalAlignment::Left,
                padding: (12.0, 6.0, 12.0, 6.0),
            }
            Separator { margin: (0.0, 4.0, 0.0, 4.0) }
            TextBlock {
                text: "WORKSPACE",
                font_size: 11.0,
                opacity: 0.6,
                margin: (12.0, 8.0, 12.0, 4.0),
            }
            for item in ["Scripts", "Tools", "Plot", "Net monitor"] {
                Button {
                    content: item,
                    horizontal_alignment: HorizontalAlignment::Stretch,
                    horizontal_content_alignment: HorizontalAlignment::Left,
                    padding: (12.0, 6.0, 12.0, 6.0),
                }
            }
            Separator { margin: (0.0, 4.0, 0.0, 4.0) }
            Button {
                content: "Settings",
                horizontal_alignment: HorizontalAlignment::Stretch,
                horizontal_content_alignment: HorizontalAlignment::Left,
                padding: (12.0, 6.0, 12.0, 6.0),
            }
        }
    };
    rsx! {
        SplitView {
            display_mode: SplitViewDisplayMode::Inline,
            pane_placement: SplitViewPanePlacement::Left,
            is_pane_open: true,
            open_pane_length: 220.0,
            pane: pane,
            Grid {
                row_definitions: "Auto,Auto,*,Auto,Auto",
                TextBlock {
                    Grid.row: 0,
                    text: "COM3 ● — 115200 8N1 — UTF-8 — log auto-saved",
                    padding: (8.0, 8.0, 8.0, 4.0),
                    opacity: 0.8,
                }
                Grid {
                    Grid.row: 1,
                    { port_bar(rows) }
                }
                Grid {
                    Grid.row: 2,
                    column_definitions: "*,280",
                    ScrollViewer {
                        vertical_scroll_bar_visibility: ScrollBarVisibility::Auto,
                        StackPanel {
                            { log_view(rows) }
                        }
                    }
                    Border {
                        Grid.column: 1,
                        border_thickness: (1.0, 0.0, 0.0, 0.0),
                        { quick_send_panel(draft) }
                    }
                }
                Grid {
                    Grid.row: 3,
                    { send_bar(rows, draft) }
                }
                Grid {
                    Grid.row: 4,
                    { bottom_pane() }
                }
            }
        }
    }
}
