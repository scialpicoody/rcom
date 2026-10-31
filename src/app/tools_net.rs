//! The net tools page (issue #20): the TCP/UDP/TLS client+server panel of the
//! bottom tools pane.
//!
//! The XAML carries the skeleton (named controls, no visible strings); this
//! module resolves them, sets every label from the i18n dictionaries, wires
//! the events and pumps [`UiMsg::Net`] messages into the page state. Like the
//! rest of the app it lives on the UI thread only (`Rc` handles) — the page
//! reaches the core through the process-global [`NetHub`], whose sync methods
//! spawn the actual work on the core runtime.
//!
//! Traffic echo: every net event renders one row into the page's list *and*
//! into the main log view (the host driver does that part — see
//! [`format_net_row`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{Button, CheckBox, ComboBox, ListBox, TabItem, TextBox, TextBlock};

use rust_i18n::t;

use rcom::core::bus::Payload;
use rcom::core::nettools::hub;
use rcom::core::nettools::{ChannelConfig, NetDirection, NetEvent, NetEventKind, NetMode, NetProtocol};

/// How many rows the page's traffic list keeps (the main log view has its own,
/// larger cap — this is the tools-page echo).
const PAGE_ROW_CAP: usize = 500;

/// The protocol the protocol selector offers at an index (mirrors
/// [`NetProtocol::all`]).
fn protocol_at(index: i32) -> NetProtocol {
    NetProtocol::all().get(index.max(0) as usize).copied().unwrap_or(NetProtocol::Tcp)
}

/// The mode the mode selector offers at an index.
fn mode_at(index: i32) -> NetMode {
    if index == 1 { NetMode::Server } else { NetMode::Client }
}

/// One page instance: the resolved controls plus its command state.
struct NetPage {
    tab: Ref<TabItem>,
    protocol: Ref<ComboBox>,
    mode: Ref<ComboBox>,
    cert: Ref<CheckBox>,
    host: Ref<TextBox>,
    port: Ref<TextBox>,
    start: Ref<Button>,
    bridge: Ref<CheckBox>,
    status: Ref<TextBlock>,
    send_box: Ref<TextBox>,
    send_button: Ref<Button>,
    log: Ref<ListBox>,
    /// The §4.1 empty-state caption (naming the next action; disappears on
    /// first data and never returns).
    empty: Ref<TextBlock>,
    /// The channel this page currently drives, if any.
    channel: Cell<Option<u64>>,
    /// Whether that channel counts as running (the Start/Stop button follows).
    running: Cell<bool>,
}

impl NetPage {
    /// Applies the locale strings once at attach time (the app re-creates the
    /// window per locale change — the rest of the app works the same way).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("net_tools").to_string())));
        self.cert.set_content(Some(Rc::new(t!("net_cert").to_string())));
        self.bridge.set_content(Some(Rc::new(t!("net_bridge").to_string())));
        self.host.set_placeholder_text(Some(&t!("net_host_hint")));
        self.port.set_placeholder_text(Some(&t!("net_port_hint")));
        self.send_box.set_placeholder_text(Some(&t!("net_send_hint")));
        self.start.set_content(Some(Rc::new(t!("net_start").to_string())));
        self.send_button.set_content(Some(Rc::new(t!("net_send").to_string())));
        self.status.set_text(Some(&t!("net_idle")));
        // §4.1: exactly one accent primary action per page header.
        super::ui_patterns::accent(&self.start);
        self.empty.set_text(Some(&t!("net_empty")));

        self.protocol.set_selected_index(-1);
        let protocols = self.protocol.items();
        protocols.clear();
        for protocol in NetProtocol::all() {
            let label = match protocol {
                NetProtocol::Tcp => "TCP",
                NetProtocol::Udp => "UDP",
                NetProtocol::Tls => "TCP-TLS",
            };
            let value: BoxedValue = Rc::new(label.to_string());
            protocols.add(Some(value));
        }
        self.protocol.set_selected_index(0);

        self.mode.set_selected_index(-1);
        let modes = self.mode.items();
        modes.clear();
        for mode in [NetMode::Client, NetMode::Server] {
            let label = match mode {
                NetMode::Client => t!("net_mode_client").to_string(),
                NetMode::Server => t!("net_mode_server").to_string(),
            };
            let value: BoxedValue = Rc::new(label);
            modes.add(Some(value));
        }
        self.mode.set_selected_index(0);
    }

    /// The start configuration from the current fields; `Err` carries the
    /// user-facing reason.
    fn current_config(&self) -> Result<ChannelConfig, String> {
        let host = self.host.text().unwrap_or_else(|| "127.0.0.1".into());
        let port_text = self.port.text().unwrap_or_default();
        let port: u16 = port_text.trim().parse().map_err(|_| t!("net_bad_port").to_string())?;
        Ok(ChannelConfig {
            protocol: protocol_at(self.protocol.selected_index()),
            mode: mode_at(self.mode.selected_index()),
            host,
            port,
            accept_any_cert: self.cert.is_checked().unwrap_or(false),
        })
    }

    /// The Start/Stop button: starts a channel from the fields, or stops the
    /// current one.
    fn toggle_start(&self) {
        let Some(hub) = hub::global() else {
            self.set_status(t!("net_failed", error = t!("net_hub_unavailable").to_string()).to_string());
            return;
        };
        if let Some(channel) = self.channel.get() {
            if self.running.get() {
                hub.stop_channel(channel);
                self.set_status(t!("net_starting").to_string());
                return;
            }
        }
        let config = match self.current_config() {
            Ok(config) => config,
            Err(message) => {
                self.set_status(message);
                return;
            }
        };
        match hub.start_channel(config) {
            Ok(channel) => {
                self.channel.set(Some(channel));
                self.running.set(true);
                self.start.set_content(Some(Rc::new(t!("net_stop").to_string())));
                self.set_status(t!("net_starting").to_string());
                // A bridge requested before the start attaches now.
                if self.bridge.is_checked() == Some(true) {
                    self.apply_bridge();
                }
            }
            Err(error) => {
                self.set_status(t!("net_start_failed", error = &error).to_string());
            }
        }
    }

    /// Sends the send box's text as UTF-8 (the wire bytes; scripts and the log
    /// view own any richer encoding).
    fn send_text(&self) {
        let Some(text) = self.send_box.text() else { return };
        if text.is_empty() {
            return;
        }
        self.send_bytes(text.into_bytes());
    }

    fn send_bytes(&self, bytes: Vec<u8>) {
        let Some(channel) = self.channel.get() else {
            self.set_status(t!("net_not_running").to_string());
            return;
        };
        let Some(hub) = hub::global() else { return };
        if !hub.send(channel, bytes) {
            self.set_status(t!("net_not_running").to_string());
        }
    }

    /// Attaches or detaches the net⇄serial bridge for the current channel.
    fn apply_bridge(&self) {
        let Some(hub) = hub::global() else { return };
        let Some(channel) = self.channel.get() else { return };
        if self.bridge.is_checked() == Some(true) {
            match hub.attach_bridge(channel) {
                Ok(session) => {
                    self.append_page_row(format!("## {}", t!("net_bridge_on", channel = channel, session = session)));
                }
                Err(error) => {
                    self.append_page_row(format!("!! {}", t!("net_start_failed", error = &error)));
                    self.bridge.set_is_checked(Some(false));
                }
            }
        } else {
            hub.detach_bridge();
            self.append_page_row(format!("## {}", t!("net_bridge_off")));
        }
    }

    fn set_status(&self, text: String) {
        self.status.set_text(Some(&text));
    }

    /// One row into the page's list (capped; the newest visible, auto-scrolled).
    fn append_page_row(&self, text: String) {
        // First data: the empty caption disappears and never returns (§4.1).
        self.empty.set_is_visible(false);
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

    /// The UI reaction to one [`NetEvent`] (also mirrored to the main log view
    /// by the host driver).
    fn apply_net(&self, net: &NetEvent) {
        let mine = self.channel.get() == Some(net.channel_id);
        match &net.kind {
            NetEventKind::Listening { local } => {
                if mine {
                    self.set_status(t!("net_listening", local = local.to_string()).to_string());
                }
                self.append_page_row(format!(
                    "## [net#{id} {proto} {mode}] {}",
                    t!("net_listening", local = local.to_string()),
                    id = net.channel_id,
                    proto = net.protocol.label(),
                    mode = mode_label(net.mode),
                ));
            }
            NetEventKind::Connected { remote } => {
                if mine {
                    self.set_status(t!("net_connected", remote = remote.to_string()).to_string());
                }
                self.append_page_row(format!(
                    "## [net#{id} {proto} {mode}] {}",
                    t!("net_connected", remote = remote.to_string()),
                    id = net.channel_id,
                    proto = net.protocol.label(),
                    mode = mode_label(net.mode),
                ));
            }
            NetEventKind::Error { message } => {
                self.append_page_row(format!(
                    "!! [net#{id}] {}",
                    t!("net_failed", error = message),
                    id = net.channel_id,
                ));
            }
            NetEventKind::Disconnected { .. } => {
                self.append_page_row(format!(
                    "## [net#{id} {proto} {mode}] {}",
                    t!("net_closed"),
                    id = net.channel_id,
                    proto = net.protocol.label(),
                    mode = mode_label(net.mode),
                ));
                if mine {
                    self.running.set(false);
                    self.start.set_content(Some(Rc::new(t!("net_start").to_string())));
                    self.set_status(t!("net_closed").to_string());
                    // The channel is gone; a dangling bridge would only error.
                    if hub::global().is_some_and(|hub| hub.bridged_channel() == Some(net.channel_id)) {
                        hub::global().map(|hub| hub.detach_bridge());
                        self.bridge.set_is_checked(Some(false));
                    }
                }
            }
            NetEventKind::Data { direction, remote, bytes } => {
                self.append_page_row(format_net_data_row(net.channel_id, *direction, *remote, bytes));
            }
        }
    }
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle to the page (the pump and the event handlers run
    /// there; the pattern of the controller's PUMP).
    static PAGE: RefCell<Option<Rc<NetPage>>> = RefCell::new(None);
}

/// Resolves the named controls of the net tools tab, sets the strings and
/// wires the events. One insertion point in the controller's `attach`.
pub fn attach(element: &Ref<StyledElement>) {
    let page = Rc::new(NetPage {
        tab: NameScopeExtensions::get(element, "NetToolsTab"),
        protocol: NameScopeExtensions::get(element, "NetProtocolSelector"),
        mode: NameScopeExtensions::get(element, "NetModeSelector"),
        cert: NameScopeExtensions::get(element, "NetCertCheck"),
        host: NameScopeExtensions::get(element, "NetHostBox"),
        port: NameScopeExtensions::get(element, "NetPortBox"),
        start: NameScopeExtensions::get(element, "NetStartButton"),
        bridge: NameScopeExtensions::get(element, "NetBridgeCheck"),
        status: NameScopeExtensions::get(element, "NetStatus"),
        send_box: NameScopeExtensions::get(element, "NetSendBox"),
        send_button: NameScopeExtensions::get(element, "NetSendButton"),
        log: NameScopeExtensions::get(element, "NetLog"),
        empty: NameScopeExtensions::get(element, "NetEmpty"),
        channel: Cell::new(None),
        running: Cell::new(false),
    });
    page.apply_ui_strings();

    page.start.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.toggle_start()
    });
    page.send_button.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.send_text()
    });
    page.send_box.key_down({
        let page = Rc::clone(&page);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                page.send_text();
            }
        }
    });
    page.bridge.is_checked_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_bridge()
    });

    PAGE.with(|cell| *cell.borrow_mut() = Some(page));
}

/// The pump's entry for [`UiMsg::Net`](super::host::UiMsg::Net).
pub fn apply_net(net: NetEvent) {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.apply_net(&net);
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

/// The row a net event renders as in the main log view: the tools' traffic
/// appears in the session log (the page keeps its own, richer list).
pub fn format_net_row(net: &NetEvent) -> String {
    let stamp = net.timestamp.format("%H:%M:%S%.3f");
    match &net.kind {
        NetEventKind::Listening { local } => format!(
            "{stamp}  ==  [net#{id} {proto} {mode}] {detail}",
            id = net.channel_id,
            proto = net.protocol.label(),
            mode = mode_label(net.mode),
            detail = t!("net_listening", local = local.to_string()),
        ),
        NetEventKind::Connected { remote } => format!(
            "{stamp}  ==  [net#{id} {proto} {mode}] {detail}",
            id = net.channel_id,
            proto = net.protocol.label(),
            mode = mode_label(net.mode),
            detail = t!("net_connected", remote = remote.to_string()),
        ),
        NetEventKind::Disconnected { .. } => format!(
            "{stamp}  ==  [net#{id} {proto} {mode}] {detail}",
            id = net.channel_id,
            proto = net.protocol.label(),
            mode = mode_label(net.mode),
            detail = t!("net_closed"),
        ),
        NetEventKind::Error { message } => format!(
            "{stamp}  !!  [net#{id}] {detail}",
            id = net.channel_id,
            detail = t!("net_failed", error = message),
        ),
        NetEventKind::Data { direction, remote, bytes } => {
            format!("{stamp}  {}", format_net_data_row(net.channel_id, *direction, *remote, bytes))
        }
    }
}

/// The mode's label for the row tags (`[net#3 TCP client]`), localized —
/// the sweep translates even the tag prose (the protocol names stay proper
/// nouns).
fn mode_label(mode: NetMode) -> String {
    match mode {
        NetMode::Client => t!("net_mode_client").to_string(),
        NetMode::Server => t!("net_mode_server").to_string(),
    }
}

/// One data chunk as `←/→ N bytes from peer` plus the lossy text preview.
fn format_net_data_row(
    channel_id: u64,
    direction: NetDirection,
    remote: std::net::SocketAddr,
    bytes: &Payload,
) -> String {
    let arrow = match direction {
        NetDirection::Rx => "←",
        NetDirection::Tx => "→",
    };
    let text = String::from_utf8_lossy(bytes.as_slice());
    let text = if text.chars().count() > 64 {
        format!("{} …", text.chars().take(64).collect::<String>())
    } else {
        text.replace('\n', "\\n").replace('\r', "\\r")
    };
    format!(
        "[net#{id}] {arrow} {n} B {peer}  \"{text}\"",
        id = channel_id,
        n = bytes.len(),
        peer = remote,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn event(kind: NetEventKind) -> NetEvent {
        NetEvent {
            channel_id: 3,
            protocol: NetProtocol::Tcp,
            mode: NetMode::Client,
            timestamp: Local::now(),
            kind,
        }
    }

    /// The main-log rows: lifecycle kinds render their i18n line, data renders
    /// the arrow preview with the peer and the byte count.
    #[test]
    fn net_rows_render_lifecycle_and_data() {
        let row = format_net_row(&event(NetEventKind::Connected { remote: "[::1]:80".parse().unwrap() }));
        assert!(
            row.contains(&format!("[net#3 TCP {}]", t!("net_mode_client"))),
            "{row}"
        );
        assert!(row.contains("[::1]:80"), "{row}");

        let row = format_net_row(&event(NetEventKind::Disconnected { reason: None }));
        assert!(row.contains("net#3"), "{row}");

        let row = format_net_row(&event(NetEventKind::Error { message: "boom".into() }));
        assert!(row.contains("!!"), "{row}");
        assert!(row.contains("boom"), "{row}");
    }

    /// The row-tag mode label is localized (the sweep): en reads "client".
    #[test]
    fn mode_label_is_localized() {
        assert_eq!(mode_label(NetMode::Client), t!("net_mode_client").to_string());
        assert_eq!(mode_label(NetMode::Server), t!("net_mode_server").to_string());
    }

    /// The data preview truncates long payloads and escapes line breaks.
    #[test]
    fn net_data_rows_preview_text() {
        let row = format_net_data_row(
            1,
            NetDirection::Rx,
            "127.0.0.1:9000".parse().unwrap(),
            &Payload::from_vec(b"hi\r\n".to_vec()),
        );
        assert!(row.contains("←"), "{row}");
        assert!(row.contains("4 B"), "{row}");
        assert!(row.contains("hi\\r\\n"), "{row}");

        let long: Vec<u8> = std::iter::repeat(b'x').take(100).collect();
        let row = format_net_data_row(
            1,
            NetDirection::Tx,
            "127.0.0.1:9000".parse().unwrap(),
            &Payload::from_vec(long),
        );
        assert!(row.contains('…'), "{row}");
    }

    /// The selector index mapping mirrors `NetProtocol::all` and the two modes.
    #[test]
    fn selector_indices_map_to_protocol_and_mode() {
        for (index, protocol) in NetProtocol::all().iter().enumerate() {
            assert_eq!(protocol_at(index as i32), *protocol);
        }
        assert_eq!(protocol_at(-1), NetProtocol::Tcp);
        assert_eq!(protocol_at(99), NetProtocol::Tcp);
        assert_eq!(mode_at(0), NetMode::Client);
        assert_eq!(mode_at(1), NetMode::Server);
        assert_eq!(mode_at(42), NetMode::Client);
    }
}
