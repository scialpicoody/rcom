# rcom design

A cross-platform serial tool in Rust + FerroUI: full llcom parity (README feature list + `LuaApi.md` surface), then stronger. Reference: https://github.com/chenxuuu/llcom

## Goals / non-goals

- Parity with every llcom capability, incl. the Lua runtime (`LuaApi.md`), network tools, encodings, and serial monitor.
- Beyond llcom: cross-platform (Win/macOS/Linux), multiple simultaneous port sessions, curve plotting + Lua protocol decoders, sandboxed scripts with a documented Rust extension API.
- Non-goals (ruled out): installers/MS Store/auto-update, mobile, Bluetooth/BLE.
- Also ruled out: CI pipelines and test suites (by decision). Verification = `cargo build` + `FERROUI_SMOKE_EXIT_MS` smoke run + manual demo per slice. The mock core stays — it is the UI-parallel seam, not a test rig.

## Stack — mature libraries first

| Subsystem | Crate | Notes |
|---|---|---|
| Async runtime | `tokio` 1.54 | full features; the core event loop |
| Serial I/O | `serialport` 4.10 | de-facto standard, sync; reader on `spawn_blocking` thread → channel. (`tokio-serial` rejected: thin wrapper, weak close/cancel semantics) |
| Port metadata | `serialport` `available_ports()` | VID/PID/friendly names, all OSes |
| Hotplug | poll `available_ports()` (1 s) | portable and robust; `udev` crate (Linux) / WM_DEVICECHANGE later as optimization |
| Lua | `mlua` 0.10 (`lua53`, `vendored`, `send`, `async`) | xlua is Lua 5.3 → `lua53` for script compat; sandbox + async built in |
| MQTT | `rumqttc` 0.25 | async client, actively maintained |
| TLS | `tokio-rustls` 0.26 / `rustls` | client + server; self-signed certs via `rcgen` |
| Encodings | `encoding_rs` 0.8 | Firefox's encoder: GBK/GB18030/Big5/Shift_JIS/… |
| Time | `chrono` | log timestamps, local time |
| Logging | `tracing` + `tracing-subscriber` | app diagnostics (separate from the serial session log) |
| Errors | `thiserror` (core) / `anyhow` (app) | standard split |
| Serde | `serde` + `serde_json` + `toml` | settings, quick-send profiles, Lua JSON bridge |
| Paths | `dirs` | platform config/data dirs |
| i18n | `rust-i18n` 4 | zh + en from day one, YAML dicts |
| UI | FerroUI `=0.4.0-preview.1` | pinned; `base`, `controls`, `desktop`, `markup-xaml`, `view`, `themes-winui` |

FerroUI pin gaps (verified against the registry cache for `0.4.0-preview.1`): no charts, no code-editor, no tree-datagrid crate published. Consequences: log view on virtualized `ItemsControl`/`ListBox`; Lua editor starts as a monospaced `TextBox` (syntax highlight later); plot is a small custom render control. Re-evaluate at the next FerroUI release.

## Architecture

Single crate for now: `src/core/` (no FerroUI imports) + `src/app/` (FerroUI). Extract to a workspace only if it earns it.

```
src/
├── core/                  # no ferroui deps
│   ├── port/              # SerialBackend trait, serialport impl, PortManager
│   │                      # reconnect state machine + hotplug poller
│   ├── bus.rs             # tokio broadcast<Event>: Rx, Tx, Log, PortEvent, ScriptOut
│   ├── session.rs         # Session = port + config + log store + Lua VM handle
│   ├── lua/               # mlua runtime: VM pool, sys.* compat, rcom.* extension API
│   ├── logstore.rs        # ring buffer + timestamped file sink (auto-save)
│   ├── nettools/          # tcp/udp/ssl server+client (IPv6), mqtt client
│   ├── codec.rs           # encoding conversion, garbled-text recovery, hex utils
│   └── monitor/           # serial monitor: Win FFI (phase 4), pty-proxy (unix)
└── app/                   # FerroUI XAML shell + viewmodels over the bus
    ├── main_window         # log view | send bar | quick-send pages | tabs per session
    ├── pages/              # script editor, terminal, tools (encodings/net/monitor), plot
    └── i18n/               # zh/en dictionaries
```

### Event flow

```
serial reader ─┐                          ┌─> log store ─> UI log view
net tools ─────┼─> bus (broadcast<Event>) ┼─> Lua VM(s) ─> bus (script TX)
UI send ───────┘         │                └─> plot taps / decoders
                         └─> port events -> reconnect state machine
```

TX path (UI quick-send, terminal, or script) always goes through the session's Lua pre-send hook (llcom parity), then the port writer.

### Serial layer

- `SerialBackend` trait: `open/close/write/` + a stream of reads; `serialport` impl runs a blocking reader on `spawn_blocking`, forwarding bytes over an mpsc channel.
- Reconnect state machine (llcom semantics): `Closed → Open → (error/unplug) → WatchDesired(poll for port reappearing, backoff) → Open`. Port identity = name + VID/PID; auto-reopen only when the user left it open.
- Cancel the blocking read by dropping the session: reader owns a `CancellationToken` and a short timeout so `join` never hangs (the classic .NET bug llcom fought).

### Lua layer

- One sandboxed VM per serial session + one standalone VM (llcom's script panel). `mlua` `lua53` + `sandbox`.
- `sys.*` compat (Luat framework): `taskInit` = coroutine; `sys.wait(ms)` = yield → tokio timer resumes it; `sys.waitUntil`/`publish`/`subscribe` = VM-internal pub-sub; `timerStart`/`timerLoopStart` = tokio intervals. Typical llcom scripts run unmodified.
- Builtin libs: `log`, `uart` (`uartReceive` callback, `apiSendUartData`), `JSON` (serde-backed), string extensions (`fromHex`, `split`, `toHex`).
- `CS.*` interop is **not** carried over; instead a documented `rcom.*` table: `rcom.ports` (enumerate, cross-session send), `rcom.net` (http/tcp one-shots), `rcom.ui` (log, plot taps), `rcom.fs` (sandboxed IO).

### UI layout

```
┌ tabs: [COM3 ●] [COM7 ○] [+] ──────────────────────────────┐
│ ┌ log view (virtualized, HEX+text rows, auto-scroll) ───┐ │ ┌ quick-send (10 pages) ┐
│ │                                                       │ │ │ [entry] [send]        │
│ └───────────────────────────────────────────────────────┘ │ │ ...                   │
│ [port ▾][baud ▾][open][enc ▾][hex] [send box] [send]      │ └───────────────────────┘
│ [ script editor | terminal | tools | plot ]  (tabbed pane) │
└───────────────────────────────────────────────────────────┘
```

Tab per session (each: port, quick-send set, Lua VM, log). Terminal page captures raw keys incl. ctrl+letters.

### Serial monitor (llcom's 串口监听)

- llcom = P/Invoke into a bundled native `serial_monitor.dll` (verified: Costura-embedded, Windows-only). rcom Windows: FFI to the same DLL, **but its license must be verified first** — Apache-2.0 on llcom doesn't obviously cover a prebuilt driver DLL. Blocked on that check (phase 4).
- Linux/macOS: pty-pair proxy mode — rcom sits between two virtual ports and mirrors traffic (covers the common "see what app X sends" case honestly).

## Parity mapping (llcom → rcom)

| llcom | rcom |
|---|---|
| HEX+string dual log, timestamps | log store, one virtualized row = both views |
| auto-save logs with time | `logstore` file sink |
| auto-reconnect on replug | reconnect state machine |
| Lua pre-processing of TX | session pre-send hook |
| quick-send, unlimited entries, 10 pages | paged send bar, Lua hook applies |
| standalone Lua + timers + tasks | standalone VM + `sys.*` |
| text encodings selectable | `encoding_rs` set (UTF-8/GBK/GB18030/Big5/…) |
| terminal mode incl. ctrl+letters | terminal page, raw key capture |
| hide sent data | log row filter toggle |
| TCP/UDP/SSL server/client, IPv6 | `nettools` (tokio + rustls) |
| encoding conversion tools | `codec` + tools page |
| garbled-text recovery | `codec` heuristics |
| MQTT test | `rumqttc` tools page |
| serial monitor | phase 4 (see above) |
| Lua API surface | `LuaApi.md` checklist, `sys.*` + builtins + `rcom.*` |

## Roadmap

1. **Core serial**: deps in, `SerialBackend` + reconnect, log store, main window wired end-to-end (open/send/receive/hex), i18n skeleton. *DoD: open a port, see data, send, log file written.*
2. **Lua engine**: mlua + `sys.*` + builtins + pre-send hook + standalone VM + script editor page. *DoD: llcom's sample scripts run unchanged.*
3. **Network + codec tools**: tcp/udp/ssl/mqtt pages, encodings/converters/garbled recovery. *DoD: parity checklist items green.*
4. **Monitor + plot**: Win monitor FFI (after license check) / pty proxy; custom plot control + Lua decoders. *DoD: curve renders from a script decoder.*
5. **Polish**: full zh/en sweep, themes, settings persistence. *DoD: parity checklist complete; hand-off ready.*

## Open risks

- `serial_monitor.dll` license (phase 4 gate) — fallback: pty-proxy only.
- FerroUI preview churn: pin exactly; re-audit gaps at each bump.
- mlua `lua53` + `async` interplay for `sys.wait` resume semantics — prototype early in phase 2 (half a day spike).
