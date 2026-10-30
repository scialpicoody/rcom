# 05 — UI↔Core contract (draft)

- **Ticket:** #32 (grilling draft — owner confirmation pending; final assembly into `docs/design-ui.md` happens in #33)
- **Refines:** `docs/design.md` (Architecture, Event flow, Serial layer). Where this doc and design.md disagree, design.md wins.
- **Purpose:** the parallel-development seam. A UI team builds the whole app against *this document + the mock core*, then swaps in the real core with zero UI changes.
- **Status:** draft. The log-interaction prototype (see map ticket #27) may amend §1.3 (`Log` payload) and §6 after it lands.
- **No code ships with this ticket.** All Rust is illustrative type sketches, not a commitment to signatures.

---

## 0. The seam in one picture

```
┌─────────────────────────── app/ (FerroUI) ────────────────────────────┐
│ views (rsx!/XAML)   ← bind →   viewmodels (signals, commands)         │
│                                     │ pump events → signals           │
│                                     ▼                                 │
│                        CoreHandle  ← THE ONLY CORE TYPE THE UI SEES   │
└─────────────────────────────────────┼─────────────────────────────────┘
                                      ▼
┌─────────────────────────── core/ (no FerroUI) ────────────────────────┐
│ CoreHandle: SessionManager + global bus + log-store access            │
│ Session: config + reconnect machine + Lua VM + codec                  │
│ bus: tokio broadcast<Event>          logstore: ring buffer + file sink│
│ BackendFactory ──► SerialBackend trait ◄── serialport impl             │
│                                        ◄── MockBackend (scenarios) ◄──┐│
└────────────────────────────────────────────────────────────────────────┘
                                                   swap point: ═════════╝
```

The swap is **inside core, below the session layer**: real and mock are two
`SerialBackend` implementations behind one `BackendFactory`. UI code, session
code, bus, log store, Lua — all unchanged across the swap. §4.2 specifies the
selection mechanism.

Consistency map (design.md → this doc):

| design.md element | Specified here |
|---|---|
| `bus.rs` — `broadcast<Event>`: Rx, Tx, Log, PortEvent, ScriptOut | §2 |
| `SerialBackend` trait: open/close/write + stream of reads | §4.2 (as the swap seam), §5 |
| Reconnect machine `Closed → Open → WatchDesired` | §3 |
| TX path always through the Lua pre-send hook | §2.2 `write()`, open question Q4 |
| Ring-buffer `logstore` + file sink | §1.4 (`Log`), §2.2 `log_snapshot()` |
| Mock core stays (parallel seam, not a test rig) | §4.2, §5 |

---

## 1. Bus events

### 1.1 Channel and delivery semantics

One global `tokio::sync::broadcast::channel<Event>` per process, proposed
capacity **1024**. Consequences the UI must design for:

- **No replay.** The bus is live-only. Anything historical (log rows on tab
  open, state before subscribe) comes from a snapshot API, never from the bus:
  `log_snapshot()` for rows, `watch` receivers for state/config (§3).
- **Lagged consumers.** A slow subscriber gets
  `Err(RecvError::Lagged(n))` — the *oldest* `n` events are gone, channel is
  not closed. **Required viewmodel policy:** on `Lagged`, log a `tracing`
  warning, drop incremental view state, re-pull `log_snapshot()` + read the
  `watch` values, continue. The view self-heals; it never freezes waiting for
  events that will never come.
- **Ordering.** Within one session, all session-scoped events are published
  from that session's single task in wire order; `seq` is per-session
  monotonic across *all* event kinds. A `seq` gap ⇒ this consumer lagged (not
  wire loss — wire loss is a serial-layer concern, out of scope). Across
  sessions there is **no** ordering guarantee and UI must not assume any.
- **No persistence.** The file sink (logstore) is the durable copy, not the
  bus. App restart ⇒ nothing recoverable from the bus.
- **Payload sharing.** Byte payloads are `bytes::Bytes` (cheap `Arc` clone
  into every subscriber; illustrative choice — any cheap-clone byte type
  works). Serde serializes them as raw byte sequences; hex/text rendering
  happens at the edges (§1.3, `Log`).

### 1.2 Session scoping

`SessionId` is a `u64` allocated by the `SessionManager`, never reused within
a process run. Every event carries `session_id: Option<SessionId>`:

- `Some(id)` — session-scoped (Rx/Tx/Log, lifecycle of that session).
- `None` — process-global (hotplug arrival/removal, standalone-VM script
  output).

Subscribers filter by id. `SessionHandle::subscribe()` (§2.2) does the
filtering for you; a tab must never process another session's `Rx`.

### 1.3 The event set

design.md names five: `Rx, Tx, Log, PortEvent, ScriptOut`. They become the
variants of one enum; the five names are kept verbatim.

```rust
pub type SessionId = u64;

#[derive(Debug, Clone, Serialize)]
pub enum Event {
    /// Raw bytes off the wire (or bridged in, e.g. nettools → session).
    Rx(RxEvent),
    /// Bytes actually written to the wire, AFTER the Lua pre-send hook.
    Tx(TxEvent),
    /// One decoded log row. Consumed by the log view AND the file sink.
    Log(LogEvent),
    /// Session lifecycle + process-global hotplug.
    Port(PortEvent),
    /// Anything a script pushed out: print output, plot taps, cross-sends.
    ScriptOut(ScriptOutEvent),
}

pub struct RxEvent {
    pub session_id: SessionId,
    pub seq: u64,            // per-session monotonic, all event kinds
    pub at: DateTime<Utc>,   // chrono, per design.md; rendered local at the edge
    pub bytes: Bytes,        // raw wire bytes, before any decoding
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum TxOrigin { UiSend, QuickSend, Script, NetBridge, Terminal, MockControl }

pub struct TxEvent {
    pub session_id: SessionId,
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub bytes: Bytes,        // post-hook: what the wire actually saw
    pub origin: TxOrigin,    // drives quick-send highlight, hide-sent filter
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum Direction { Rx, Tx, Note }

pub struct LogEvent {
    pub session_id: SessionId,
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub direction: Direction,
    pub raw: Bytes,          // authoritative — the log store keeps this
    pub text: String,        // convenience decode at emit time (current encoding, lossy)
    pub hex: String,         // preformatted hex view (lowercase, space-separated)
    pub garbled: bool,       // decoder hit invalid sequences (feeds recovery UI)
}
```

`Log` design note — **decision, see open question Q2:** `raw` is the single
source of truth; the log store persists `raw` + direction + timestamp. `text`
is a convenience for "render now without a decode pass". When the user
switches encoding, the viewmodel re-renders by re-reading `log_snapshot()`
(ring buffer, cheap) with the new encoding — it does **not** wait for the bus
to re-deliver history. This guarantees the file sink and the screen always
show the same bytes, and encoding switches affect history.

```rust
pub struct PortEvent {
    pub session_id: Option<SessionId>, // None = process-global
    pub port_name: String,
    pub kind: PortKind,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub enum PortKind {
    // session-scoped (Some(id)) — the reconnect machine's outputs
    Opened,
    Closed { by_user: bool },
    Error { message: String },          // mid-stream failure; state → WatchDesired
    Unplugged,                          // port vanished; state → WatchDesired
    WatchDesired,                       // now polling for the port to return
    Reopened,                           // auto-reopen succeeded; state → Open
    // process-global (None) — hotplug poller's outputs (1 s poll, design.md)
    Arrived(PortInfo),
    Removed(PortInfo),
}

pub struct ScriptOutEvent {
    pub session_id: Option<SessionId>, // None = standalone VM
    pub at: DateTime<Utc>,
    pub kind: ScriptOutKind,
}

#[derive(Debug, Clone, Serialize)]
pub enum ScriptOutKind {
    Print { vm: VmId, line: String },            // print()/log() output
    Error { vm: VmId, message: String },         // runtime error, script panel
    PlotTap { decoder: String, series: String, x: f64, y: f64 },
    CrossSend { to: SessionId, bytes: Bytes },   // rcom.ports; target logs a normal Tx
}
```

### 1.4 Publisher / subscriber matrix

| Event | Published by | Consumed by | Ordering / notes |
|---|---|---|---|
| `Rx` | session reader task (port or mock) | session Lua VM, codec→`Log`, plot decoders, UI | in-order per session; the raw feed |
| `Tx` | session write path (post-hook) | codec→`Log`, UI (quick-send highlight, hide-sent) | in-order per session |
| `Log` | session codec (from Rx+Tx) and `Direction::Note` emitters (reconnect banner, script notes) | log view model, logstore file sink | one row = one event; store keeps `raw` |
| `Port` (session) | reconnect state machine | session state watch owner, UI status chips/banners | state transitions also mirrored on the `watch` (§3) |
| `Port` (global) | hotplug poller (1 s) | port-list viewmodel, "port arrived" toasts | derived from diffing consecutive polls; no coalescing |
| `ScriptOut` | mlua layer | script panel (Print/Error), plot page (PlotTap), target session (CrossSend → its Tx path) | per-VM in-order |

App diagnostics are `tracing`, never bus events (design.md keeps the two
separate). The bus is the *session* log's transport only.

### 1.5 Provisional events — refined by their tickets

Marked provisional so the shell ticket can stub them; **their tickets own the
final schema.** Do not build UI against these beyond a disabled placeholder.

```rust
/// nettools ticket — tcp/udp/ssl server+client, IPv6
Event::Net(NetEvent)        // Connected/Disconnected/Data { channel_id, bytes }
                            // bridged data ALSO appears as session Rx (origin NetBridge)

/// MQTT ticket — rumqttc tools page
Event::Mqtt(MqttEvent)      // ConnAck/Disconnected/Message { topic, payload }

/// settings ticket — live-apply (theme, log limit, language)
Event::Settings(SettingsEvent) // Changed { path: String } — UI re-reads the store

/// phase-4 monitor — pty proxy / Win FFI
Event::Monitor(MonitorEvent) // same shape as Rx/Tx, mirrored-packet origin

/// Lua ticket — per-send hook tracing (see Q4)
Event::ScriptOut(ScriptOutKind::HookTrace { vm: VmId, bytes: Bytes }) // debug toggle only
```

---

## 2. Session-handle API

### 2.1 `CoreHandle` — the one object the UI holds

```rust
pub struct CoreHandle { /* SessionManager + bus sender + logstore accessors */ }

impl CoreHandle {
    pub fn list_ports(&self) -> Vec<PortInfo>;
    pub fn watch_ports(&self) -> watch::Receiver<PortMap>; // hotplug-refreshed snapshot
    pub fn create_session(&self, config: SessionConfig) -> Result<SessionHandle, CoreError>;
    pub fn sessions(&self) -> Vec<SessionId>;
    pub fn subscribe_all(&self) -> broadcast::Receiver<Event>; // cross-session views, hotplug
}
```

`PortInfo` and config types (illustrative):

```rust
pub struct PortInfo {
    pub name: String,               // "COM3" | "/dev/ttyUSB0" | "MOCK0"
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub friendly: Option<String>,
    pub kind: PortKindInfo,         // Serial | Mock(MockScenario)
}

pub struct SessionConfig {
    pub port: PortInfo,
    pub baud: u32,                  // default 115_200
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
    pub encoding: EncodingId,       // "utf-8" | "gbk" | "gb18030" | "big5" | ...
}
```

**Boundary rule:** hardware params + `encoding` are session config (core
decodes with the encoding for `Log.text`, Lua string conversion, and the
convenience path). *Presentation* options — hex display, timestamps on/off,
hide-sent, autoscroll, font size — are **viewmodel state, never core state**.
A second UI against the same core may legitimately show different filters.

### 2.2 `SessionHandle` — per open port

```rust
#[derive(Clone)]
pub struct SessionHandle { /* clonable; session lives in the manager */ }

impl SessionHandle {
    pub fn id(&self) -> SessionId;

    // Observables — watch (last-value) semantics, never lag.
    pub fn state(&self) -> watch::Receiver<SessionState>;
    pub fn config(&self) -> watch::Receiver<SessionConfig>;

    // Commands — async, Result for immediate outcomes (§2.4).
    pub async fn open(&self) -> Result<(), SessionError>;
    pub async fn close(&self) -> Result<(), SessionError>;   // from any state; cancels watching
    pub async fn write(&self, data: Bytes, origin: TxOrigin)
        -> Result<TxReceipt, SessionError>;                  // goes THROUGH the Lua pre-send hook
    pub fn set_config(&self, patch: ConfigPatch) -> Result<(), SessionError>;

    // Snapshots + filtered stream.
    pub fn subscribe(&self) -> mpsc::Receiver<Event>;        // this session's events only, in order
    pub fn log_snapshot(&self, limit: usize) -> Vec<LogRow>; // ring-buffer read, re-decoded now
}

pub struct TxReceipt { pub bytes: Bytes } // post-hook bytes: UI shows what the hook sent
```

Semantics the UI depends on:

- `open()` resolves **when the port is actually open** (or on first error) —
  not fire-and-forget. The `Opening` phase is the in-flight future, not a
  fourth state (§3 keeps design.md's three states exactly).
- `write()` is async because the pre-send hook may yield (`sys.wait`, llcom
  parity). The receipt returns post-hook bytes, so a hook that rewrites the
  payload is visible to the sender. UI pattern: optimistic row is fine, but
  reconcile against the receipt/Tx event.
- `set_config`: baud/flow apply live; `encoding` applies to decoding only
  (no port touch); **parity/stop while open → `Err(RequiresRestart)`** — UI
  disables those two controls while the port is open. (Simplest correct rule;
  revisit if a real device workflow needs live parity.)
- Dropping the handle does **not** close the session (tabs may be rebuilt);
  `close()` is explicit. Quitting the app drops `CoreHandle`, which closes
  everything (reader cancellation path, design.md's CancellationToken rule).
- `subscribe()` is a filtered, ordered `mpsc` fed by a forwarder task off the
  global bus — a tab gets exactly its session's events with the same
  in-order guarantee, and `Lagged` still applies (same policy: snapshot and
  heal).

### 2.3 Multi-session readiness

- N concurrent sessions; one tab per `SessionId`; **no global mutable port
  config** — every knob is per session.
- Same port in two sessions simultaneously is a serial-layer error
  (`PortBusy` on the second `open()`), surfaced per session, never global.
- Cross-session features (`rcom.ports`) publish `CrossSend`; the target
  session emits a normal `Tx`. UI must not assume "my tab only sees my
  writes" — filter by `session_id` everywhere.
- The global `subscribe_all()` exists for the few cross-session views:
  port list, hotplug toasts, future multi-port log.

### 2.4 How errors surface

One rule: **synchronous validation and command outcomes → `Result`;
everything after `open()` succeeded → bus events.** UI never polls.

```rust
pub enum SessionError {   // thiserror, per design.md's core/app split
    Config(String),       // invalid baud, unknown EncodingId
    RequiresRestart,      // parity/stop changed while open
    PortBusy,             // port held by another session/process
    NotFound,             // port vanished before open completed
    Hook(String),         // Lua pre-send hook failed (send rejected)
    Closed,               // write/close on a closed session
    Io(String),
}
```

- Immediate: `open()` on a busy port → `Err(PortBusy)` dialog at the command
  site.
- Asynchronous: unplug, driver error, reconnect success — `PortEvent`s on the
  bus + the `state()` watch. A session that was fine 5 s ago fails **without
  any Result being involved**; the status chip and banner react to events.
- App diagnostics (`tracing`) are never user-facing; user-facing text comes
  from `PortKind::Error { message }` + i18n templates.

### 3. State observable

`SessionState` is design.md's reconnect machine, verbatim — three states:

```rust
pub enum SessionState { Closed, Open, WatchDesired }
```

| Transition | Trigger | UI expectation |
|---|---|---|
| `Closed → Open` | user `open()` ok | chip green; controls enable |
| `Open → WatchDesired` | `PortKind::Error` or `Unplugged` (auto-reopen was armed by the user's open) | chip amber + spinner; banner "waiting for port…"; log `Note` row |
| `WatchDesired → Open` | poller sees the port return (`Reopened`) | chip green; log `Note` row; stream resumes |
| `WatchDesired → Closed` | user `close()` | watching cancelled; chip grey |
| `Open → Closed` | user `close()` | chip grey |

Auto-reopen only when the user left it open (design.md). Delivered via
`watch` (coalescing: UI reads latest, never counts updates) **and** as
`PortEvent`s (for banners/log notes). Both, because watch can't express
"this just happened".

---

## 4. ViewModel boundaries and the mock seam

### 4.1 What lives where

| Layer | Owns | Must never contain |
|---|---|---|
| `core/` | sessions, bytes, reconnect machine, log store, Lua, codecs; emits events; serde types | FerroUI types, signals, i18n strings |
| `app/viewmodels/` | per-session + per-page VMs; pump bus/watch → `ferroui-reactive` signals; commands calling `SessionHandle`; presentation state (hex, hide-sent, autoscroll, filters); encode/decode for display | rsx!/XAML, control types, direct `SerialBackend` access |
| `app/` views | rsx!/XAML bound to VM signals (`.bind`, `.two_way`), page layout, i18n keys | tokio types, `Event` matching, core knowledge |

VM plumbing, per FerroUI's MVVM stack (`ferroui-reactive`, `-reactiveui`,
`-dynamicdata`): one background pump task per VM does
`recv()` on the filtered `mpsc` and writes signals; views only read signals.
VMs are plain Rust over signals — testable headless per the FerroUI testing
stack, without a window.

The VM layer is what makes the swap invisible: a VM written against
`SessionHandle` cannot tell a `MockBackend` from a serial port (§4.2).

### 4.2 The swap seam and its mechanism

**Seam:** `BackendFactory` inside core.

```rust
pub trait SerialBackend {   // design.md: open/close/write + a stream of reads
    fn open(&mut self) -> Result<(), BackendError>;
    fn write(&mut self, data: Bytes) -> Result<(), BackendError>;
    fn read_stream(&mut self) -> mpsc::Receiver<Bytes>;  // blocking reader → channel
    fn close(&mut self);
}

pub trait BackendFactory: Send + Sync {
    fn enumerate(&self) -> Vec<PortInfo>;                       // merged into CoreHandle::list_ports
    fn create(&self, port: &PortInfo) -> Result<Box<dyn SerialBackend>, BackendError>;
}
```

Real factory = `serialport`. Mock factory = `MockBackend` scenario players
(§5). Everything above the factory — session, bus, log store, Lua, all
viewmodels, all views — is byte-identical across the swap.

**Selection mechanism (proposal):** compiled in always, selected at runtime
by env var. No cargo feature — a feature would fork `src/app` with `#[cfg]`
and break the "zero UI changes" promise; design.md keeps the mock as a
permanent seam, not a test rig.

```
RCOM_MOCK = off | all | echo | noisy | framed | unplug
  off (default in release builds)  → real backend only
  all (default in debug builds)    → MOCK0 echo, MOCK1 noisy, MOCK2 framed, MOCK3 unplug
  <scenario>                       → mock backend with just that scenario as MOCK0
```

Rules that make the seam verifiable:

- `src/app` contains **zero** `RCOM_MOCK` references or mock conditionals. UI
  reacts only to `PortInfo::kind == Mock(_)` (badge) and to
  `SessionHandle::mock_controls()` being `Some` (debug drawer).
- Mock ports participate in hotplug UX (MOCK3 "removes"/"reappears" through
  the same `Port` events as real ports).

Playback controls (§5.1) ride the session handle as an optional extension
trait so the main API stays mock-free:

```rust
impl SessionHandle {
    pub fn mock_controls(&self) -> Option<&dyn MockControl>; // None on real ports
}
```

---

## 5. Mock core scope

### 5.1 `MockBackend` and playback controls

`MockBackend: SerialBackend` plays a **script** — a timeline of items:

```rust
enum MockItem {
    Rx { after_ms: u64, bytes: Bytes },       // emit bytes as a normal Rx chunk
    Error { after_ms: u64, message: String }, // backend error → PortKind::Error
    Unplug { after_ms: u64 },                 // Removed + session Unplugged
    Reappear { after_ms: u64 },               // Arrived again → auto-reopen path
    Note { text: String },                    // banner row
}

pub trait MockControl: Send + Sync {
    fn pause(&self, on: bool);
    fn set_speed(&self, x: f32);              // 0.25× .. 8×, scales `after_ms`
    fn step(&self);                           // while paused: play next item only
    fn inject_rx(&self, bytes: Bytes);        // play user-typed bytes as device output
    fn trigger(&self, what: MockTrigger);     // Unplug | Error | Reappear | (scenario extras)
    fn restart(&self);                        // replay the script from t=0
}
```

Controls surface as a **Mock drawer** in the UI, visible only for sessions
whose `mock_controls()` is `Some`. Timelines are wall-clock-relative, scaled
by speed. Every scenario is deterministic at 1× with no injected triggers.

### 5.2 Scenario A — echo device (`MOCK0` / `RCOM_MOCK=echo`)

| t (1×) | Item |
|---|---|
| open +100 ms | `Note` "rcom mock echo device v1" |
| ongoing | every `write()` echoed back byte-exact as `Rx` after 30 ms |

**Demonstrates:** the full send→receive loop without hardware — send bar and
quick-send pages produce a Tx row then an Rx row; hex/text toggle flips both;
hide-sent filter leaves only echoes; a Lua pre-send hook that rewrites TX is
**visible in the echo** (what comes back is the post-hook bytes).

### 5.3 Scenario B — noisy sensor (`MOCK1` / `RCOM_MOCK=noisy`)

| t (1×) | Item |
|---|---|
| every 500 ms | `Rx` 16-byte ASCII frame `"<ms>,T=23.4,P=1013.2\r\n"` |
| p≈0.2 per tick | extra `Rx`: 32 random bytes incl. runs of 0x80–0xFF (garbled) |
| p≈0.05 per tick | frame split mid-UTF-8-sequence across two chunks |

**Demonstrates:** encoding switching (GBK ↔ UTF-8 re-renders history from
`log_snapshot`); garbled-row flagging + recovery heuristic; hex view for
binary junk; autoscroll under sustained load; chunk-boundary decode
correctness. Also the lagged-consumer path: at 8× the view must self-heal
(§1.1 policy), not freeze.

### 5.4 Scenario C — framed protocol (`MOCK2` / `RCOM_MOCK=framed`)

| t (1×) | Item |
|---|---|
| 10 frames/s | `Rx` frame: `AA 55 | len u8 | seq u16 | temp f32 | pressure f32 | volts f32 | crc8` |
| trigger `CorruptFrame` | one frame with a bad CRC / short payload |

**Demonstrates:** the plot pipeline end to end — a Lua decoder script
subscribes to `Rx`, validates header+CRC, emits `PlotTap`s, and the plot
page renders 3 live curves at ~10 Hz; pause freezes the *plot* (decoders
paused) without pausing the stream; the corrupt trigger exercises decoder
error paths (skip + note, no crash).

### 5.5 Scenario D — unplug/error mid-stream (`MOCK3` / `RCOM_MOCK=unplug`)

| t (1×) | Item |
|---|---|
| open +100 ms | `Note` "sensor link up" |
| every 1 s | `Rx` status line |
| open +10 s (or `trigger(Unplug)`) | `Unplug` → `Removed` + session `Unplugged` |
| unplug +5 s (adjustable 1–30 s via control) | `Reappear` → `Arrived` → auto-reopen (`Reopened`) |

**Demonstrates:** the reconnect state machine as UX — chip
`Open → WatchDesired → Open`; amber banner with wait reason; log continuity
across the reconnect (one scrollback, `Note` rows at both edges); repeated
rapid unplugs (5× back-to-back triggers) must not hang the reader or leak
sessions (design.md's cancel-by-drop rule); `close()` during `WatchDesired`
cancels watching cleanly.

---

## 6. Acceptance criteria — `UI shell + mock core` implementation ticket

Verification mode is design.md's: build + smoke + manual demo (no CI). Every
criterion below is demonstrable with mock only — no hardware anywhere.

1. **Boot to traffic:** with `RCOM_MOCK=echo`, launching the app lists
   `MOCK0`; opening it at any baud shows the banner row within ~1 s. No real
   port present at any point.
2. **Send/receive loop:** on `MOCK0`, sending from the send bar and from a
   quick-send page produces a Tx row + echo Rx row each; hex toggle flips
   both views; hide-sent leaves echoes only.
3. **Hook visibility:** with the sample pre-send hook active (rewrites TX),
   `MOCK0`'s echo shows the *rewritten* bytes, and the send bar receipt
   reflects them.
4. **Encoding + garble:** on `MOCK1`, switching encoding to GBK re-renders
   history without new traffic; garbled rows are flagged; hex view shows the
   raw junk.
5. **Plot pipeline:** on `MOCK2`, loading the sample decoder renders 3 curves
   at ~10 Hz; mock pause freezes the plot (decoders idle) while the log keeps
   filling — stream continues, per §5.4.
6. **Reconnect UX:** on `MOCK3`, the scripted unplug moves the chip
   `Open → WatchDesired → Open` with banner + log `Note` rows; five rapid
   `trigger(Unplug)` cycles in a row leave the session usable (no hang, no
   duplicate tabs).
7. **Multi-session:** `RCOM_MOCK=all` with `MOCK0` and `MOCK1` open — two
   tabs, independent scrollbacks, no `Rx` cross-talk; closing one tab leaves
   the other streaming.
8. **Self-heal:** `MOCK1` at 8× speed for 60 s: log view stays responsive;
   after a deliberate stall, the view recovers via snapshot (gap visibly
   closes) rather than freezing or erroring.
9. **Seam cleanliness (code review, not demo):** `src/app` has no `RCOM_MOCK`
   or mock-specific `#[cfg]`; UI mock affordances key off
   `PortInfo::kind == Mock(_)` and `mock_controls()` only. Swapping
   `RCOM_MOCK=off` + a real port changes nothing in `src/app`.
10. **Lifecycle:** quitting the app with a mock session open exits cleanly
    (no hung reader); a closed mock session's port can be reopened into a
    fresh session.

Criteria 5 and the `Log` payload stay open to amendment by the
log-interaction prototype (header note).

---

## 7. Open questions for the owner

**Q1 — One global bus, or a channel per session?**
Proposed: one global `broadcast<Event>` + per-session filtered `mpsc` via
`SessionHandle::subscribe()` (§1.1, §2.2).
*Recommended default: as proposed.* Hotplug and `rcom.ports` cross-sends need
a global view anyway; per-session channels would duplicate that plumbing.

**Q2 — Log rows: raw-first with convenience decode, or fully pre-rendered?**
Proposed: `raw` is authoritative (store keeps raw; encoding switch re-renders
from `log_snapshot()`); `text`/`hex` on `LogEvent` are emit-time conveniences
(§1.3).
*Recommended default: as proposed.* It keeps the file sink and screen
byte-identical and makes encoding switching cheap. Fully pre-rendered rows
would freeze history in the encoding of the moment.

**Q3 — Mock selection: single `RCOM_MOCK` env var (proposed), separate
enable+scenario vars, or a settings.json field?**
*Recommended default: single env var* (`off|all|<scenario>`, §4.2), debug
default `all`. One var, demo-friendly, no settings UI needed; promote to a
settings field later only if demos need in-app switching.

**Q4 — What does `Tx` expose about the Lua hook?**
Proposed: `TxEvent.bytes` = post-hook only, `origin` tags the sender; a
per-send hook-trace event is **provisional** (§1.5), behind a debug toggle,
for the Lua ticket to refine.
*Recommended default: post-hook only in v1.* The echo scenario already makes
hook rewrites observable; full tracing can wait for a real need.

**Q5 — Should `write()` block on the Lua hook (async, returns post-hook
receipt), or fire-and-forget?**
Proposed: async through the hook, `TxReceipt { bytes }` (§2.2).
*Recommended default: as proposed.* llcom parity makes the hook part of TX
semantics; fire-and-forget would hide hook rewrites from the sender and
break criterion 3.

**Q6 — Parity/stop changes while open: `Err(RequiresRestart)` (proposed) or
auto close+reopen?**
*Recommended default: error + disabled controls while open.* Simplest correct
rule; revisit only if a real device workflow demands live parity.
