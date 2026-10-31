//! One MQTT session worker per instance: drives the [`rumqttc`] async event
//! loop and reports progress as [`Event::Mqtt`] bus events.
//!
//! Every session is one supervisor task ended through a `watch`-based cancel
//! flag (the session layer's cancel-by-drop rule). Stopping is graceful: the
//! cancel path sends the MQTT DISCONNECT (a clean disconnect per 3.1.1) before
//! the worker exits, and the hub publishes the terminal [`MqttEventKind::Disconnected`].
//! Event-loop errors are NOT terminal — rumqttc retries the connection on the
//! next poll; the worker backs off a beat and de-duplicates the error events,
//! so a broker that is down neither floods the bus nor spins the runtime.

use std::sync::Arc;
use std::time::Duration;

// `rumqttc::Event` is the event-loop's; the app's bus event is `bus::Event`
// (and `super::MqttEvent`), so the loop event is aliased apart here.
use rumqttc::{AsyncClient, ConnectionError, Event as LoopEvent, MqttOptions, Packet, QoS, Transport, TlsConfiguration};
use tokio::sync::{mpsc, watch};

use crate::core::bus::{Event, EventBus, Payload};

use super::{MqttConfig, MqttDirection, MqttEvent, MqttEventKind, MqttQos};

/// The keep-alive the tool proposes (test-tool responsiveness over the rumqttc
/// default of 60 s).
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The pause after an event-loop error before the next poll (rumqttc retries
/// the connection per poll; without this, a refusing broker spins).
const RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// The bounded poll that flushes the queued DISCONNECT on stop (one eventloop
/// poll writes and flushes it — see the cancel arm).
const DISCONNECT_FLUSH: Duration = Duration::from_millis(500);

/// The command half of a session: publish/subscribe/unsubscribe requests from
/// the hub. Kept by the hub; a closed channel means the session is gone.
pub(crate) type CmdTx = mpsc::UnboundedSender<MqttCmd>;

/// One request into the session worker.
#[derive(Debug, Clone)]
pub(crate) enum MqttCmd {
    Publish { topic: String, payload: Vec<u8>, qos: MqttQos, retain: bool },
    Subscribe { topic: String, qos: MqttQos },
    Unsubscribe { topic: String },
}

/// The cancel flag of one session: `true` ends every task it reached.
#[derive(Clone)]
pub(crate) struct Cancel(Arc<watch::Sender<bool>>);

impl Cancel {
    pub(crate) fn new() -> (Self, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        (Self(Arc::new(tx)), rx)
    }

    pub(crate) fn cancel(&self) {
        let _ = self.0.send(true);
    }

    /// Whether the flag was raised (the supervisor then leaves the terminal
    /// `Disconnected` event to the hub that stopped the session).
    pub(crate) fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// A fresh receiver of the cancel flag.
    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// Resolves once the flag flips to `true` (or the sender goes away).
async fn cancelled(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Starts a session worker: spawns the supervisor through `runtime` (the hub
/// calls this from the UI thread, outside any runtime context) and returns its
/// command sender and cancel flag. Progress lands on the bus as [`Event::Mqtt`]
/// with `session_id`; the hub publishes [`MqttEventKind::Disconnected`] when it
/// stops the session itself.
pub(crate) fn start(
    bus: EventBus,
    session_id: u64,
    config: MqttConfig,
    runtime: &tokio::runtime::Handle,
) -> (CmdTx, Cancel) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<MqttCmd>();
    let (cancel, _cancel_rx) = Cancel::new();
    runtime.spawn(supervise(bus, session_id, config, cmd_rx, cancel.clone()));
    (cmd_tx, cancel)
}

/// Builds the rumqttc options for a session (the TLS material is resolved here,
/// eagerly — a broken setup fails in the command, not silently on the loop).
fn build_options(config: &MqttConfig) -> Result<MqttOptions, String> {
    let mut options = MqttOptions::new(config.client_id.clone(), config.host.clone(), config.port);
    options.set_keep_alive(KEEP_ALIVE);
    options.set_clean_session(true);
    if let Some(username) = config.username.as_deref().filter(|name| !name.is_empty()) {
        options.set_credentials(username, config.password.clone().unwrap_or_default());
    }
    if config.tls {
        // The nettools client config: standard webpki verification, or the
        // explicit accept-any debugging toggle (self-signed brokers included).
        let rustls_config = crate::core::nettools::tls::client_config(config.accept_any_cert)?;
        options.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(rustls_config)));
    }
    Ok(options)
}

/// The per-session supervisor: connection management, then the event pump.
async fn supervise(
    bus: EventBus,
    session_id: u64,
    config: MqttConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<MqttCmd>,
    cancel: Cancel,
) {
    let label = config.label();
    let result = run(&bus, session_id, &config, &mut cmd_rx, cancel.clone()).await;
    cancel.cancel();
    // A session stopped by its owner already carries the owner's terminal
    // `Disconnected` event; the supervisor publishes only the endings the owner
    // did not cause (command channel closed = the hub is gone).
    if cancel.is_cancelled() {
        return;
    }
    if let Err(error) = &result {
        bus.publish(Event::Mqtt(MqttEvent {
            session_id,
            timestamp: chrono::Local::now(),
            kind: MqttEventKind::Error { message: error.clone() },
        }));
        tracing::warn!(session = session_id, "{label}: {error}");
    }
    bus.publish(Event::Mqtt(MqttEvent {
        session_id,
        timestamp: chrono::Local::now(),
        kind: MqttEventKind::Disconnected { reason: None },
    }));
    tracing::info!(session = session_id, "{label}: session closed");
}

type RunResult = Result<(), String>;

async fn run(
    bus: &EventBus,
    session_id: u64,
    config: &MqttConfig,
    cmd_rx: &mut mpsc::UnboundedReceiver<MqttCmd>,
    cancel: Cancel,
) -> RunResult {
    let options = build_options(config)?;
    let server = format!("{}:{}", config.host, config.port);
    // Roomy request queue: publishes typed while offline wait here instead of
    // erroring the senders.
    let (client, mut eventloop) = AsyncClient::new(options, 64);

    // The live subscriptions, re-applied after every (re)connect: with
    // `clean_session` the broker drops the session state, so the SUBSCRIBEs
    // must go out again per ConnAck.
    let mut subscriptions: Vec<(String, MqttQos)> = Vec::new();
    // The last error surfaced on the bus; the next distinct one is published.
    let mut reported_error: Option<String> = None;

    loop {
        tokio::select! {
            _ = cancelled(cancel.subscribe()) => {
                // Graceful MQTT disconnect: rumqttc's `disconnect` only queues
                // the packet — the event loop writes it on its next poll, so
                // give it one bounded poll before the loop is dropped
                // (dropping right away would lose the queued DISCONNECT).
                let _ = client.disconnect().await;
                let _ = tokio::time::timeout(DISCONNECT_FLUSH, eventloop.poll()).await;
                return Ok(());
            }
            command = cmd_rx.recv() => match command {
                None => return Ok(()), // the hub dropped the sender
                Some(MqttCmd::Publish { topic, payload, qos, retain }) => {
                    if let Err(error) = client
                        .publish(&topic, qos_of(qos), retain, payload.clone())
                        .await
                    {
                        tracing::debug!(session = session_id, "publish to {topic} failed: {error}");
                    }
                    publish_message(
                        bus,
                        session_id,
                        MqttDirection::Tx,
                        &topic,
                        &payload,
                        qos,
                        retain,
                    );
                }
                Some(MqttCmd::Subscribe { topic, qos }) => {
                    match client.subscribe(&topic, qos_of(qos)).await {
                        Ok(()) => {
                            subscriptions.retain(|(existing, _)| *existing != topic);
                            subscriptions.push((topic.clone(), qos));
                            bus.publish(Event::Mqtt(MqttEvent {
                                session_id,
                                timestamp: chrono::Local::now(),
                                kind: MqttEventKind::Subscribed { topic, qos },
                            }));
                        }
                        Err(error) => {
                            let message = format!("subscribe {topic} failed: {error}");
                            tracing::debug!(session = session_id, "{message}");
                            bus.publish(Event::Mqtt(MqttEvent {
                                session_id,
                                timestamp: chrono::Local::now(),
                                kind: MqttEventKind::Error { message },
                            }));
                        }
                    }
                }
                Some(MqttCmd::Unsubscribe { topic }) => {
                    let _ = client.unsubscribe(&topic).await;
                    subscriptions.retain(|(existing, _)| *existing != topic);
                    bus.publish(Event::Mqtt(MqttEvent {
                        session_id,
                        timestamp: chrono::Local::now(),
                        kind: MqttEventKind::Unsubscribed { topic },
                    }));
                }
            },
            polled = eventloop.poll() => match polled {
                Ok(LoopEvent::Incoming(Packet::ConnAck(_))) => {
                    // A refused CONNACK never arrives as an event — rumqttc
                    // turns it into a poll error (the arm below).
                    reported_error = None;
                    publish_connected(bus, session_id, &server);
                    // Clean session on the broker side: re-subscribe everything.
                    for (topic, qos) in &subscriptions {
                        let _ = client.subscribe(topic, qos_of(*qos)).await;
                    }
                }
                Ok(LoopEvent::Incoming(Packet::Publish(incoming))) => {
                    publish_message(
                        bus,
                        session_id,
                        MqttDirection::Rx,
                        &incoming.topic,
                        incoming.payload.as_ref(),
                        MqttQos::from_wire(incoming.qos as u8),
                        incoming.retain,
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    let message = error_message(&error);
                    if reported_error.as_deref() != Some(message.as_str()) {
                        reported_error = Some(message.clone());
                        bus.publish(Event::Mqtt(MqttEvent {
                            session_id,
                            timestamp: chrono::Local::now(),
                            kind: MqttEventKind::Error { message },
                        }));
                    }
                    tracing::debug!(session = session_id, "event loop: {error}");
                    // Back off before the retry poll (which re-dials).
                    tokio::select! {
                        _ = cancelled(cancel.subscribe()) => {
                            let _ = client.disconnect().await;
                            let _ = tokio::time::timeout(DISCONNECT_FLUSH, eventloop.poll()).await;
                            return Ok(());
                        }
                        _ = tokio::time::sleep(RETRY_BACKOFF) => {}
                    }
                }
            },
        }
    }
}

/// The user-facing error line (rumqttc's `Display` covers every variant,
/// including the refused-CONNACK codes).
fn error_message(error: &ConnectionError) -> String {
    error.to_string()
}

fn qos_of(qos: MqttQos) -> QoS {
    match qos {
        MqttQos::AtMostOnce => QoS::AtMostOnce,
        MqttQos::AtLeastOnce => QoS::AtLeastOnce,
        MqttQos::ExactlyOnce => QoS::ExactlyOnce,
    }
}

fn publish_connected(bus: &EventBus, session_id: u64, server: &str) {
    bus.publish(Event::Mqtt(MqttEvent {
        session_id,
        timestamp: chrono::Local::now(),
        kind: MqttEventKind::Connected { server: server.to_string() },
    }));
}

fn publish_message(
    bus: &EventBus,
    session_id: u64,
    direction: MqttDirection,
    topic: &str,
    payload: &[u8],
    qos: MqttQos,
    retain: bool,
) {
    bus.publish(Event::Mqtt(MqttEvent {
        session_id,
        timestamp: chrono::Local::now(),
        kind: MqttEventKind::Message {
            direction,
            topic: topic.to_string(),
            payload: Payload::from_vec(payload.to_vec()),
            qos,
            retain,
        },
    }));
}
