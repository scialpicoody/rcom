//! Shell candidate B (#28): top tab strip.
//!
//! One `TabControl` tab per session (`COM3 ●`, `COM7 ○`, `+`), a
//! `CommandBar` toolbar row under the tabs, the session content (log,
//! quick-send, send bar) inside the selected tab, and the bottom pane across
//! the whole window. Styled with the WinUI theme;
//! `FERROUI_SMOKE_EXIT_MS=<n>` closes the window after `n` ms (handled by
//! `launch_themed`).
//!
//! ```text
//! cargo run --example shell_b
//! FERROUI_SMOKE_EXIT_MS=3000 cargo run --example shell_b
//! cargo run --example shell_b -- --selfcheck   # headless tree check
//! ```

#[path = "shell_common/mod.rs"]
mod shell_common;

use ferroui_base::collections::FerroList;
use ferroui_base::media::TextWrapping;
use ferroui_base::Ref;
use ferroui_controls::primitives::ScrollBarVisibility;
use ferroui_controls::{CommandBar, CommandBarButton, CommandBarDefaultLabelPosition, Dock, ICommandBarElement};
use ferroui_themes_winui::{DensityStyle, WinUITheme};
use ferroui_view::prelude::*;
use shell_common::{bottom_pane, info_row, log_view, mock_rows, port_bar, quick_send_panel, send_bar, LogRow};
use std::rc::Rc;

/// The needles the selfcheck looks for among the texts of the tree.
const SELFCHECK_NEEDLES: &[&str] = &["COM7 — idle session", "Quick send", "P1=1234", "plot placeholder", "状态: OK"];

/// The session toolbar: a `CommandBar` with the primary commands inline and
/// one secondary command in the overflow. The primary commands live in a
/// `FerroList<Rc<dyn ICommandBarElement>>` (not a property child), so the bar
/// is assembled imperatively and handed to the tree as a control.
fn session_command_bar(rows: Signal<Vec<LogRow>>) -> Ref<CommandBar> {
    let commands = FerroList::<Rc<dyn ICommandBarElement>>::new();
    for label in ["New session", "Open / close port", "Clear log", "Save log"] {
        let item = CommandBarButton::new();
        item.set_label(Some(label));
        let (rows, note) = (rows, label.to_string());
        item.click(move |_, _| {
            rows.update(|v| {
                let seq = v.len() as u64 + 3000;
                v.push(info_row(seq, "12:00:34.000", &format!("command bar: {note} (mock)")));
            });
        });
        commands.add(item.as_command_bar_element());
    }

    let overflow = FerroList::<Rc<dyn ICommandBarElement>>::new();
    let settings = CommandBarButton::new();
    settings.set_label(Some("Session settings"));
    overflow.add(settings.as_command_bar_element());

    let bar = CommandBar::new();
    bar.set_default_label_position(CommandBarDefaultLabelPosition::Right);
    bar.set_primary_commands(Some(commands));
    bar.set_secondary_commands(Some(overflow));
    bar
}

fn main() {
    let rows = mock_rows();
    if std::env::args().any(|arg| arg == "--selfcheck") {
        std::process::exit(shell_common::selfcheck("shell_b", SELFCHECK_NEEDLES, move || {
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
                    title: "rcom — shell B: top tab strip (mock)",
                    width: 1200.0,
                    height: 800.0,
                    { content(rows, draft) }
                }
            }
        },
        |_window| {},
    );
}

/// The window content: the session tabs, the `CommandBar` of the selected
/// session, and the bottom pane across the whole window.
fn content(rows: Signal<Vec<LogRow>>, draft: Signal<String>) -> Element {
    rsx! {
        DockPanel {
            last_child_fill: true,
            Grid {
                DockPanel.dock: Dock::Bottom,
                { bottom_pane() }
            }
            TabControl {
                TabItem {
                    header: "COM3 ●",
                    Grid {
                        row_definitions: "Auto,*",
                        Grid {
                            Grid.row: 0,
                            { session_command_bar(rows) }
                        }
                        Grid {
                            Grid.row: 1,
                            row_definitions: "Auto,*,Auto",
                            column_definitions: "*,280",
                            Grid {
                                Grid.row: 0,
                                Grid.column: 0,
                                Grid.column_span: 2,
                                { port_bar(rows) }
                            }
                            ScrollViewer {
                                Grid.row: 1,
                                Grid.column: 0,
                                vertical_scroll_bar_visibility: ScrollBarVisibility::Auto,
                                StackPanel {
                                    { log_view(rows) }
                                }
                            }
                            Border {
                                Grid.row: 1,
                                Grid.column: 1,
                                border_thickness: (1.0, 0.0, 0.0, 0.0),
                                { quick_send_panel(draft) }
                            }
                            Grid {
                                Grid.row: 2,
                                Grid.column: 0,
                                Grid.column_span: 2,
                                { send_bar(rows, draft) }
                            }
                        }
                    }
                }
                TabItem {
                    header: "COM7 ○",
                    StackPanel {
                        margin: (12.0, 12.0, 12.0, 12.0),
                        spacing: 8.0,
                        TextBlock {
                            text: "COM7 — idle session (mock)",
                            font_size: 16.0,
                        }
                        TextBlock {
                            text: "Each tab owns its port, quick-send set, Lua VM and log. This one is closed — pick COM3 above for the filled session.",
                            text_wrapping: TextWrapping::Wrap,
                            opacity: 0.7,
                        }
                    }
                }
                TabItem {
                    header: "+",
                    TextBlock {
                        text: "— new session placeholder —",
                        opacity: 0.6,
                        margin: (12.0, 12.0, 12.0, 12.0),
                    }
                }
            }
        }
    }
}
