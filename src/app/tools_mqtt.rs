//! The MQTT tools page (issue #21): the broker-session panel of the bottom
//! tools pane.
//!
//! The XAML carries the skeleton (named controls, no visible strings); this
//! module resolves them, sets every label from the i18n dictionaries, wires
//! the events and pumps [`UiMsg::Mqtt`] messages into the page state. Like the
//! rest of the app it lives on the UI thread only (`Rc` handles) — the page
//! reaches the core through the process-global [`MqttHub`], whose sync methods
//! spawn the actual work on the core runtime.
//!
//! Traffic echo: every MQTT event renders one row into the page's list *and*
//! into the main log view (the host driver does that part — see
//! [`format_mqtt_row`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{Button, CheckBox, ComboBox, ListBox, TabItem, TextBox, TextBlock};

use rust_i18n::t;

use rcom::core::bus::Payload;
use rcom::core::mqtt::hub;
use rcom::core::mqtt::{
    MqttConfig, MqttDirection, MqttEvent, MqttEventKind, MqttQos,
};

/// How many rows the page's traffic list keeps (the main log view has its own,
/// larger cap — this is the tools-page echo).
const PAGE_ROW_CAP: usize = 500;

/// The QoS level a selector offers at an index (mirrors [`MqttQos::all`]).
fn qos_at(index: i32) -> MqttQos {
    MqttQos::from_index(index)
}

/// The `topic (qos N)` label of one subscription list entry.
fn subscription_label(topic: &str, qos: MqttQos) -> String {
    format!("{topic}  (qos {})", qos.wire())
}

/// One page instance: the resolved controls plus its command state.
struct MqttPage {
    tab: Ref<TabItem>,
    host: Ref<TextBox>,
    port: Ref<TextBox>,
    client: Ref<TextBox>,
    tls: Ref<CheckBox>,
    user: Ref<TextBox>,
    pass: Ref<TextBox>,
    connect: Ref<Button>,
    sub_box: Ref<TextBox>,
    sub_qos: Ref<ComboBox>,
    sub_button: Ref<Button>,
    unsub_button: Ref<Button>,
    subs_list: Ref<ComboBox>,
    pub_topic: Ref<TextBox>,
    pub_qos: Ref<ComboBox>,
    retain: Ref<CheckBox>,
    pub_payload: Ref<TextBox>,
    pub_button: Ref<Button>,
    bridge: Ref<CheckBox>,
    bridge_topic: Ref<TextBox>,
    status: Ref<TextBlock>,
    log: Ref<ListBox>,
    /// The MQTT session this page currently drives, if any.
    session: Cell<Option<u64>>,
    /// Whether that session counts as connected-or-connecting (the
    /// Connect/Disconnect button follows).
    running: Cell<bool>,
    /// The live subscriptions of the current session (the list's mirror).
    subs: RefCell<Vec<(String, MqttQos)>>,
}

impl MqttPage {
    /// Applies the locale strings once at attach time (the app re-creates the
    /// window per locale change — the rest of the app works the same way).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("mqtt_tab").to_string())));
        self.tls.set_content(Some(Rc::new(t!("mqtt_tls").to_string())));
        self.retain.set_content(Some(Rc::new(t!("mqtt_retain").to_string())));
        self.bridge.set_content(Some(Rc::new(t!("mqtt_bridge").to_string())));
        self.host.set_placeholder_text(Some(&t!("mqtt_host_hint")));
        self.port.set_placeholder_text(Some(&t!("mqtt_port_hint")));
        self.client.set_placeholder_text(Some(&t!("mqtt_client_hint")));
        self.user.set_placeholder_text(Some(&t!("mqtt_user_hint")));
        self.pass.set_placeholder_text(Some(&t!("mqtt_pass_hint")));
        self.sub_box.set_placeholder_text(Some(&t!("mqtt_sub_hint")));
        self.pub_topic.set_placeholder_text(Some(&t!("mqtt_pub_topic_hint")));
        self.pub_payload.set_placeholder_text(Some(&t!("mqtt_pub_payload_hint")));
        self.bridge_topic.set_placeholder_text(Some(&t!("mqtt_bridge_topic_hint")));
        self.connect.set_content(Some(Rc::new(t!("mqtt_connect").to_string())));
        self.sub_button.set_content(Some(Rc::new(t!("mqtt_sub").to_string())));
        self.unsub_button.set_content(Some(Rc::new(t!("mqtt_unsub").to_string())));
        self.pub_button.set_content(Some(Rc::new(t!("mqtt_publish").to_string())));
        self.status.set_text(Some(&t!("mqtt_idle")));

        for selector in [&self.sub_qos, &self.pub_qos] {
            selector.set_selected_index(-1);
            let items = selector.items();
            items.clear();
            for qos in MqttQos::all() {
                let value: BoxedValue = Rc::new(format!("QoS {}", qos.wire()));
                items.add(Some(value));
            }
            selector.set_selected_index(0);
        }
    }

    /// The connect configuration from the current fields; `Err` carries the
    /// user-facing reason.
    fn current_config(&self) -> Result<MqttConfig, String> {
        let host = self.host.text().unwrap_or_default();
        let port_text = self.port.text().unwrap_or_default();
        let port: u16 = port_text.trim().parse().map_err(|_| t!("mqtt_bad_port").to_string())?;
        Ok(MqttConfig {
            host,
            port,
            client_id: self.client.text().unwrap_or_default(),
            username: self.user.text().filter(|name| !name.trim().is_empty()),
            password: self.pass.text().filter(|pass| !pass.is_empty()),
            tls: self.tls.is_checked() == Some(true),
            accept_any_cert: self.tls.is_checked() == Some(true),
        })
    }

    /// The Connect/Disconnect button: starts a session from the fields, or
    /// stops the current one.
    fn toggle_connect(&self) {
        let Some(mqtt) = hub::global() else {
            self.set_status(t!("mqtt_failed", error = t!("mqtt_hub_unavailable").to_string()).to_string());
            return;
        };
        if let Some(session) = self.session.get() {
            if self.running.get() {
                mqtt.stop_session(session);
                self.set_status(t!("mqtt_connecting").to_string());
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
        match mqtt.start_session(config) {
            Ok(session) => {
                self.session.set(Some(session));
                self.running.set(true);
                self.connect.set_content(Some(Rc::new(t!("mqtt_disconnect").to_string())));
                self.set_status(t!("mqtt_connecting").to_string());
                // A bridge requested before the connect attaches now.
                if self.bridge.is_checked() == Some(true) {
                    self.apply_bridge();
                }
            }
            Err(error) => {
                self.set_status(t!("mqtt_start_failed", error = &error).to_string());
            }
        }
    }

    /// The topic the subscribe row describes: the box's text, or — when empty —
    /// the selected entry of the subscriptions list (select-to-remove).
    fn sub_topic(&self) -> Option<String> {
        if let Some(text) = self.sub_box.text() {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        let index = self.subs_list.selected_index();
        if index >= 0 {
            return self
                .subs
                .borrow()
                .get(index as usize)
                .map(|(topic, _)| topic.clone());
        }
        None
    }

    fn subscribe(&self) {
        let Some(session) = self.session.get() else {
            self.set_status(t!("mqtt_not_running").to_string());
            return;
        };
        let Some(topic) = self.sub_topic() else { return };
        let Some(mqtt) = hub::global() else { return };
        let qos = qos_at(self.sub_qos.selected_index());
        if !mqtt.subscribe(session, &topic, qos) {
            self.set_status(t!("mqtt_not_running").to_string());
        }
    }

    fn unsubscribe(&self) {
        let Some(session) = self.session.get() else {
            self.set_status(t!("mqtt_not_running").to_string());
            return;
        };
        let Some(topic) = self.sub_topic() else { return };
        let Some(mqtt) = hub::global() else { return };
        if !mqtt.unsubscribe(session, &topic) {
            self.set_status(t!("mqtt_not_running").to_string());
        }
    }

    /// Publishes the payload box's text as UTF-8 (the wire bytes; scripts and
    /// the log view own any richer encoding).
    fn publish_text(&self) {
        let Some(session) = self.session.get() else {
            self.set_status(t!("mqtt_not_running").to_string());
            return;
        };
        let Some(topic) = self.pub_topic.text() else { return };
        if topic.trim().is_empty() {
            return;
        }
        let Some(payload) = self.pub_payload.text() else { return };
        let Some(mqtt) = hub::global() else { return };
        let qos = qos_at(self.pub_qos.selected_index());
        let retain = self.retain.is_checked() == Some(true);
        if !mqtt.publish(session, &topic, payload.into_bytes(), qos, retain) {
            self.set_status(t!("mqtt_not_running").to_string());
        }
    }

    /// Attaches or detaches the MQTT⇄serial bridge for the current session.
    fn apply_bridge(&self) {
        let Some(mqtt) = hub::global() else { return };
        let Some(session) = self.session.get() else { return };
        if self.bridge.is_checked() == Some(true) {
            let topic = self
                .bridge_topic
                .text()
                .filter(|topic| !topic.trim().is_empty());
            let Some(topic) = topic else {
                self.append_page_row(format!(
                    "!! {}",
                    t!("mqtt_start_failed", error = t!("mqtt_bridge_topic_empty").to_string())
                ));
                self.bridge.set_is_checked(Some(false));
                return;
            };
            match mqtt.attach_bridge(session, &topic) {
                Ok(port_session) => {
                    self.append_page_row(format!(
                        "## {}",
                        t!("mqtt_bridge_on", session = session, port = port_session)
                    ));
                }
                Err(error) => {
                    self.append_page_row(format!("!! {}", t!("mqtt_start_failed", error = &error)));
                    self.bridge.set_is_checked(Some(false));
                }
            }
        } else {
            mqtt.detach_bridge();
            self.append_page_row(format!("## {}", t!("mqtt_bridge_off")));
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

    /// Rebuilds the subscriptions list from the mirror (add/remove land here).
    fn rebuild_subs_list(&self) {
        let items = self.subs_list.items();
        items.clear();
        for (topic, qos) in self.subs.borrow().iter() {
            let value: BoxedValue = Rc::new(subscription_label(topic, *qos));
            items.add(Some(value));
        }
        self.subs_list.set_selected_index(-1);
    }

    /// Marks the session stopped (button label, status, subscriptions, bridge).
    fn mark_stopped(&self) {
        self.running.set(false);
        self.connect.set_content(Some(Rc::new(t!("mqtt_connect").to_string())));
        self.set_status(t!("mqtt_closed").to_string());
        self.subs.borrow_mut().clear();
        self.rebuild_subs_list();
        // The session is gone; a dangling bridge would only error.
        if let Some(mqtt) = hub::global() {
            if mqtt.bridged_session() == self.session.get() {
                mqtt.detach_bridge();
                self.bridge.set_is_checked(Some(false));
            }
        }
    }

    /// The UI reaction to one [`MqttEvent`] (also mirrored to the main log view
    /// by the host driver).
    fn apply_mqtt(&self, mqtt: &MqttEvent) {
        let mine = self.session.get() == Some(mqtt.session_id);
        match &mqtt.kind {
            MqttEventKind::Connected { server } => {
                if mine {
                    self.set_status(t!("mqtt_connected_to", server = server).to_string());
                }
                self.append_page_row(format!(
                    "## [mqtt#{id}] {}",
                    t!("mqtt_connected_to", server = server),
                    id = mqtt.session_id,
                ));
            }
            MqttEventKind::Error { message } => {
                if mine {
                    self.set_status(t!("mqtt_failed", error = message).to_string());
                }
                self.append_page_row(format!(
                    "!! [mqtt#{id}] {}",
                    t!("mqtt_failed", error = message),
                    id = mqtt.session_id,
                ));
            }
            MqttEventKind::Disconnected { .. } => {
                self.append_page_row(format!(
                    "## [mqtt#{id}] {}",
                    t!("mqtt_closed"),
                    id = mqtt.session_id,
                ));
                if mine {
                    self.mark_stopped();
                }
            }
            MqttEventKind::Subscribed { topic, qos } => {
                if mine {
                    self.subs
                        .borrow_mut()
                        .retain(|(existing, _)| existing != topic);
                    self.subs.borrow_mut().push((topic.clone(), *qos));
                    self.rebuild_subs_list();
                }
                self.append_page_row(format!(
                    "## [mqtt#{id}] {}",
                    t!("mqtt_subscribed", topic = topic, qos = qos.wire()),
                    id = mqtt.session_id,
                ));
            }
            MqttEventKind::Unsubscribed { topic } => {
                if mine {
                    self.subs.borrow_mut().retain(|(existing, _)| existing != topic);
                    self.rebuild_subs_list();
                }
                self.append_page_row(format!(
                    "## [mqtt#{id}] {}",
                    t!("mqtt_unsubscribed", topic = topic),
                    id = mqtt.session_id,
                ));
            }
            MqttEventKind::Message { direction, topic, payload, .. } => {
                self.append_page_row(format_mqtt_data_row(mqtt.session_id, *direction, topic, payload));
            }
        }
    }
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle to the page (the pump and the event handlers run
    /// there; the pattern of the controller's PUMP).
    static PAGE: RefCell<Option<Rc<MqttPage>>> = RefCell::new(None);
}

/// Resolves the named controls of the MQTT tab, sets the strings and wires the
/// events. One insertion point in the controller's `attach`.
pub fn attach(element: &Ref<StyledElement>) {
    let page = Rc::new(MqttPage {
        tab: NameScopeExtensions::get(element, "MqttTab"),
        host: NameScopeExtensions::get(element, "MqttHostBox"),
        port: NameScopeExtensions::get(element, "MqttPortBox"),
        client: NameScopeExtensions::get(element, "MqttClientBox"),
        tls: NameScopeExtensions::get(element, "MqttTlsCheck"),
        user: NameScopeExtensions::get(element, "MqttUserBox"),
        pass: NameScopeExtensions::get(element, "MqttPassBox"),
        connect: NameScopeExtensions::get(element, "MqttConnectButton"),
        sub_box: NameScopeExtensions::get(element, "MqttSubBox"),
        sub_qos: NameScopeExtensions::get(element, "MqttSubQosSelector"),
        sub_button: NameScopeExtensions::get(element, "MqttSubButton"),
        unsub_button: NameScopeExtensions::get(element, "MqttUnsubButton"),
        subs_list: NameScopeExtensions::get(element, "MqttSubsList"),
        pub_topic: NameScopeExtensions::get(element, "MqttPubTopicBox"),
        pub_qos: NameScopeExtensions::get(element, "MqttPubQosSelector"),
        retain: NameScopeExtensions::get(element, "MqttRetainCheck"),
        pub_payload: NameScopeExtensions::get(element, "MqttPubPayloadBox"),
        pub_button: NameScopeExtensions::get(element, "MqttPubButton"),
        bridge: NameScopeExtensions::get(element, "MqttBridgeCheck"),
        bridge_topic: NameScopeExtensions::get(element, "MqttBridgeTopicBox"),
        status: NameScopeExtensions::get(element, "MqttStatus"),
        log: NameScopeExtensions::get(element, "MqttLog"),
        session: Cell::new(None),
        running: Cell::new(false),
        subs: RefCell::new(Vec::new()),
    });
    page.apply_ui_strings();

    page.connect.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.toggle_connect()
    });
    page.sub_button.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.subscribe()
    });
    page.unsub_button.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.unsubscribe()
    });
    page.pub_button.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.publish_text()
    });
    page.pub_payload.key_down({
        let page = Rc::clone(&page);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                page.publish_text();
            }
        }
    });
    page.bridge.is_checked_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_bridge()
    });

    PAGE.with(|cell| *cell.borrow_mut() = Some(page));
}

/// The pump's entry for [`UiMsg::Mqtt`](super::host::UiMsg::Mqtt).
pub fn apply_mqtt(mqtt: MqttEvent) {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.apply_mqtt(&mqtt);
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
/// The row an MQTT event renders as in the main log view: the tool's traffic
/// appears in the session log (the page keeps its own, richer list).
pub fn format_mqtt_row(mqtt: &MqttEvent) -> String {
    let stamp = mqtt.timestamp.format("%H:%M:%S%.3f");
    match &mqtt.kind {
        MqttEventKind::Connected { server } => format!(
            "{stamp}  ==  [mqtt#{id}] {detail}",
            id = mqtt.session_id,
            detail = t!("mqtt_connected_to", server = server),
        ),
        MqttEventKind::Disconnected { .. } => format!(
            "{stamp}  ==  [mqtt#{id}] {detail}",
            id = mqtt.session_id,
            detail = t!("mqtt_closed"),
        ),
        MqttEventKind::Error { message } => format!(
            "{stamp}  !!  [mqtt#{id}] {detail}",
            id = mqtt.session_id,
            detail = t!("mqtt_failed", error = message),
        ),
        MqttEventKind::Subscribed { topic, qos } => format!(
            "{stamp}  ==  [mqtt#{id}] {detail}",
            id = mqtt.session_id,
            detail = t!("mqtt_subscribed", topic = topic, qos = qos.wire()),
        ),
        MqttEventKind::Unsubscribed { topic } => format!(
            "{stamp}  ==  [mqtt#{id}] {detail}",
            id = mqtt.session_id,
            detail = t!("mqtt_unsubscribed", topic = topic),
        ),
        MqttEventKind::Message { direction, topic, payload, .. } => {
            format!("{stamp}  {}", format_mqtt_data_row(mqtt.session_id, *direction, topic, payload))
        }
    }
}

/// One message as `←/→ topic "payload"` plus the byte count.
fn format_mqtt_data_row(session_id: u64, direction: MqttDirection, topic: &str, payload: &Payload) -> String {
    let arrow = match direction {
        MqttDirection::Rx => "←",
        MqttDirection::Tx => "→",
    };
    let text = String::from_utf8_lossy(payload.as_slice());
    let text = if text.chars().count() > 64 {
        format!("{} …", text.chars().take(64).collect::<String>())
    } else {
        text.replace('\n', "\\n").replace('\r', "\\r")
    };
    format!(
        "[mqtt#{id}] {arrow} {topic} ({n} B)  \"{text}\"",
        id = session_id,
        n = payload.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn event(kind: MqttEventKind) -> MqttEvent {
        MqttEvent {
            session_id: 3,
            timestamp: Local::now(),
            kind,
        }
    }

    /// The main-log rows: lifecycle kinds render their i18n line, messages
    /// render the arrow preview with the topic and the byte count.
    #[test]
    fn mqtt_rows_render_lifecycle_and_data() {
        let row = format_mqtt_row(&event(MqttEventKind::Connected { server: "127.0.0.1:1883".into() }));
        assert!(row.contains("[mqtt#3]"), "{row}");
        assert!(row.contains("127.0.0.1:1883"), "{row}");

        let row = format_mqtt_row(&event(MqttEventKind::Disconnected { reason: None }));
        assert!(row.contains("mqtt#3"), "{row}");

        let row = format_mqtt_row(&event(MqttEventKind::Error { message: "boom".into() }));
        assert!(row.contains("!!"), "{row}");
        assert!(row.contains("boom"), "{row}");

        let row = format_mqtt_row(&event(MqttEventKind::Subscribed {
            topic: "demo/up".into(),
            qos: MqttQos::AtLeastOnce,
        }));
        assert!(row.contains("subscribed"), "{row}");
        assert!(row.contains("demo/up"), "{row}");
        assert!(row.contains("qos 1"), "{row}");
    }

    /// The message preview truncates long payloads and escapes line breaks;
    /// directions render as ←/→.
    #[test]
    fn mqtt_data_rows_preview_text() {
        let rx = format_mqtt_data_row(1, MqttDirection::Rx, "demo/up", &Payload::from_vec(b"hi\r\n".to_vec()));
        assert!(rx.contains('←'), "{rx}");
        assert!(rx.contains("demo/up"), "{rx}");
        assert!(rx.contains("4 B"), "{rx}");
        assert!(rx.contains("hi\\r\\n"), "{rx}");

        let tx = format_mqtt_data_row(1, MqttDirection::Tx, "demo/up", &Payload::from_vec(b"out".to_vec()));
        assert!(tx.contains('→'), "{tx}");

        let long = std::iter::repeat_n(b'x', 100).collect::<Vec<u8>>();
        let row = format_mqtt_data_row(1, MqttDirection::Rx, "demo/up", &Payload::from_vec(long));
        assert!(row.contains('…'), "{row}");
    }

    /// The selector index mapping mirrors `MqttQos::all`.
    #[test]
    fn selector_indices_map_to_qos() {
        for (index, qos) in MqttQos::all().iter().enumerate() {
            assert_eq!(qos_at(index as i32), *qos);
        }
        assert_eq!(qos_at(-1), MqttQos::AtMostOnce);
        assert_eq!(qos_at(99), MqttQos::AtMostOnce);
    }

    /// The subscriptions-list labels carry the topic and the QoS wire number.
    #[test]
    fn subscription_labels_show_topic_and_qos() {
        assert_eq!(subscription_label("demo/up", MqttQos::ExactlyOnce), "demo/up  (qos 2)");
    }
}
