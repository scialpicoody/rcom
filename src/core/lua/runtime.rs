//! The Lua runtime: one sandboxed VM running llcom's Luat task framework
//! (`sys.*`), with the host API (`api*` functions) llcom's scripts expect.
//!
//! Mechanism (ticket #18 spike, verified in the tests below):
//! - `sys.wait(ms)` suspends the task coroutine in `coroutine.yield`; the host's
//!   `apiStartTimer` armed a one-shot tokio timer that fires `tiggerCB(id)`,
//!   which `coroutine.resume`s the task. No busy waits; jitter is tokio timer
//!   precision.
//! - `sys.waitUntil`/`publish`/`subscribe` are VM-internal (llcom's `sys.lua`,
//!   unmodified) — nothing to implement on the Rust side.
//! - UART RX enters the VM as a channel message: `tiggerCB(-1, "uart", bytes)`,
//!   which runs the script's `uartReceive` callback. `apiSendUartData` /
//!   `apiSend("uart", ...)` go out through the [`UartSink`] (the session writer).
//! - Stops: one `stop` flag per runtime + mlua instruction-count hooks armed on
//!   whichever thread is about to run (Rust entry points arm the current thread;
//!   `head.lua` re-arms on every `coroutine.resume`). mlua keeps a single hook
//!   slot per VM and strips Lua-inherited hooks on coroutines, so arming the
//!   thread that runs *is* the design. Setting the flag unwinds any running Lua
//!   within HOOK_EVERY_NTH_INSTRUCTION instructions; pending timers are aborted.
//! - Errors never panic the app: user-script load errors and callback failures
//!   surface as `Event::ScriptOut(ScriptOutKind::Error)` bus events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{Function, HookTriggers, Lua, Table, Thread, VmState};

use crate::core::bus::{Event, EventBus, ScriptOutEvent, ScriptOutKind};

use super::json;
use super::sandbox;
use super::scripts;

/// Check the stop flag every this many VM instructions: cheap (a closure call
/// per bucket), yet a hard loop dies within milliseconds of `stop()`.
pub const HOOK_EVERY_NTH_INSTRUCTION: u32 = 250_000;
/// The send-conversion VM's watchdog (llcom `head.lua`: 3 s for `runType == "send"`).
pub const SEND_VM_TIMEOUT: Duration = Duration::from_secs(3);

/// Where `apiSend("uart", ...)` / `apiSendUartData(...)` deliver. The session
/// wires this to its port writer; tests collect into a buffer.
pub trait UartSink: Send + Sync {
    /// Returns whether the bytes were accepted (a closed port refuses them).
    fn send_uart(&self, bytes: Vec<u8>) -> bool;
}

/// A no-op sink for VMs without a serial session (standalone script panel).
pub struct DetachedSink;

impl UartSink for DetachedSink {
    fn send_uart(&self, _bytes: Vec<u8>) -> bool {
        false
    }
}

/// Arms the stop hook on `thread`: from here on, `stop()` errors that thread's
/// execution within HOOK_EVERY_NTH_INSTRUCTION instructions.
pub(crate) fn arm_stop_hook_on(thread: &Thread, stop: Arc<AtomicBool>) {
    thread.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_EVERY_NTH_INSTRUCTION),
        move |_, _| {
            if stop.load(Ordering::Relaxed) {
                Err(mlua::Error::runtime("script stopped"))
            } else {
                Ok(VmState::Continue)
            }
        },
    );
}

/// Everything shared between the runtime handle and the tokio timer tasks.
pub struct Inner {
    pub lua: Arc<Lua>,
    pub bus: EventBus,
    pub session_id: Option<u64>,
    pub stop: Arc<AtomicBool>,
    /// Live one-shot timers (llcom's `LuaRunEnv.pool`): `apiStartTimer` id → handle.
    pub timers: Mutex<HashMap<i64, tokio::task::AbortHandle>>,
    pub sink: Option<Arc<dyn UartSink>>,
}

impl Inner {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Arms the stop hook on whichever thread is about to run (every Rust→Lua
    /// entry point calls this on the way in).
    pub fn arm_stop_hook(&self) {
        arm_stop_hook_on(&self.lua.current_thread(), Arc::clone(&self.stop));
    }

    /// Fires `tiggerCB(id, channel, data)` — the single bridge Rust→Lua for
    /// timers and channel messages. Byte payloads go in as Lua *strings*
    /// (mlua turns a bare `Vec<u8>` into an array table, which is not what
    /// llcom scripts expect). Lua-side pcalls turn script errors into
    /// `log.error` output; anything escaping still becomes a bus error event.
    pub fn trigger(&self, id: i64, channel: &str, data: Option<Vec<u8>>) {
        if self.stopped() {
            return;
        }
        self.arm_stop_hook();
        let payload: mlua::Result<mlua::Value> = match &data {
            Some(bytes) => self.lua.create_string(bytes).map(mlua::Value::String),
            None => Ok(mlua::Value::Nil),
        };
        let result = match (self.lua.globals().get::<Function>("tiggerCB"), payload) {
            (Ok(cb), Ok(data)) => cb.call::<()>((id, channel.to_string(), data)),
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
        if let Err(e) = result {
            self.report_error(format!("lua trigger failed: {e}"));
        }
    }

    /// Publishes a script error onto the bus (the AC: script errors surface in
    /// the log, never panic the app).
    pub fn report_error(&self, message: String) {
        self.bus.publish(Event::ScriptOut(ScriptOutEvent {
            session_id: self.session_id,
            timestamp: chrono::Local::now(),
            kind: ScriptOutKind::Error { message },
        }));
    }

    pub fn publish_print(&self, line: String) {
        self.bus.publish(Event::ScriptOut(ScriptOutEvent {
            session_id: self.session_id,
            timestamp: chrono::Local::now(),
            kind: ScriptOutKind::Print { line },
        }));
    }

    /// `apiStartTimer` (llcom `LuaRunEnv::StartTimer`): one-shot tokio timer that
    /// fires `tiggerCB(id, "timer")` on expiry. Re-arming an id cancels the old
    /// timer first. Returns success like llcom's C `apiStartTimer` (1/0).
    pub fn start_timer(self: &Arc<Self>, id: i64, ms: u64) -> bool {
        if self.stopped() || ms == 0 {
            return false;
        }
        let mut timers = self.timers.lock().unwrap();
        if let Some(old) = timers.remove(&id) {
            old.abort();
        }
        let runtime = Arc::clone(self);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            runtime.timers.lock().unwrap().remove(&id);
            runtime.trigger(id, "timer", None);
        });
        timers.insert(id, handle.abort_handle());
        true
    }

    /// `apiStopTimer`: cancels a pending timer. Unknown ids are a no-op (llcom's
    /// StopTimer just clears the token if present).
    pub fn stop_timer(&self, id: i64) {
        if let Some(handle) = self.timers.lock().unwrap().remove(&id) {
            handle.abort();
        }
    }

    /// Aborts every pending timer and stops the VM for good.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut timers = self.timers.lock().unwrap();
        for (_, handle) in timers.drain() {
            handle.abort();
        }
    }
}

/// A running Lua VM. Cloneable handle; the VM lives as long as any handle, timer
/// task or in-flight call holds it. Create with [`LuaRuntime::start`].
#[derive(Clone)]
pub struct LuaRuntime {
    inner: Arc<Inner>,
}

impl LuaRuntime {
    /// Creates the VM: sandbox + host API + `head.lua`. The returned handle's
    /// [`stop`](LuaRuntime::stop) works from this point on — even while
    /// [`run_script`](LuaRuntime::run_script) is still unwinding a hard loop —
    /// which is why this is split from `start` (a script that spins forever at
    /// load must not take the stop switch with it).
    pub fn create(bus: EventBus, session_id: Option<u64>, sink: Option<Arc<dyn UartSink>>) -> mlua::Result<Self> {
        let lua = Arc::new(Lua::new());
        sandbox::apply(&lua)?;
        install_preloads(&lua)?;
        let inner = Arc::new(Inner {
            lua: Arc::clone(&lua),
            bus,
            session_id,
            stop: Arc::new(AtomicBool::new(false)),
            timers: Mutex::new(HashMap::new()),
            sink,
        });
        install_host_api(&lua, Arc::clone(&inner))?;
        // The `rcom.net` one-shot table (issue #20), next to the api* builtins.
        // Functions resolve the process-global net hub lazily; without one they
        // still run (one-shots are hub-independent).
        crate::core::nettools::lua_api::install(&lua)?;
        // The `rcom.ui` plot table (issue #24): the `add_point` tap the decoders
        // (and any script) use; merges into the same `rcom` global.
        crate::core::plot::lua_api::install(&lua)?;

        // Script VMs announce themselves, then head.lua sets up print/log/sys/…
        lua.globals().set("runType", "script")?;
        lua.load(scripts::HEAD_LUA).set_name("=head").exec()?;
        Ok(Self { inner })
    }

    /// Creates the VM and runs `script` in one go — the one-shot shape the demo
    /// and tests use (ticket #19 will hold the `create`d handle instead). A
    /// script error stops the VM (llcom parity: a failed start means nothing
    /// keeps half-running), publishes the error on the bus and returns it.
    pub fn start(
        bus: EventBus,
        session_id: Option<u64>,
        sink: Option<Arc<dyn UartSink>>,
        script: &str,
    ) -> mlua::Result<Self> {
        let runtime = Self::create(bus, session_id, sink)?;
        runtime.run_script(script)?;
        Ok(runtime)
    }

    /// Runs the user/panel script chunk in this VM. Re-runnable (the script
    /// panel's run button), but a VM that was `stop()`ed stays stopped.
    pub fn run_script(&self, script: &str) -> mlua::Result<()> {
        if self.is_stopped() {
            return Err(mlua::Error::runtime("script stopped"));
        }
        self.inner.arm_stop_hook();
        if let Err(e) = self.inner.lua.load(script).set_name("=script").exec() {
            self.inner.stop();
            let message = format!("script error: {e}");
            self.inner.report_error(message.clone());
            return Err(mlua::Error::runtime(message));
        }
        Ok(())
    }

    /// Sends raw bytes to the VM's `uartReceive` path (the session reader calls
    /// this for every RX chunk).
    pub fn feed_uart(&self, bytes: &[u8]) {
        self.inner.trigger(-1, "uart", Some(bytes.to_vec()));
    }

    /// Whether `name` is a function global in this VM (the plot decoder host
    /// checks the `decode` contract, issue #24).
    pub fn has_function(&self, name: &str) -> bool {
        self.inner
            .lua
            .globals()
            .get::<Function>(name)
            .is_ok()
    }

    /// Calls the global function `name(bytes, timestamp_ms)` — the plot
    /// decoder seam (issue #24): the decoder host feeds every RX chunk into the
    /// script's `decode`. Unlike `tiggerCB`, a script error is returned (the
    /// host decides: a broken decoder stops, it does not spam).
    pub fn call_bytes_function(&self, name: &str, bytes: &[u8], timestamp_ms: f64) -> mlua::Result<()> {
        if self.is_stopped() {
            return Err(mlua::Error::runtime("script stopped"));
        }
        self.inner.arm_stop_hook();
        let function = self.inner.lua.globals().get::<Function>(name)?;
        let data = self.inner.lua.create_string(bytes)?;
        function.call::<()>((data, timestamp_ms))
    }

    /// Runs a line of Lua in the running VM (llcom's `RunCommand` console path).
    pub fn run_command(&self, line: &str) {
        self.inner.trigger(-1, "cmd", Some(line.as_bytes().to_vec()));
    }

    /// Publishes one `print`-style line as a script output event (used by tests
    /// and, later, the script panel).
    pub fn print(&self, line: impl Into<String>) {
        self.inner.publish_print(line.into());
    }

    /// Whether this VM has been stopped (errors and triggers are then ignored).
    pub fn is_stopped(&self) -> bool {
        self.inner.stopped()
    }

    /// Stops the VM: pending timers are aborted, running Lua unwinds, triggers
    /// are ignored. Never blocks and never panics, from any thread.
    pub fn stop(&self) {
        self.inner.stop();
    }
}

/// The host-side `api*` functions + preloaded modules (require is preload-only).
fn install_preloads(lua: &Lua) -> mlua::Result<()> {
    let preload: Table = lua.globals().get::<Table>("package")?.get("preload")?;
    preload.set("sys", lua.load(scripts::SYS_LUA).set_name("=sys").into_function()?)?;
    preload.set(
        "strings",
        lua.load(scripts::STRINGS_LUA).set_name("=strings").into_function()?,
    )?;
    preload.set("log", lua.load(scripts::LOG_LUA).set_name("=log").into_function()?)?;
    // package.preload holds loader FUNCTIONS: wrap the ready-made JSON table.
    let json_module = json::module(lua)?;
    preload.set(
        "JSON",
        lua.create_function(move |_lua, ()| Ok(json_module.clone()))?,
    )?;
    Ok(())
}

fn install_host_api(lua: &Lua, inner: Arc<Inner>) -> mlua::Result<()> {
    let globals = lua.globals();

    // apiStartTimer(id, ms) — arms a one-shot tokio timer (sys.lua's core hook).
    let start_inner = Arc::clone(&inner);
    globals.set(
        "apiStartTimer",
        lua.create_function(move |_, (id, ms): (i64, f64)| {
            let ms = if ms.is_finite() && ms > 0.0 { ms as u64 } else { 0 };
            Ok(if start_inner.start_timer(id, ms) { 1 } else { 0 })
        })?,
    )?;

    // apiStopTimer(id)
    let stop_inner = Arc::clone(&inner);
    globals.set(
        "apiStopTimer",
        lua.create_function(move |_, id: i64| {
            stop_inner.stop_timer(id);
            Ok(())
        })?,
    )?;

    // apiPrintLog(line) — the VM's overridden print lands here.
    let print_inner = Arc::clone(&inner);
    globals.set(
        "apiPrintLog",
        lua.create_function(move |_, line: mlua::String| {
            print_inner.publish_print(String::from_utf8_lossy(line.as_bytes().as_ref()).into_owned());
            Ok(())
        })?,
    )?;

    // apiSend(channel, data[, table]) — the uart channel writes to the session.
    let send_inner = Arc::clone(&inner);
    globals.set(
        "apiSend",
        lua.create_function(
            move |_, (channel, data, _extra): (String, mlua::String, Option<Table>)| {
                if send_inner.stopped() {
                    return Ok(false);
                }
                if channel == "uart" {
                    let bytes = data.as_bytes().as_ref().to_vec();
                    match &send_inner.sink {
                        Some(sink) => Ok(sink.send_uart(bytes)),
                        None => Ok(false),
                    }
                } else {
                    Ok(false)
                }
            },
        )?,
    )?;

    // apiGetPath / apiUtf8ToHex / apiAscii2Utf8 — llcom API surface shims.
    // rcom keeps scripts in-memory (require is preload-only), so the profile
    // path is empty; hex encoding covers UTF-8 bytes (GBK lands with encoding_rs).
    globals.set("apiGetPath", lua.create_function(|_, ()| Ok(String::new()))?)?;
    globals.set(
        "apiUtf8ToHex",
        lua.create_function(|_, text: String| Ok(hex_upper(text.as_bytes())))?,
    )?;
    globals.set(
        "apiAscii2Utf8",
        lua.create_function(|_, bytes: Vec<u8>| Ok(String::from_utf8_lossy(&bytes).into_owned()))?,
    )?;

    // apiQuickSendList(id) — "" until ticket #19 wires the quick-send store;
    // head.lua's wrapper turns "" into nil, exactly like llcom out-of-range.
    globals.set(
        "apiQuickSendList",
        lua.create_function(|_, _id: i64| Ok(String::new()))?,
    )?;

    // __armStopHook(co) — used by head.lua's coroutine.resume override so task
    // threads carry the stop hook (mlua keeps one hook slot and strips
    // Lua-inherited ones; see module docs).
    let arm_stop = Arc::clone(&inner.stop);
    globals.set(
        "__armStopHook",
        lua.create_function(move |_, thread: Thread| {
            arm_stop_hook_on(&thread, Arc::clone(&arm_stop));
            Ok(())
        })?,
    )?;

    Ok(())
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crate::core::bus::Event;

    use super::scripts::LLCOM_README_SAMPLE;

    /// Collects what a test script "sends to the port".
    pub(crate) struct TestSink {
        pub sent: Mutex<Vec<Vec<u8>>>,
    }

    impl TestSink {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
            })
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl UartSink for TestSink {
        fn send_uart(&self, bytes: Vec<u8>) -> bool {
            self.sent.lock().unwrap().push(bytes);
            true
        }
    }

    /// A bus with a background drainer splitting ScriptOut into channels a test
    /// can await with timeouts.
    struct OutBox {
        prints: mpsc::Receiver<String>,
        errors: mpsc::Receiver<String>,
        bus: EventBus,
    }

    impl OutBox {
        fn new() -> Self {
            let bus = EventBus::new();
            let (prints_tx, prints_rx) = mpsc::channel();
            let (errors_tx, errors_rx) = mpsc::channel();
            let mut events = bus.subscribe();
            std::thread::spawn(move || loop {
                match events.blocking_recv() {
                    Ok(Event::ScriptOut(out)) => match out.kind {
                        ScriptOutKind::Print { line } => {
                            if prints_tx.send(line).is_err() {
                                return;
                            }
                        }
                        ScriptOutKind::Error { message } => {
                            if errors_tx.send(message).is_err() {
                                return;
                            }
                        }
                    },
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(_) => continue,
                }
            });
            Self {
                prints: prints_rx,
                errors: errors_rx,
                bus,
            }
        }

        fn next_print(&self, within: Duration) -> String {
            self.prints
                .recv_timeout(within)
                .expect("expected a script print")
        }

        /// Next print containing `needle` (skips unrelated lines, e.g. sys.lua's
        /// own "core loaded!" banner) — and on timeout, dumps what did arrive.
        fn next_print_matching(&self, within: Duration, needle: &str) -> String {
            loop {
                match self.prints.recv_timeout(within) {
                    Ok(line) if line.contains(needle) => return line,
                    Ok(_) => continue,
                    Err(_) => panic!(
                        "no print containing {needle:?} within {within:?}; got: {:?}",
                        self.drain_prints()
                    ),
                }
            }
        }

        fn drain_prints(&self) -> Vec<String> {
            self.prints
                .try_iter()
                .take(1000)
                .collect()
        }
    }

    /// THE acceptance criterion: llcom's README sample — uartReceive callback +
    /// taskInit + waitUntil/publish + apiSendUartData + sys.wait + timerLoopStart —
    /// runs byte-for-byte unmodified, end to end: feeding `hello` produces the
    /// task's reply on the sink and the timer keeps printing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llcom_readme_sample_runs_unchanged() {
        let outbox = OutBox::new();
        let sink = TestSink::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            Some(1),
            Some(Arc::clone(&sink) as Arc<dyn UartSink>),
            LLCOM_README_SAMPLE,
        )
        .expect("README sample must start cleanly");

        // The sample's task replies "ok!" over uart for every received chunk.
        runtime.feed_uart(b"hello");
        let got = recv_until(&sink, |bytes| bytes == b"ok!", Duration::from_secs(5))
            .await
            .expect("task never sent the reply");
        assert_eq!(got, b"ok!");

        // Its log lines, exactly as llcom's log.info + print join them.
        let uart_receive = outbox.next_print_matching(Duration::from_secs(5), "[I]-[uartReceive]");
        assert!(uart_receive.contains("hello"), "{uart_receive}");
        let task_wait = outbox.next_print_matching(Duration::from_secs(5), "[I]-[task waitUntil]");
        assert!(task_wait.contains("hello"), "{task_wait}");
        let sent = outbox.next_print_matching(Duration::from_secs(5), "[I]-[uart send]");
        assert!(sent.contains("true"), "{sent}");

        // The 1000ms loop timer keeps firing without new traffic.
        outbox.next_print_matching(Duration::from_secs(10), "[I]-[timer test]");
        outbox.next_print_matching(Duration::from_secs(10), "[I]-[timer test]");
        runtime.stop();
    }

    async fn recv_until(
        sink: &Arc<TestSink>,
        matches: impl Fn(&[u8]) -> bool,
        within: Duration,
    ) -> Option<Vec<u8>> {
        let t0 = Instant::now();
        while t0.elapsed() < within {
            for bytes in sink.sent() {
                if matches(&bytes) {
                    return Some(bytes);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    /// `sys.wait` resumes on time under tokio, no busy waits: three 100ms waits
    /// take the right total, and the gaps stay near 100ms.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sys_wait_resumes_on_time() {
        let outbox = OutBox::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"
            sys.taskInit(function()
                for i = 1, 3 do
                    sys.wait(100)
                    log.info("tick", i)
                end
            end)
        "#,
        )
        .expect("timing script must start");

        let t0 = Instant::now();
        let mut stamps = Vec::new();
        for _ in 0..3 {
            outbox.next_print_matching(Duration::from_secs(5), "[I]-[tick]");
            stamps.push(t0.elapsed());
        }
        // Total: 3 waits ≈ 300ms (allow generous scheduler slack), each gap ~100ms.
        assert!(
            stamps[2] >= Duration::from_millis(280) && stamps[2] <= Duration::from_millis(700),
            "three sys.wait(100) took {stamps:?}"
        );
        for pair in stamps.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap >= Duration::from_millis(80) && gap <= Duration::from_millis(400),
                "wait gap {gap:?} outside tolerance"
            );
        }
        runtime.stop();
    }

    /// timerLoopStart fires periodically; timerStop silences it (sys.timerStart
    /// with args dedupes identical timers, Luat semantics).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timer_loop_start_and_stop() {
        let outbox = OutBox::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"
            tid = sys.timerLoopStart(log.info, 50, "loop")
            sys.taskInit(function()
                sys.wait(200)
                sys.timerStop(tid)
                log.info("stopped")
            end)
        "#,
        )
        .expect("timer script must start");

        // At ~50ms period, ~4 fires before the stop at 200ms.
        let mut fires = 0;
        loop {
            let line = outbox.next_print(Duration::from_secs(5));
            if line.contains("stopped") {
                break;
            }
            if line.contains("[I]-[loop]") {
                fires += 1;
                assert!(fires <= 6, "timer not stopped: {fires} fires");
            }
        }
        // After the stop, a quiet window proves the timer is really dead.
        std::thread::sleep(Duration::from_millis(300));
        let leftovers: Vec<String> = outbox
            .drain_prints()
            .into_iter()
            .filter(|l| l.contains("[I]-[loop]"))
            .collect();
        assert!(leftovers.is_empty(), "fires after timerStop: {leftovers:?}");
        runtime.stop();
    }

    /// vm-internal pub-sub without timers: publish wakes a waiting task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_subscribe_wakes_task() {
        let outbox = OutBox::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"
            sys.taskInit(function()
                local _, what = sys.waitUntil("GO")
                log.info("got", what)
            end)
            sys.taskInit(function()
                sys.wait(80)
                sys.publish("GO", "payload")
            end)
        "#,
        )
        .expect("pubsub script must start");

        let line = outbox.next_print_matching(Duration::from_secs(5), "[I]-[got]");
        assert!(line.contains("payload"), "{line}");
        runtime.stop();
    }

    /// A script that errors at load: `start` reports the error, publishes it on
    /// the bus (the log AC) and stops the VM — the app never panics.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn script_error_surfaces_on_bus() {
        let outbox = OutBox::new();
        let result = LuaRuntime::start(
            outbox.bus.clone(),
            Some(7),
            None,
            r#"error("boom", 0)"#,
        );
        assert!(result.is_err());
        let message = outbox
            .errors
            .recv_timeout(Duration::from_secs(2))
            .expect("error must be published");
        assert!(message.contains("boom"), "{message}");
    }

    /// The sandbox holds inside the full runtime: io, os.exit and on-disk
    /// require are unavailable; a script trying them errors (published, not panics).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sandbox_blocks_io_and_os_exit() {
        let outbox = OutBox::new();
        let result = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"io.open("/tmp/pwn", "w")"#,
        );
        assert!(result.is_err(), "io must be blocked");

        let result = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"os.exit(1)"#,
        );
        assert!(result.is_err(), "os.exit must be blocked");

        let result = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"require("some_unknown_module")"#,
        );
        assert!(result.is_err(), "require must be preload-only");

        // debug is gone too
        let result = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"debug.sethook(function() end, "l")"#,
        );
        assert!(result.is_err(), "debug must be blocked");
    }

    /// A `while true do end` inside a task must remain stoppable: `stop()`
    /// unwinds the running Lua and quiets the VM (AC: hard loop stoppable).
    /// The handle comes from `create`, so the stop switch exists even while
    /// the script is spinning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hard_loop_in_task_is_stoppable() {
        let outbox = OutBox::new();
        let runtime = LuaRuntime::create(outbox.bus.clone(), None, None)
            .expect("VM must boot");
        assert!(!runtime.is_stopped());

        let runner = runtime.clone();
        let spinning = tokio::task::spawn_blocking(move || {
            runner.run_script(
                r#"
                sys.taskInit(function()
                    while true do end
                end)
            "#,
            )
        });
        // Let the task hit the loop, then cut it down.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let t_stop = Instant::now();
        runtime.stop();
        let result = tokio::time::timeout(Duration::from_secs(5), spinning)
            .await
            .expect("stop must break the loop")
            .unwrap();
        assert!(result.is_err(), "the spinning script must report stopped");
        assert!(runtime.is_stopped());
        assert!(
            t_stop.elapsed() < Duration::from_secs(2),
            "stop took {:?}",
            t_stop.elapsed()
        );

        // A second runtime keeps working: the app survives the stop. (sys.lua
        // prints its own "core loaded!" line at require time — look for ours.)
        let outbox2 = OutBox::new();
        let second = LuaRuntime::start(
            outbox2.bus.clone(),
            None,
            None,
            r#"print("alive")"#,
        )
        .expect("app must survive a stopped VM");
        for _ in 0..3 {
            let line = outbox2.next_print(Duration::from_secs(2));
            if line == "alive" {
                break;
            }
        }
        second.stop();
    }

    /// JSON (serde-backed) round-trips inside the runtime, README-style.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn json_roundtrip_in_runtime() {
        let outbox = OutBox::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"
            json = require("JSON")
            local t = json:decode('{"a":1,"list":[1,2,3],"s":"hi"}')
            log.info("json", t.a, t.s, #t.list)
            print(json:encode({key = "v"}))
        "#,
        )
        .expect("json script must start");

        let json_line = outbox.next_print_matching(Duration::from_secs(2), "[I]-[json]");
        assert!(json_line.contains("1") && json_line.contains("hi") && json_line.contains("3"), "{json_line}");
        let encoded = outbox.next_print_matching(Duration::from_secs(2), "key");
        assert!(encoded.contains("\"key\":\"v\""), "{encoded}");
        runtime.stop();
    }
}
