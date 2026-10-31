//! The pty-pair proxy (unix): pty creation and the one relay thread per monitor
//! that moves bytes between the virtual port and the upstream, capturing
//! everything it relays as [`Event::Monitor`](crate::core::bus::Event::Monitor)
//! with direction + timestamps.
//!
//! A monitor owns two ends:
//! - the **app end**: a pty whose slave path (`/dev/ttys…`) the target app opens
//!   as its serial port; rcom holds the master;
//! - the **upstream end**: either a second pty ([`Upstream::VirtualLoopback`]) or
//!   a real serial port opened through the backend trait.
//!
//! Both master fds run non-blocking inside one `poll` loop on a dedicated thread
//! (the port layer's blocking-fd discipline: no async runtime needed, and stop
//! never hangs — the stop flag is checked once per poll round, so it takes
//! effect within the 20 ms poll timeout). The slave's termios is set to raw at
//! creation so the tty line discipline never echoes or line-buffers: the pty
//! must behave like a dumb wire, or the capture would double every byte the app
//! writes (the echo would come back as app TX again) and hold chunks until a
//! newline.
//!
//! Chunk granularity: each `read` is one capture event (≤ 8 KiB), matching the
//! nettools data-event shape.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use nix::unistd::{close, read, ttyname, write};

use crate::core::bus::{Event, EventBus, Payload};
use crate::core::monitor::{
    MonitorConfig, MonitorDirection, MonitorEvent, MonitorEventKind, Upstream,
};
use crate::core::port::{OpenPort, PortConfig, ReadEvent, SerialBackend};

/// How many bytes one read step picks up (the chunk granularity of the capture).
const READ_BUF: usize = 8 * 1024;

/// The poll timeout of the relay loop in ms: bounds the serial-upstream drain
/// pacing (a serial reader delivers over a channel, not a polled fd) and the
/// stop latency (the stop flag is checked every round).
const POLL_TIMEOUT_MS: i32 = 20;

/// How long the loop backs off when a pty reports "no slave attached"
/// (Linux `EIO` / macOS `POLLHUP`): keeps the "app closed its port" state from
/// becoming a busy spin, while staying responsive to fresh opens.
const NO_SLAVE_BACKOFF: Duration = Duration::from_millis(50);

/// The app end of a monitor: rcom's master handle plus the slave path the
/// target app opens. Dropping it closes both fds (and with them the pty).
///
/// The slave fd from `openpty` is **held open for the monitor's lifetime**
/// (never read, never written): on macOS/BSD, closing the last slave fd hangs
/// up the master→slave direction permanently — even after the target app
/// reopens the path, master writes never reach it (verified on darwin; the
/// Python repro in the ticket comment). Holding one slave open keeps the pty
/// alive both ways, and since rcom never reads from it, it cannot steal a byte
/// of the app's input — the input queue belongs to the master reads.
pub(crate) struct PtyEnd {
    master: RawFd,
    slave: RawFd,
    pub slave_path: String,
}

impl PtyEnd {
    /// Creates a pty pair and prepares it for dumb-wire duty: master
    /// non-blocking, slave termios raw (no echo, no line discipline). The
    /// original slave fd stays open (see the type doc); the target app opens
    /// the path alongside it.
    pub fn open() -> nix::Result<Self> {
        let pair = openpty(None, None)?;
        let (master, slave) = (pair.master, pair.slave);
        // Raw line discipline BEFORE the app ever opens the slave: a
        // canonical+echo tty would echo every app write back to the master and
        // line-buffer until `\n` — neither belongs on a serial wire.
        let mut termios = tcgetattr(slave)?;
        cfmakeraw(&mut termios);
        tcsetattr(slave, SetArg::TCSANOW, &termios)?;
        let slave_path = ttyname(slave)?.to_string_lossy().into_owned();
        fcntl(master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        Ok(Self { master, slave, slave_path })
    }
}

impl Drop for PtyEnd {
    fn drop(&mut self) {
        let _ = close(self.master);
        let _ = close(self.slave);
    }
}

/// The upstream end: a second pty, or the serial port's reader/writer handles
/// (the backend's own reader thread + writer queue — the monitor never runs a
/// second reader on the same fd).
pub(crate) enum UpstreamEnd {
    Pty(PtyEnd),
    Serial(OpenPort),
}

/// Builds the upstream end from the monitor config. The real-port open touches
/// the OS, so the hub calls this before spawning the relay: a bad port fails
/// the start command, not the relay thread.
pub(crate) fn make_upstream(
    config: &MonitorConfig,
    backend: &dyn SerialBackend,
) -> Result<UpstreamEnd, String> {
    match &config.upstream {
        Upstream::VirtualLoopback => PtyEnd::open()
            .map(UpstreamEnd::Pty)
            .map_err(|error| format!("create the loopback pty: {error}")),
        Upstream::Real { port, baud } => backend
            .open(port, &PortConfig::new(*baud))
            .map(UpstreamEnd::Serial)
            .map_err(|error| format!("open the upstream port {port}: {error}")),
    }
}

/// Spawns the relay thread. `stop` is the user's stop switch (checked once per
/// poll round; see [`POLL_TIMEOUT_MS`]).
pub(crate) fn spawn_relay(
    bus: EventBus,
    monitor_id: u64,
    app: PtyEnd,
    upstream: UpstreamEnd,
    stop: Arc<AtomicBool>,
) {
    let builder = std::thread::Builder::new().name(format!("rcom-monitor-{monitor_id}"));
    let spawned = builder.spawn(move || {
        let mut upstream = upstream;
        let result = relay_loop(&bus, monitor_id, &app, &mut upstream, &stop);
        drop(app);
        drop(upstream);
        // A user stop already carries the hub's terminal `Stopped` event; the
        // relay publishes only the endings the user did not cause.
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let reason = match result {
            Ok(()) => None,
            Err(error) => {
                bus.publish(Event::Monitor(MonitorEvent {
                    monitor_id,
                    timestamp: chrono::Local::now(),
                    kind: MonitorEventKind::Error { message: error.clone() },
                }));
                Some(error)
            }
        };
        bus.publish(Event::Monitor(MonitorEvent {
            monitor_id,
            timestamp: chrono::Local::now(),
            kind: MonitorEventKind::Stopped { reason },
        }));
        tracing::debug!(monitor = monitor_id, "relay thread ended");
    });
    if spawned.is_err() {
        tracing::error!(monitor = monitor_id, "relay thread spawn failed");
    }
}

/// The relay loop. Every chunk read from the app end is captured as
/// [`MonitorDirection::Tx`] and forwarded upstream; every chunk from the
/// upstream is captured as [`MonitorDirection::Rx`] and forwarded to the app.
fn relay_loop(
    bus: &EventBus,
    monitor_id: u64,
    app: &PtyEnd,
    upstream: &mut UpstreamEnd,
    stop: &AtomicBool,
) -> Result<(), String> {
    let upstream_fd: Option<RawFd> = match upstream {
        UpstreamEnd::Pty(pty) => Some(pty.master),
        UpstreamEnd::Serial(_) => None,
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // The poll set: the app master and the upstream pty when there is one.
        // A serial upstream is drained from its channel every round.
        let mut set = vec![PollFd::new(app.master, PollFlags::POLLIN)];
        if let Some(fd) = upstream_fd {
            set.push(PollFd::new(fd, PollFlags::POLLIN));
        }
        let ready = poll(&mut set, POLL_TIMEOUT_MS)
            .map_err(|error| format!("relay poll failed: {error}"))?;
        if ready > 0 {
            // App → upstream (the app transmitted).
            let app_events = set[0].revents().unwrap_or(PollFlags::empty());
            if app_events.intersects(PollFlags::POLLIN) {
                let relayed = read_side(
                    bus,
                    monitor_id,
                    app.master,
                    MonitorDirection::Tx,
                    &mut |chunk| forward_upstream(upstream, chunk, stop),
                    stop,
                )?;
                if !relayed {
                    // poll fired but read gave nothing: no slave is attached
                    // (macOS signals POLLIN|POLLHUP for a pty nobody opened).
                    // Back off so this state never spins; a fresh open or the
                    // next write wakes us either way.
                    std::thread::sleep(NO_SLAVE_BACKOFF);
                }
            } else if app_events.intersects(PollFlags::POLLHUP) {
                // The app closed the virtual port (or has not opened it yet):
                // keep the relay alive, back off so this state never spins.
                std::thread::sleep(NO_SLAVE_BACKOFF);
            }
            // Upstream pty → app (the app received).
            if let Some(fd) = upstream_fd {
                let up_events = set[1].revents().unwrap_or(PollFlags::empty());
                if up_events.intersects(PollFlags::POLLIN) {
                    let app_master = app.master;
                    let relayed = read_side(
                        bus,
                        monitor_id,
                        fd,
                        MonitorDirection::Rx,
                        &mut |chunk| write_fd_all(app_master, chunk, stop),
                        stop,
                    )?;
                    if !relayed {
                        std::thread::sleep(NO_SLAVE_BACKOFF);
                    }
                } else if up_events.intersects(PollFlags::POLLHUP) {
                    std::thread::sleep(NO_SLAVE_BACKOFF);
                }
            }
        }
        drain_serial_upstream(bus, monitor_id, app, upstream, stop)?;
    }
}

/// Reads every available chunk from `fd` (non-blocking) and, per chunk, captures
/// it as a `direction` event and hands it to `forward`. Returns whether any
/// byte was relayed (`false` = drained/empty: the caller backs off — poll can
/// signal POLLIN for a pty whose slave side nobody has open). `Err` ends the
/// relay.
fn read_side(
    bus: &EventBus,
    monitor_id: u64,
    fd: RawFd,
    direction: MonitorDirection,
    forward: &mut impl FnMut(&[u8]) -> Result<(), String>,
    stop: &AtomicBool,
) -> Result<bool, String> {
    let mut buf = [0u8; READ_BUF];
    let mut relayed_any = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match read(fd, &mut buf) {
            Ok(0) => return Ok(relayed_any), // drained
            Ok(n) => {
                relayed_any = true;
                let chunk = &buf[..n];
                bus.publish(Event::Monitor(MonitorEvent {
                    monitor_id,
                    timestamp: chrono::Local::now(),
                    kind: MonitorEventKind::Data {
                        direction,
                        bytes: Payload::from_vec(chunk.to_vec()),
                    },
                }));
                forward(chunk)?;
            }
            Err(nix::errno::Errno::EAGAIN) => return Ok(relayed_any),
            Err(nix::errno::Errno::EIO) => {
                // Linux: the slave side is closed ("no one is attached"). Keep
                // the relay alive for the next open; the caller's back-off keeps
                // this from spinning.
                std::thread::sleep(NO_SLAVE_BACKOFF);
                return Ok(relayed_any);
            }
            Err(error) => return Err(format!("relay read failed: {error}")),
        }
    }
}

/// Forwards a chunk to the upstream end: serial goes through the backend's
/// writer queue, a pty through the bounded non-blocking write helper.
fn forward_upstream(
    upstream: &UpstreamEnd,
    chunk: &[u8],
    stop: &AtomicBool,
) -> Result<(), String> {
    match upstream {
        UpstreamEnd::Pty(pty) => write_fd_all(pty.master, chunk, stop),
        UpstreamEnd::Serial(open) => open.writer.try_write(chunk.to_vec()).map_err(|_| {
            "the upstream port's writer is gone (port closed)".to_string()
        }),
    }
}

/// Serial upstreams deliver through the backend reader's channel: drain it here
/// (pty upstreams leave this empty). Each `ReadEvent::Data` is captured as the
/// app's RX and written to the app's master; Error/Closed end the relay.
fn drain_serial_upstream(
    bus: &EventBus,
    monitor_id: u64,
    app: &PtyEnd,
    upstream: &mut UpstreamEnd,
    stop: &AtomicBool,
) -> Result<(), String> {
    let UpstreamEnd::Serial(open) = upstream else { return Ok(()) };
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match open.events.try_recv() {
            Ok(ReadEvent::Data(bytes)) => {
                bus.publish(Event::Monitor(MonitorEvent {
                    monitor_id,
                    timestamp: chrono::Local::now(),
                    kind: MonitorEventKind::Data {
                        direction: MonitorDirection::Rx,
                        bytes: Payload::from_vec(bytes.clone()),
                    },
                }));
                write_fd_all(app.master, &bytes, stop)?;
            }
            Ok(ReadEvent::Error(error)) => {
                return Err(format!("the upstream port reader died: {error}"));
            }
            Ok(ReadEvent::Closed) => {
                return Err("the upstream port was closed".into());
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return Err("the upstream port reader is gone".into());
            }
        }
    }
}

/// Writes the whole `chunk` to a non-blocking fd, waiting for writability on
/// `EAGAIN` (bounded polls, stop-checked) — a pty master's buffer is small, and
/// a half-relayed chunk would corrupt the stream.
fn write_fd_all(fd: RawFd, chunk: &[u8], stop: &AtomicBool) -> Result<(), String> {
    let mut written = 0usize;
    while written < chunk.len() {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match write(fd, &chunk[written..]) {
            Ok(0) => {}
            Ok(n) => written += n,
            Err(nix::errno::Errno::EAGAIN) => {
                // Bounded wait for buffer room; the stop check above is the
                // cancel (one poll round at most is lost to a stop).
                let mut wait = [PollFd::new(fd, PollFlags::POLLOUT)];
                let _ = poll(&mut wait, 500);
            }
            Err(error) => return Err(format!("relay write failed: {error}")),
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    /// The pty pair comes up as a usable virtual port: the slave path exists and
    /// a plain `File` open of it (the "target app") can round-trip bytes through
    /// the master — with no echo doubling (the raw termios is the point).
    #[test]
    fn pty_pair_is_a_dumb_wire() {
        let pty = PtyEnd::open().expect("openpty must work on this platform");
        assert!(pty.slave_path.starts_with("/dev/tty"), "{}", pty.slave_path);
        assert!(std::path::Path::new(&pty.slave_path).exists());

        let mut app = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pty.slave_path)
            .expect("the target app must be able to open the virtual port");
        app.write_all(b"ping").unwrap();
        let mut buf = [0u8; 16];
        let n = read(pty.master, &mut buf).expect("master must read the app's bytes");
        assert_eq!(&buf[..n], b"ping");

        write(pty.master, b"pong").unwrap();
        let n = app.read(&mut buf).expect("the app must read the master's bytes");
        assert_eq!(&buf[..n], b"pong");
    }

    /// Collects `Data` capture events until their concatenation (in `direction`)
    /// starts with `prefix` or the deadline passes — robust to the tty splitting
    /// one logical write across reads.
    fn wait_capture(
        events: &mut tokio::sync::broadcast::Receiver<Event>,
        monitor_id: u64,
        direction: MonitorDirection,
        prefix: &[u8],
    ) -> Option<MonitorEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pending: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            while let Ok(event) = events.try_recv() {
                if let Event::Monitor(monitor) = event {
                    if monitor.monitor_id != monitor_id {
                        continue;
                    }
                    if let MonitorEventKind::Data { direction: dir, bytes, .. } = &monitor.kind {
                        if *dir != direction {
                            continue;
                        }
                        pending.extend_from_slice(bytes.as_slice());
                        if pending.starts_with(prefix) {
                            return Some(monitor);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// The full relay in loopback mode: the "app" and the "device" open their
    /// pty slaves with plain File I/O; bytes cross the relay in both directions
    /// byte-exact, and every capture carries its direction and a timestamp
    /// (the AC's "captures with direction + timestamps").
    #[test]
    fn relay_moves_bytes_both_ways_and_captures_them() {
        let bus = EventBus::new();
        let app_pty = PtyEnd::open().unwrap();
        let dev_pty = PtyEnd::open().unwrap();
        let app_path = app_pty.slave_path.clone();
        let dev_path = dev_pty.slave_path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        spawn_relay(
            bus.clone(),
            9,
            app_pty,
            UpstreamEnd::Pty(dev_pty),
            Arc::clone(&stop),
        );

        let mut events = bus.subscribe();
        let mut app = OpenOptions::new().read(true).write(true).open(&app_path).unwrap();
        let mut device = OpenOptions::new().read(true).write(true).open(&dev_path).unwrap();

        // App → device: the app writes, the relay captures TX, the device reads.
        app.write_all(b"hello-device").unwrap();
        let tx = wait_capture(&mut events, 9, MonitorDirection::Tx, b"hello-device")
            .expect("the app's write must be captured as TX");
        assert!(tx.timestamp <= chrono::Local::now(), "capture carries a real timestamp");
        let expected = b"hello-device";
        let mut seen = vec![0u8; expected.len()];
        device.read_exact(&mut seen).expect("device must read the relayed bytes");
        assert_eq!(seen, expected, "the relay must deliver the app's bytes 1:1");

        // Device → app: the device replies, the relay captures RX, the app reads.
        device.write_all(b"hello-app").unwrap();
        let rx = wait_capture(&mut events, 9, MonitorDirection::Rx, b"hello-app")
            .expect("the device's reply must be captured as RX");
        assert!(rx.timestamp >= tx.timestamp, "captures are ordered by their stamps");
        let mut back = [0u8; b"hello-app".len()];
        app.read_exact(&mut back).expect("app must read the relayed reply");
        assert_eq!(&back, b"hello-app");

        // The user stop ends the relay promptly and silently: the hub owns the
        // terminal `Stopped` event on user stops, so the thread publishes
        // neither `Stopped` nor `Error` here.
        stop.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(200));
        while let Ok(event) = events.try_recv() {
            if let Event::Monitor(monitor) = event {
                if monitor.monitor_id == 9 {
                    assert!(
                        !matches!(
                            monitor.kind,
                            MonitorEventKind::Stopped { .. } | MonitorEventKind::Error { .. }
                        ),
                        "a user-stopped relay must stay silent (the hub publishes the terminal event)"
                    );
                }
            }
        }
    }

    /// The AC's "an app talking to a virtual port": an **external process**
    /// (`sh`) opens the virtual port and writes to it; the relay captures its
    /// bytes as app-TX with a timestamp and delivers them to the device side.
    #[test]
    fn relay_captures_an_external_process() {
        let bus = EventBus::new();
        let app_pty = PtyEnd::open().unwrap();
        let dev_pty = PtyEnd::open().unwrap();
        let app_path = app_pty.slave_path.clone();
        let dev_path = dev_pty.slave_path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        spawn_relay(
            bus.clone(),
            11,
            app_pty,
            UpstreamEnd::Pty(dev_pty),
            Arc::clone(&stop),
        );

        let mut events = bus.subscribe();
        let mut device = OpenOptions::new().read(true).write(true).open(&dev_path).unwrap();
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf 'ext-app-says-hi' > {app_path}"))
            .status()
            .expect("sh must be available");
        assert!(status.success(), "the external app must be able to write the port");

        let tx = wait_capture(&mut events, 11, MonitorDirection::Tx, b"ext-app-says-hi")
            .expect("the external process's bytes must be captured as TX");
        assert!(tx.timestamp <= chrono::Local::now(), "capture carries a real timestamp");
        let expected = b"ext-app-says-hi";
        let mut seen = vec![0u8; expected.len()];
        device.read_exact(&mut seen).expect("device must read the relayed bytes");
        assert_eq!(seen, expected, "the relay must deliver the external app's bytes 1:1");
        stop.store(true, Ordering::Relaxed);
    }
}
