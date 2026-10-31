//! The MQTT test tool (docs/design.md, stack table; issue #21 — the llcom
//! MQTT-test parity slice): connect to a broker (raw TCP or TLS), subscribe,
//! publish, watch the traffic, and bridge topics ⇄ the serial session.
//!
//! Layout: [`client`] runs one worker per session over the [`rumqttc`] async
//! event loop and reports progress as [`Event::Mqtt`] on the bus
//! (docs/ui-drafts/05-contract.md §1.5: `Event::Mqtt(MqttEvent)` — the
//! provisional sketch, refined per this ticket); [`hub`] is the registry the
//! page and the bridge drive; [`bridge`] glues an MQTT session to the serial
//! session's RX/TX events; [`mockbroker`] is the minimal MQTT 3.1.1 broker
//! over a `TcpListener` (loopback tests and the `RCOM_MQTTDEMO` demo — no
//! external broker is ever required); [`demo`] is the demo itself.
//!
//! TLS reuses the nettools rustls stack ([`crate::core::nettools::tls`]): the
//! transport is [`rumqttc::TlsConfiguration::Rustls`] over the same client
//! config the net channels use — standard webpki verification, or the explicit
//! accept-any debugging toggle (llcom parity).
//!
//! Threading: everything here is core-side tokio; the UI reaches it through
//! [`hub::MqttHub`], whose methods are sync (they only spawn tasks on the
//! stored runtime handle) and therefore callable from the FerroUI thread.

pub mod bridge;
pub mod client;
pub mod demo;
pub mod hub;
pub mod mockbroker;

use chrono::{DateTime, Local};

use crate::core::bus::Payload;

/// The MQTT quality-of-service level of a subscription or publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttQos {
    AtMostOnce,
    AtLeastOnce,
    ExactlyOnce,
}

impl MqttQos {
    /// The wire number (0/1/2).
    pub fn wire(self) -> u8 {
        match self {
            MqttQos::AtMostOnce => 0,
            MqttQos::AtLeastOnce => 1,
            MqttQos::ExactlyOnce => 2,
        }
    }

    /// The QoS levels the page's selectors offer, in index order.
    pub fn all() -> [MqttQos; 3] {
        [MqttQos::AtMostOnce, MqttQos::AtLeastOnce, MqttQos::ExactlyOnce]
    }

    /// The selector index (`-1`/out-of-range maps to QoS 0, like the net page's
    /// selector fallback).
    pub fn from_index(index: i32) -> MqttQos {
        MqttQos::all().get(index.max(0) as usize).copied().unwrap_or(MqttQos::AtMostOnce)
    }

    /// From the wire number; out-of-range values clamp to QoS 0.
    pub fn from_wire(wire: u8) -> MqttQos {
        match wire & 0b11 {
            1 => MqttQos::AtLeastOnce,
            2 => MqttQos::ExactlyOnce,
            _ => MqttQos::AtMostOnce,
        }
    }
}

/// The full start configuration of one MQTT session.
#[derive(Debug, Clone)]
pub struct MqttConfig {
    /// Broker host or IP literal (rumqttc resolves names through its own
    /// network layer).
    pub host: String,
    pub port: u16,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Connect over TLS (the transport is built from the nettools rustls
    /// config; [`MqttConfig::accept_any_cert`] selects the verification mode).
    pub tls: bool,
    /// TLS only: accept any server certificate (self-signed included). A
    /// debugging tool (llcom parity); the toggle is explicit in the UI.
    pub accept_any_cert: bool,
}

impl MqttConfig {
    /// The `mqtt host:port` label used by log/page rows.
    pub fn label(&self) -> String {
        format!(
            "mqtt{} {}:{}",
            if self.tls { "+tls" } else { "" },
            self.host,
            self.port
        )
    }
}

/// The direction of a [`MqttEventKind::Message`] relative to the local client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttDirection {
    /// Received from the broker (an incoming publish).
    Rx,
    /// Published by the local client.
    Tx,
}

/// One thing that happened on an MQTT session (docs/ui-drafts/05-contract.md
/// §1.5: `Event::Mqtt(MqttEvent)` — the provisional sketch refined per this
/// ticket; the ConnAck/Disconnected/Message sketch became the kinds below).
#[derive(Debug, Clone)]
pub struct MqttEvent {
    /// The session instance this event belongs to (ids are process-unique,
    /// allocated by [`hub::MqttHub`], never reused).
    pub session_id: u64,
    pub timestamp: DateTime<Local>,
    pub kind: MqttEventKind,
}

/// The payload of a [`MqttEvent`].
#[derive(Debug, Clone)]
pub enum MqttEventKind {
    /// The broker accepted the CONNECT ([`rumqttc::Packet::ConnAck`] with the
    /// success code). Subscriptions are (re-)applied right after this event.
    Connected { server: String },
    /// The session ended (the user stopped it — the hub publishes this one and
    /// it is terminal for that session id; the graceful DISCONNECT has already
    /// gone out).
    Disconnected { reason: Option<String> },
    /// The event loop failed (broker unreachable, TLS handshake, CONNACK
    /// refused, …). NOT terminal: rumqttc retries the connection, and the next
    /// [`MqttEventKind::Connected`] clears the state. Published once per
    /// distinct message — a persistently refusing broker does not flood the
    /// bus.
    Error { message: String },
    /// One publish in one direction. TX events are emitted when the publish is
    /// accepted for delivery (QoS 0 goes out essentially at once; QoS 1/2 are
    /// rumqttc-managed afterwards, including reconnect retries).
    Message { direction: MqttDirection, topic: String, payload: Payload, qos: MqttQos, retain: bool },
    /// A subscribe request was queued for the broker (re-applied after every
    /// reconnect; the SUBACK is rumqttc-internal and not surfaced).
    Subscribed { topic: String, qos: MqttQos },
    /// An unsubscribe request was queued for the broker.
    Unsubscribed { topic: String },
}

/// A valid MQTT topic or filter, or the reason it is not (the hub validates
/// before a session starts and every page command).
pub fn validate_topic(topic: &str) -> Result<(), String> {
    let trimmed = topic.trim();
    if trimmed.is_empty() {
        return Err("empty topic".into());
    }
    if trimmed.len() > 65_535 {
        return Err("topic too long".into());
    }
    // `#` and `+` are only legal as whole levels (# only as the last one).
    for (index, level) in trimmed.split('/').enumerate() {
        let wildcard = level == "#" || level == "+";
        let contains = level.contains('#') || level.contains('+');
        if contains && !wildcard {
            return Err(format!("invalid wildcard in topic {trimmed:?}"));
        }
        if level == "#" && trimmed.split('/').nth(index + 1).is_some() {
            return Err(format!("'#' must be the last level in {trimmed:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Selector/wire index mapping round-trips; garbage clamps to QoS 0.
    #[test]
    fn qos_index_mapping() {
        for (index, qos) in MqttQos::all().iter().enumerate() {
            assert_eq!(MqttQos::from_index(index as i32), *qos);
            assert_eq!(MqttQos::from_wire(qos.wire()), *qos);
            assert_eq!(qos.wire() as usize, index);
        }
        assert_eq!(MqttQos::from_index(-1), MqttQos::AtMostOnce);
        assert_eq!(MqttQos::from_index(99), MqttQos::AtMostOnce);
        assert_eq!(MqttQos::from_wire(7), MqttQos::AtMostOnce);
    }

    /// Plain topics and legal wildcard filters pass; misplaced wildcards fail.
    #[test]
    fn topic_validation() {
        assert!(validate_topic("demo/hello").is_ok());
        assert!(validate_topic("a/+/b").is_ok());
        assert!(validate_topic("a/#").is_ok());
        assert!(validate_topic("#").is_ok());
        assert!(validate_topic("sensors/room1/temperature").is_ok());
        assert!(validate_topic("").is_err());
        assert!(validate_topic("  ").is_err());
        assert!(validate_topic("de#mo").is_err());
        assert!(validate_topic("de+mo/x").is_err());
        assert!(validate_topic("a/#/b").is_err());
    }
}
