//! The serial monitor page (issue #23): the pty-proxy panel of the bottom tools
//! pane — the unix story of llcom's Windows-only 串口监听.
//!
//! The XAML carries the skeleton (named controls, no visible strings); this
//! module resolves them, sets every label from the i18n dictionaries, wires
//! the events and pumps [`UiMsg::Monitor`] messages into the page state. Like
//! the rest of the app it lives on the UI thread only (`Rc` handles) — the page
//! reaches the core through the process-global [`MonitorHub`], whose sync
//! methods drive the relay threads directly.
//!
//! What the page says about itself (the platform story + the honesty note):
//! the monitor sees **what flows through rcom's relay** — it is not a passive
//! wire tap. On Windows it would say the FFI verdict; on unix it says pty
//! proxy. Traffic echoes into the main log view (the host driver does that
//! part — see [`format_monitor_row`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{Button, ComboBox, ListBox, TabItem, TextBox, TextBlock};

use rust_i18n::t;

use rcom::core::bus::Payload;
use rcom::core::monitor::hub;
use rcom::core::monitor::{
    MonitorConfig, MonitorDirection, MonitorEvent, MonitorEventKind, Upstream,
};
use rcom::core::port::PortInfo;

/// How many rows the page's traffic list keeps (the main log view has its own,
/// larger cap — this is the tools-page echo).
const PAGE_ROW_CAP: usize = 500;

/// The baud rate the baud box starts with (the real-port upstream default).
const DEFAULT_BAUD: u32 = 115_200;

/// One page instance: the resolved controls plus its command state.
struct MonitorPage {
    tab: Ref<TabItem>,
    upstream: Ref<ComboBox>,
    baud_box: Ref<TextBox>,
    start: Ref<Button>,
    status: Ref<TextBlock>,
    port_caption: Ref<TextBlock>,
    port_box: Ref<TextBox>,
    mode_story: Ref<TextBlock>,
    log: Ref<ListBox>,
    /// The monitor this page currently drives, if any.
    monitor: Cell<Option<u64>>,
    /// Whether that monitor counts as running (the Start/Stop button follows).
    running: Cell<bool>,
    /// The ports behind the upstream selector (index 0 is the loopback entry).
    ports: RefCell<Vec<PortInfo>>,
}

impl MonitorPage {
    /// Applies the locale strings once at attach time (the app re-creates the
    /// window per locale change — the rest of the app works the same way).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("monitor_tab").to_string())));
        self.port_caption.set_text(Some(&t!("monitor_port_caption")));
        self.start.set_content(Some(Rc::new(t!("monitor_start").to_string())));
        self.status.set_text(Some(&t!("monitor_status_idle")));
        // The platform story: pty proxy here, the license verdict on Windows.
        self.mode_story.set_text(Some(&t!("monitor_mode_story")));
        self.baud_box.set_text(Some(&DEFAULT_BAUD.to_string()));

        self.fill_upstream_selector();
        // The virtual port path shows in a read-only (still selectable/copyable)
        // box once a monitor runs.
        self.port_box.set_is_read_only(true);
    }

    /// The upstream selector: the loopback entry first, then the ports the
    /// backend lists right now (the same listing the main toolbar shows).
    fn fill_upstream_selector(&self) {
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (same guard as the settings page).
        self.upstream.set_selected_index(-1);
        let items = self.upstream.items();
        items.clear();
        let mut value: BoxedValue = Rc::new(t!("monitor_upstream_loopback").to_string());
        items.add(Some(value));
        *self.ports.borrow_mut() = hub::global()
            .map(|hub| hub.list_ports().unwrap_or_default())
            .unwrap_or_default();
        for info in self.ports.borrow().iter() {
            let name = match &info.description {
                Some(description) if !info.is_mock() => {
                    format!("{}  ({description})", info.name)
                }
                _ => info.name.clone(),
            };
            value = Rc::new(name);
            items.add(Some(value));
        }
        self.upstream.set_selected_index(0);
    }

    /// The upstream from the current selection; `Err` carries the user-facing
    /// reason.
    fn current_upstream(&self) -> Result<Upstream, String> {
        let index = self.upstream.selected_index();
        if index <= 0 {
            return Ok(Upstream::VirtualLoopback);
        }
        let port = self
            .ports
            .borrow()
            .get((index - 1) as usize)
            .map(|info| info.name.clone())
            .ok_or_else(|| t!("monitor_bad_selection").to_string())?;
        let baud: u32 = self
            .baud_box
            .text()
            .unwrap_or_default()
            .trim()
            .parse()
            .map_err(|_| t!("monitor_bad_baud").to_string())?;
        Ok(Upstream::Real { port, baud })
    }

    /// The Start/Stop button: starts a monitor from the fields, or stops the
    /// current one.
    fn toggle_start(&self) {
        let Some(hub) = hub::global() else {
            self.set_status(t!("monitor_failed", error = t!("monitor_hub_unavailable").to_string()).to_string());
            return;
        };
        if let (Some(monitor), true) = (self.monitor.get(), self.running.get()) {
            hub.stop_monitor(monitor);
            self.set_status(t!("monitor_status_stopping").to_string());
            return;
        }
        let upstream = match self.current_upstream() {
            Ok(upstream) => upstream,
            Err(message) => {
                self.set_status(message);
                return;
            }
        };
        match hub.start_monitor(MonitorConfig { upstream }) {
            Ok((monitor, port)) => {
                self.monitor.set(Some(monitor));
                self.running.set(true);
                self.start.set_content(Some(Rc::new(t!("monitor_stop").to_string())));
                self.port_box.set_text(Some(&port));
                self.set_status(t!("monitor_status_running").to_string());
            }
            Err(error) => {
                self.set_status(t!("monitor_failed", error = &error).to_string());
            }
        }
    }

    fn set_status(&self, text: String) {
        self.status.set_text(Some(&text));
    }

    /// One row into the page's list (capped; the newest visible, auto-scrolled).
    fn append_page_row(&self, text: String) {
        let items = self.log.items();
        if self.log.item_count() >= PAGE_ROW_CAP as i32 {
            items.remove_at(0);
        }
        let value: BoxedValue = Rc::new(text);
        items.add(Some(value));
        let count = self.log.item_count();
        if count > 0 {
            self.log.scroll_into_view(count - 1);
        }
    }

    /// The UI reaction to one [`MonitorEvent`] (also mirrored to the main log
    /// view by the host driver).
    fn apply_monitor(&self, monitor: &MonitorEvent) {
        let mine = self.monitor.get() == Some(monitor.monitor_id);
        match &monitor.kind {
            MonitorEventKind::Started { virtual_port, upstream } => {
                self.append_page_row(format!(
                    "## [mon#{id}] {}",
                    t!(
                        "monitor_started",
                        port = virtual_port,
                        upstream = localize_upstream(upstream)
                    ),
                    id = monitor.monitor_id,
                ));
            }
            MonitorEventKind::Stopped { .. } => {
                self.append_page_row(format!(
                    "## [mon#{id}] {}",
                    t!("monitor_closed"),
                    id = monitor.monitor_id,
                ));
                if mine {
                    self.running.set(false);
                    self.monitor.set(None);
                    self.start.set_content(Some(Rc::new(t!("monitor_start").to_string())));
                    self.set_status(t!("monitor_status_idle").to_string());
                }
            }
            MonitorEventKind::Error { message } => {
                self.append_page_row(format!(
                    "!! [mon#{id}] {}",
                    t!("monitor_failed", error = message),
                    id = monitor.monitor_id,
                ));
                if mine {
                    self.running.set(false);
                    self.start.set_content(Some(Rc::new(t!("monitor_start").to_string())));
                    self.set_status(t!("monitor_failed", error = message).to_string());
                }
            }
            MonitorEventKind::Data { direction, bytes } => {
                self.append_page_row(format_monitor_data_row(
                    monitor.monitor_id,
                    *direction,
                    bytes,
                ));
            }
        }
    }
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle to the page (the pump and the event handlers run
    /// there; the pattern of the controller's PUMP).
    static PAGE: RefCell<Option<Rc<MonitorPage>>> = RefCell::new(None);
}

/// Resolves the named controls of the monitor tab, sets the strings and
/// wires the events. One insertion point in the controller's `attach`.
pub fn attach(element: &Ref<StyledElement>) {
    let page = Rc::new(MonitorPage {
        tab: NameScopeExtensions::get(element, "MonitorTab"),
        upstream: NameScopeExtensions::get(element, "MonitorUpstreamSelector"),
        baud_box: NameScopeExtensions::get(element, "MonitorBaudBox"),
        start: NameScopeExtensions::get(element, "MonitorStartButton"),
        status: NameScopeExtensions::get(element, "MonitorStatus"),
        port_caption: NameScopeExtensions::get(element, "MonitorPortCaption"),
        port_box: NameScopeExtensions::get(element, "MonitorPortBox"),
        mode_story: NameScopeExtensions::get(element, "MonitorModeStory"),
        log: NameScopeExtensions::get(element, "MonitorLog"),
        monitor: Cell::new(None),
        running: Cell::new(false),
        ports: RefCell::new(Vec::new()),
    });
    page.apply_ui_strings();

    page.start.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.toggle_start()
    });
    // Enter in the baud box starts (the natural "press return" flow).
    page.baud_box.key_down({
        let page = Rc::clone(&page);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                page.toggle_start();
            }
        }
    });

    PAGE.with(|cell| *cell.borrow_mut() = Some(page));
}

/// The pump's entry for [`UiMsg::Monitor`](super::host::UiMsg::Monitor).
pub fn apply_monitor(monitor: MonitorEvent) {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.apply_monitor(&monitor);
        }
    });
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

/// The sweep (ticket #25): the core publishes the loopback upstream's English
/// label as data; the render boundary translates the known one and passes
/// real-port labels (`/dev/ttyUSB0 @ 115200` — locale-neutral) through.
fn localize_upstream(upstream: &str) -> String {
    if upstream == Upstream::VirtualLoopback.label() {
        t!("monitor_upstream_loopback").to_string()
    } else {
        upstream.to_string()
    }
}

/// The row a monitor event renders as in the main log view: the relay's
/// traffic appears in the session log (the page keeps its own, richer list).
pub fn format_monitor_row(monitor: &MonitorEvent) -> String {
    let stamp = monitor.timestamp.format("%H:%M:%S%.3f");
    match &monitor.kind {
        MonitorEventKind::Started { virtual_port, upstream } => format!(
            "{stamp}  ==  [mon#{id}] {detail}",
            id = monitor.monitor_id,
            detail = t!("monitor_started", port = virtual_port, upstream = localize_upstream(upstream)),
        ),
        MonitorEventKind::Stopped { .. } => format!(
            "{stamp}  ==  [mon#{id}] {detail}",
            id = monitor.monitor_id,
            detail = t!("monitor_closed"),
        ),
        MonitorEventKind::Error { message } => format!(
            "{stamp}  !!  [mon#{id}] {detail}",
            id = monitor.monitor_id,
            detail = t!("monitor_failed", error = message),
        ),
        MonitorEventKind::Data { direction, bytes } => format!(
            "{stamp}  {}",
            format_monitor_data_row(monitor.monitor_id, *direction, bytes)
        ),
    }
}

/// One captured chunk as `←/→ N B` plus the lossy text preview. The arrow is
/// relative to the monitored app: `→` it transmitted, `←` it received.
fn format_monitor_data_row(
    monitor_id: u64,
    direction: MonitorDirection,
    bytes: &Payload,
) -> String {
    let text = String::from_utf8_lossy(bytes.as_slice());
    let text = if text.chars().count() > 64 {
        format!("{} …", text.chars().take(64).collect::<String>())
    } else {
        text.replace('\n', "\\n").replace('\r', "\\r")
    };
    format!(
        "[mon#{id}] {arrow} {tag} {n} B  \"{text}\"",
        id = monitor_id,
        arrow = direction.arrow(),
        tag = direction.tag(),
        n = bytes.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn event(kind: MonitorEventKind) -> MonitorEvent {
        MonitorEvent {
            monitor_id: 5,
            timestamp: Local::now(),
            kind,
        }
    }

    /// The main-log rows: lifecycle kinds render their i18n line, data renders
    /// the arrow + tag preview with the byte count (direction + content are the
    /// AC).
    #[test]
    fn monitor_rows_render_lifecycle_and_data() {
        let row = format_monitor_row(&event(MonitorEventKind::Started {
            virtual_port: "/dev/ttys004".into(),
            upstream: "loopback (virtual /dev/ttys005)".into(),
        }));
        assert!(row.contains("[mon#5]"), "{row}");
        assert!(row.contains("/dev/ttys004"), "{row}");
        assert!(row.contains("loopback"), "{row}");

        let row = format_monitor_row(&event(MonitorEventKind::Stopped { reason: None }));
        assert!(row.contains("mon#5"), "{row}");

        let row = format_monitor_row(&event(MonitorEventKind::Error { message: "boom".into() }));
        assert!(row.contains("!!"), "{row}");
        assert!(row.contains("boom"), "{row}");
    }

    /// The data preview shows direction (arrow + RX/TX tag), byte count and
    /// text; long payloads truncate; line breaks escape.
    #[test]
    fn monitor_data_rows_show_direction_and_preview() {
        let row = format_monitor_data_row(
            1,
            MonitorDirection::Tx,
            &Payload::from_vec(b"hi\r\n".to_vec()),
        );
        assert!(row.contains('→'), "{row}");
        assert!(row.contains("TX"), "{row}");
        assert!(row.contains("4 B"), "{row}");
        assert!(row.contains("hi\\r\\n"), "{row}");

        let rx = format_monitor_data_row(1, MonitorDirection::Rx, &Payload::from_vec(b"y".to_vec()));
        assert!(rx.contains('←'), "{rx}");
        assert!(rx.contains("RX"), "{rx}");

        let long: Vec<u8> = std::iter::repeat(b'x').take(100).collect();
        let row = format_monitor_data_row(1, MonitorDirection::Rx, &Payload::from_vec(long));
        assert!(row.contains('…'), "{row}");
    }

    /// The arrows and tags are the documented way round: Tx is the app's
    /// transmit (→, app→device), Rx its receive (←, device→app).
    #[test]
    fn direction_arrows_follow_the_monitored_app() {
        assert_eq!(MonitorDirection::Tx.arrow(), "→");
        assert_eq!(MonitorDirection::Tx.tag(), "TX");
        assert_eq!(MonitorDirection::Rx.arrow(), "←");
        assert_eq!(MonitorDirection::Rx.tag(), "RX");
    }

    /// The upstream labels: loopback reads as virtual, a real port names the
    /// port and baud.
    #[test]
    fn upstream_labels_name_the_target() {
        assert_eq!(Upstream::VirtualLoopback.label(), "loopback (virtual)");
        assert_eq!(
            Upstream::Real { port: "/dev/ttyUSB0".into(), baud: 115_200 }.label(),
            "/dev/ttyUSB0 @ 115200"
        );
    }

    /// The upstream label localizes at the render boundary (the sweep): the
    /// core's English loopback label translates, real-port labels pass through.
    #[test]
    fn upstream_labels_localize() {
        assert_eq!(
            localize_upstream(&Upstream::VirtualLoopback.label()),
            t!("monitor_upstream_loopback").to_string()
        );
        assert_eq!(localize_upstream("/dev/ttyUSB0 @ 115200"), "/dev/ttyUSB0 @ 115200");
    }
}
