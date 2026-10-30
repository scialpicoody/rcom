//! Shell candidate C (#28): modernized llcom classic.
//!
//! The layout llcom users know, restyled with the WinUI theme: the port
//! toolbar on top, the log on the left with the send bar under it, the
//! quick-send panel in a right column, and the Script / Tools / Plot tab
//! pane across the bottom. `FERROUI_SMOKE_EXIT_MS=<n>` closes the window
//! after `n` ms (handled by `launch_themed`).
//!
//! ```text
//! cargo run --example shell_c
//! FERROUI_SMOKE_EXIT_MS=3000 cargo run --example shell_c
//! cargo run --example shell_c -- --selfcheck   # headless tree check
//! ```

#[path = "shell_common/mod.rs"]
mod shell_common;

use ferroui_controls::primitives::ScrollBarVisibility;
use ferroui_themes_winui::{DensityStyle, WinUITheme};
use ferroui_view::prelude::*;
use shell_common::{bottom_pane, log_view, mock_rows, port_bar, quick_send_panel, send_bar, LogRow};

/// The needles the selfcheck looks for among the texts of the tree.
const SELFCHECK_NEEDLES: &[&str] = &["Quick send", "P2=5678", "script editor placeholder", "设备已保存"];

fn main() {
    let rows = mock_rows();
    if std::env::args().any(|arg| arg == "--selfcheck") {
        std::process::exit(shell_common::selfcheck("shell_c", SELFCHECK_NEEDLES, move || {
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
                    title: "rcom — shell C: llcom classic (mock)",
                    width: 1200.0,
                    height: 800.0,
                    { content(rows, draft) }
                }
            }
        },
        |_window| {},
    );
}

/// The window content: port bar on top, log + send bar on the left,
/// quick-send in the right column, the bottom pane across the window.
fn content(rows: Signal<Vec<LogRow>>, draft: Signal<String>) -> Element {
    rsx! {
        Grid {
            row_definitions: "Auto,*,Auto",
            column_definitions: "*,300",
            Grid {
                Grid.row: 0,
                Grid.column: 0,
                Grid.column_span: 2,
                { port_bar(rows) }
            }
            Grid {
                Grid.row: 1,
                Grid.column: 0,
                row_definitions: "*,Auto",
                ScrollViewer {
                    vertical_scroll_bar_visibility: ScrollBarVisibility::Auto,
                    StackPanel {
                        { log_view(rows) }
                    }
                }
                Grid {
                    Grid.row: 1,
                    { send_bar(rows, draft) }
                }
            }
            Border {
                Grid.row: 1,
                Grid.column: 1,
                border_thickness: (1.0, 0.0, 0.0, 0.0),
                ScrollViewer {
                    vertical_scroll_bar_visibility: ScrollBarVisibility::Auto,
                    { quick_send_panel(draft) }
                }
            }
            Grid {
                Grid.row: 2,
                Grid.column: 0,
                Grid.column_span: 2,
                { bottom_pane() }
            }
        }
    }
}
