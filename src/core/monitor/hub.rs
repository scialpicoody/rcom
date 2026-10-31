//! The monitor hub: the one handle that owns every monitor instance. The monitor
//! page drives it from the UI thread (its methods are sync), the
//! `RCOM_MONITORDEMO` demo uses it end to end, and its housekeeping task keeps
//! the registry in step with the relay threads via the bus.
//!
//! The hub holds the port backend so it can offer the real-port upstream list to
//! the page and open upstream ports for real-port monitors (its own handle —
//! deliberately **not** the session machinery: a monitored port is not a
//! rcom session, it must not fight the reconnect machine or the TX hook).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::bus::{Event, EventBus};
use crate::core::monitor::{MonitorConfig, MonitorEvent, MonitorEventKind, Upstream};
use crate::core::port::{PortInfo, SerialBackend};

/// One registered monitor instance.
struct MonitorEntry {
    stop: Arc<AtomicBool>,
    /// The path the target app opens.
    app_port: String,
    /// The device-side twin path (loopback only — the upstream pty's slave).
    peer_port: Option<String>,
}

/// The registry + factory of monitors. Created inside the tokio runtime (the
/// housekeeping task needs it); handed out as `Arc<MonitorHub>` — see [`global`].
pub struct MonitorHub {
    bus: EventBus,
    backend: Arc<dyn SerialBackend>,
    next_id: AtomicU64,
    monitors: Mutex<HashMap<u64, MonitorEntry>>,
}

impl MonitorHub {
    /// Creates the hub and starts its bus housekeeping. Call from inside the
    /// tokio runtime. The backend is shared with the port manager (same
    /// enumeration, same mock seam); opening a port for the monitor's upstream
    /// is a raw `SerialBackend::open`, independent of any session.
    pub fn new(bus: EventBus, backend: Arc<dyn SerialBackend>) -> Arc<Self> {
        let hub = Arc::new(Self {
            bus,
            backend,
            next_id: AtomicU64::new(1),
            monitors: Mutex::new(HashMap::new()),
        });
        start_housekeeping(&hub);
        hub
    }

    /// Starts a monitor: creates the virtual port(s), opens a real upstream when
    /// asked, spawns the relay thread and publishes the `Started` event. Returns
    /// the monitor id and the virtual port path the target app opens.
    pub fn start_monitor(&self, config: MonitorConfig) -> Result<(u64, String), String> {
        self.start_monitor_inner(config)
    }

    /// Stops a monitor (a no-op for unknown ids). Publishes the terminal
    /// `Stopped` event itself — the relay thread stays silent on user stops.
    pub fn stop_monitor(&self, id: u64) {
        let entry = self.monitors.lock().expect("monitors mutex").remove(&id);
        if let Some(entry) = entry {
            entry.stop.store(true, Ordering::Relaxed);
            self.publish_stopped(id, None);
        }
    }

    /// Stops every monitor (app shutdown).
    pub fn stop_all(&self) {
        let ids: Vec<u64> =
            self.monitors.lock().expect("monitors mutex").keys().copied().collect();
        for id in ids {
            self.stop_monitor(id);
        }
    }

    /// The live monitor ids (oldest first).
    pub fn monitor_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> =
            self.monitors.lock().expect("monitors mutex").keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Whether a monitor is alive.
    pub fn is_running(&self, id: u64) -> bool {
        self.monitors.lock().expect("monitors mutex").contains_key(&id)
    }

    /// The virtual port paths of a running monitor: the one the target app
    /// opens, and (loopback only) the device-side twin.
    pub fn virtual_ports(&self, id: u64) -> Option<(String, Option<String>)> {
        self.monitors
            .lock()
            .expect("monitors mutex")
            .get(&id)
            .map(|entry| (entry.app_port.clone(), entry.peer_port.clone()))
    }

    /// The ports the upstream selector offers right now (real ports + the mock
    /// seam, the same listing the main toolbar shows).
    pub fn list_ports(&self) -> Result<Vec<PortInfo>, String> {
        self.backend.enumerate().map_err(|error| error.to_string())
    }

    fn publish_stopped(&self, id: u64, reason: Option<String>) {
        self.bus.publish(Event::Monitor(MonitorEvent {
            monitor_id: id,
            timestamp: chrono::Local::now(),
            kind: MonitorEventKind::Stopped { reason },
        }));
    }
}

impl Drop for MonitorHub {
    fn drop(&mut self) {
        // A dropped hub leaves no relay threads behind (the registry is dead
        // anyway; their terminal events go nowhere).
        for (_, entry) in self.monitors.get_mut().expect("monitors mutex").drain() {
            entry.stop.store(true, Ordering::Relaxed);
        }
    }
}

static GLOBAL: std::sync::OnceLock<Arc<MonitorHub>> = std::sync::OnceLock::new();

/// Installs the process-global hub (call once, inside the tokio runtime) and
/// starts its bus housekeeping. [`global`] hands the Arc out from then on.
pub fn init_global(bus: EventBus, backend: Arc<dyn SerialBackend>) -> Arc<MonitorHub> {
    let hub = MonitorHub::new(bus, backend);
    let _ = GLOBAL.set(Arc::clone(&hub));
    hub
}

/// The process-global hub, once installed.
pub fn global() -> Option<&'static Arc<MonitorHub>> {
    GLOBAL.get()
}

#[cfg(unix)]
impl MonitorHub {
    /// The unix implementation: pty creation is real.
    fn start_monitor_inner(&self, config: MonitorConfig) -> Result<(u64, String), String> {
        // The app end first: its slave path is the virtual port the page shows.
        let app = crate::core::monitor::pty::PtyEnd::open()
            .map_err(|error| format!("create the virtual port: {error}"))?;
        let app_port = app.slave_path.clone();
        let peer_port;
        let upstream = crate::core::monitor::pty::make_upstream(&config, self.backend.as_ref())?;
        // A loopback twin: remember its slave path for the demo and the rows.
        peer_port = match &upstream {
            crate::core::monitor::pty::UpstreamEnd::Pty(pty) => Some(pty.slave_path.clone()),
            crate::core::monitor::pty::UpstreamEnd::Serial(_) => None,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        // Publish Started BEFORE the relay thread exists, so the page (and the
        // demo) see the virtual port before any capture event of this id.
        let upstream_label = match &config.upstream {
            Upstream::VirtualLoopback => {
                format!("loopback (virtual {})", peer_port.clone().unwrap_or_default())
            }
            other => other.label(),
        };
        self.bus.publish(Event::Monitor(MonitorEvent {
            monitor_id: id,
            timestamp: chrono::Local::now(),
            kind: MonitorEventKind::Started {
                virtual_port: app_port.clone(),
                upstream: upstream_label,
            },
        }));
        crate::core::monitor::pty::spawn_relay(
            self.bus.clone(),
            id,
            app,
            upstream,
            stop.clone(),
        );
        self.monitors.lock().expect("monitors mutex").insert(
            id,
            MonitorEntry { stop, app_port: app_port.clone(), peer_port },
        );
        Ok((id, app_port))
    }
}

#[cfg(not(unix))]
impl MonitorHub {
    /// The Windows story (issue #23, license verdict): the bundled
    /// serial_monitor.dll is closed-source with no license statement, so the FFI
    /// path is skipped — the pty proxy is unix-only. Revisit: build llcom's
    /// Apache-2.0 `serial_monitor_rs` rewrite from source instead.
    fn start_monitor_inner(&self, _config: MonitorConfig) -> Result<(u64, String), String> {
        Err("serial monitoring is not available on this platform \
             (Windows FFI skipped — license; pty proxy is unix-only)"
            .into())
    }
}

/// Watches the bus for the hub's lifetime (subscribed at creation, so no event
/// is missed): relay-ended `Stopped` events clean the registry. The task holds
/// the hub weakly — dropping the hub ends it.
fn start_housekeeping(hub: &Arc<MonitorHub>) {
    let weak = Arc::downgrade(hub);
    let mut events = hub.bus.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let Some(hub) = weak.upgrade() else { return };
                    if let Event::Monitor(monitor) = event {
                        if matches!(monitor.kind, MonitorEventKind::Stopped { .. }) {
                            hub.monitors
                                .lock()
                                .expect("monitors mutex")
                                .remove(&monitor.monitor_id);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}
