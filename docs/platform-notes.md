# rcom platform notes (ticket #36)

Per-OS state after the #36 cross-platform validation pass: what is verified on
real hardware, what is only cross-checked, what is blocked and why, and the
manual pass checklist for a human on each platform. Toolchain facts are
recorded as observed; every "verified" claim carries the command that proves it.

## Verdict table

| OS | Rust typecheck | Test suite | GUI smoke | Autodemo | Reconnect (emulated unplug) | Serial monitor | Real-hardware pass |
|---|---|---|---|---|---|---|---|
| macOS 26 (aarch64) | **verified** (native build) | **verified** 225/225 | **verified** | **verified** (`=1`) | **verified** (`=reconnect`) | **verified** (`RCOM_MONITORDEMO=1`) | **deferred-with-reason** (no hardware on the agent host) |
| Linux x86_64 | **check-passed** (cross, §Linux) | not run | not run | not run | **deferred-with-reason** | code-path present (unix); **unverified at runtime** | **deferred-with-reason** |
| Linux aarch64 | **check-passed** (cross, §Linux) | not run | not run | not run | deferred-with-reason | as above | deferred-with-reason |
| Windows x86_64 | **check-blocked both ways** — msvc: needs the Windows SDK; gnu: mozangle/skia build walls (verbatim errors in §Windows) | not run | not run | not run | deferred-with-reason | **N/A by decision** (FFI skipped — license; §Windows) | deferred-with-reason |

"check-passed" = `cargo check --target …` exits 0 from the macOS host. That
proves the whole tree (incl. the platform cfg-gates) type-checks for that
target; it does **not** prove linking, launching, GUI rendering, or serial
runtime behaviour. The manual pass below closes that gap in minutes on a real
machine.

Host the pass ran on: macOS 26.6.2 (25G83), Apple Silicon (M4), rustc/cargo
1.99.1 (repo `rust-version = 1.89`).

---

## macOS (verified — full pass)

Everything below was run on the #36 host; commands and observed evidence:

| Step | Command | Evidence |
|---|---|---|
| Build | `cargo build` | green (2 pre-existing warnings, see §Open items) |
| Tests | `cargo test --lib` | **225 passed; 0 failed** (3.58 s) |
| Smoke | `caffeinate -u -t 5` then `FERROUI_SMOKE_EXIT_MS=6000 RCOM_AUTODEMO=1 cargo run` | `AUTODEMO OK: payload sent as TX and echoed as RX byte-exact (send→port→bus→log path proven)` |
| Reconnect | `caffeinate -u -t 5` then `FERROUI_SMOKE_EXIT_MS=18000 RCOM_AUTODEMO=reconnect cargo run` | `AUTODEMO RECONNECT: six unplug cycles fired (1 scripted + 5 rapid)`; all six cycles logged `unplug detected → WatchDesired` then `port returned → auto-reopened MOCK0 (#1…#6)` |
| Monitor | `caffeinate -u -t 5` then `FERROUI_SMOKE_EXIT_MS=10000 RCOM_MONITORDEMO=1 cargo run` | `AUTODEMO MONITOR OK: both relay directions captured with direction + timestamps — app→device TX … device→app RX …; virtual port /dev/ttys021 ⇄ /dev/ttys022` |

The reconnect log file
(`~/Library/Application Support/rcom/logs/<date>-<time>_MOCK0.log`) records the
full arc per cycle: `== MOCK0 opened at 115200 baud` → RX/TX stream →
`!! MOCK0 error: MOCK0 was unplugged` → `-- MOCK0 removed` → open +
`auto-reconnected` (6 occurrences in one file).

### macOS quirks

- **`-6661` render-timer panic while the display is asleep.** FerroUI's native
  render timer (`CVDisplayLinkCreateWithActiveCGDisplays`) panics `-6661` on
  macOS 26 / Apple Silicon **only when the display sleeps** — reproduced on the
  untouched scaffold during #28, and confirmed environmental (draft 03 §8
  finding 1). **Workaround: `caffeinate -u -t 5` before every GUI run.** A
  crash with `-6661` in the panic message is this, not an rcom bug.
- **Darwin pty realities (found and fixed in #23; the monitor demo re-verified
  both on this pass):**
  1. the pty slave's termios must be set **raw at creation** — canonical+echo
     would double every app write (the echo returns as "app TX") and
     line-buffer chunks until a newline;
  2. the `openpty` **slave fd must stay open for the monitor's lifetime** — on
     macOS/BSD, closing the last slave fd hangs up master→slave permanently,
     even after the target app reopens the path. rcom holds it unread (so it
     cannot steal app input) for the monitor's lifetime.
- **Port names**: `serialport` 4.10.1 enumerates via IOKit and returns *both*
  `IOCalloutDevice` (`/dev/cu.*`) and `IODialinDevice` (`/dev/tty.*`) entries
  (verified in the pinned source, `posix/enumerate.rs`). **Prefer `/dev/cu.*`**
  — callout devices don't gate on carrier/DCD, `/dev/tty.*` can block on open.
- Data/config dirs: logs at `~/Library/Application Support/rcom/logs`
  (`dirs::data_dir()/rcom/logs`), settings/scripts/quick-send at
  `~/Library/Application Support/rcom/`.

---

## Linux (best-effort cross-check from macOS; native pass pending)

### What was run

`cargo check --target x86_64-unknown-linux-gnu` and
`cargo check --target aarch64-unknown-linux-gnu` from the macOS host. The
method (reproducible):

1. `rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu`.
2. C/C++ cross build scripts driven through **zig cc/c++ shims** (zig 0.16.0,
   `brew install zig`): shim scripts named `<triple>-gcc`/`<triple>-g++` on
   `PATH` that exec `zig cc -target <triple> "$@"` after stripping
   `--target=<rust-triple>` args (cc-rs passes the Rust triple verbatim, which
   zig rejects). With that, the from-source C/C++ build scripts — **ring**,
   **mlua's vendored Lua 5.3** (its `liblua5.3.a` lands in the build output),
   **freetype/harfbuzz/libz** — all compile for the Linux target.
   `skia-bindings` does **not** compile Skia from source here: its default
   `binary-cache` feature downloads a **prebuilt Linux libskia** from the
   rust-skia GitHub releases, and the Rust bindings type-check against it.
3. **libudev pkg-config stub**: serialport's default `libudev` feature needs
   libudev via pkg-config, which refuses cross-compilation. A synthetic
   `libudev.pc` + `PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=<stubdir>` lets the
   build script succeed — safe for `cargo check` only (no linking happens;
   the `udev` crate's FFI declarations type-check independently of C headers).
   **Never build a runnable binary this way** — a real link needs real libudev.

### Result

- **x86_64-unknown-linux-gnu: `cargo check` exit 0** — the entire tree
  (core + app + every dependency incl. serialport, nix pty code, FerroUI
  desktop/Skia) type-checks for Linux. Only the same two pre-existing warnings
  as the macOS build.
- **aarch64-unknown-linux-gnu: `cargo check` exit 0** — same method, same
  clean result (2 m 22 s full build of the target's dependency tree).
- **No rcom code changes were needed** — the only platform gates in the tree
  are the monitor's `#[cfg(unix)]` / `#[cfg(not(unix))]` pair
  (`src/core/monitor/hub.rs`), which cross-check clean in both directions.

### Verbatim blocker (recorded, then routed around)

Without the pkg-config stub, the check fails at:

```
error: failed to run custom build command for `libudev-sys v0.1.4`
...
thread 'main' panicked at .../libudev-sys-0.1.4/build.rs:38:41:
called `Result::unwrap()` on an `Err` value: "pkg-config has not been configured
to support cross-compilation.\n\nInstall a sysroot for the target platform and
configure it via\nPKG_CONFIG_SYSROOT_DIR and PKG_CONFIG_PATH, or install a\ncross-compiling
wrapper for pkg-config and set it via\nPKG_CONFIG environment variable."
```

(LIBUDEV_NO_PKG_CONFIG=1 does **not** bypass it — that env var makes the same
build script abort outright.)

### What a native Linux pass needs

A Linux machine or container (`docker run -v $PWD:/src rust:1-bookworm` is
enough; `cross` also works — it needs a running docker daemon, which the #36
host doesn't have):

```sh
sudo apt install build-essential pkg-config libudev-dev   # the libudev-dev is the one non-obvious dep
cargo build                # FerroUI desktop: needs a display for GUI runs (X11/Wayland)
cargo test --lib
FERROUI_SMOKE_EXIT_MS=6000 RCOM_AUTODEMO=1 cargo run          # expect: AUTODEMO OK
FERROUI_SMOKE_EXIT_MS=18000 RCOM_AUTODEMO=reconnect cargo run # expect: six cycles auto-reopened
FERROUI_SMOKE_EXIT_MS=10000 RCOM_MONITORDEMO=1 cargo run      # expect: AUTODEMO MONITOR OK
```

Runtime notes for the manual pass:
- Serial access: add the user to the `dialout` group (Debian/Ubuntu) for
  `/dev/ttyUSB*` / `/dev/ttyACM*`, or run via sudo once for udev rule testing.
- Enumeration is udev-backed (`libudev` feature, the default): USB VID/PID and
  product strings come from udev properties; `rfcomm*` surfaces as Bluetooth.
  Without libudev the fallback scans `/sys/class/tty` (names only).
- The pty monitor code handles Linux's `EIO` (no slave attached) explicitly
  (`src/core/monitor/pty.rs`); the darwin-specific findings (§macOS quirks)
  are the macOS side of the same two guards — Linux runtime behaviour of both
  is **unverified until a native run**.
- Logs at `$XDG_DATA_HOME/rcom/logs` (`~/.local/share/rcom/logs`), config at
  `~/.config/rcom/`.

---

## Windows (best-effort cross-check; native pass pending)

### What was run

`cargo check --target x86_64-pc-windows-msvc` from the macOS host (target
component installed). **Result: check-blocked at the C/C++-building
dependencies** — with no Windows-target C compiler on this host, cc-rs falls
back to the host compiler (`cc`/`c++`, i.e. Apple clang) and passes it
`--target=x86_64-pc-windows-msvc`; Apple clang has no Windows SDK sysroot, so
the first native crates fail. Verbatim:

```
error: failed to run custom build command for `ring v0.17.14`
error: failed to run custom build command for `harfbuzz-sys v0.8.0`
error: failed to run custom build command for `libz-sys v1.1.31`
...
warning: libz-sys@1.1.31: src/zlib/zconf.h:456:14: fatal error: 'sys/types.h' file not found
...
error occurred in cc-rs: command did not execute successfully (status code exit status: 1):
"cc" "-O1" "-ffunction-sections" "-fdata-sections" "-m64" "--target=x86_64-pc-windows-msvc"
"-I" "src/zlib" "-w" "-DSTDC" "-o" ".../libz-sys/.../gzwrite.o" "-c" "src/zlib/gzwrite.c"
```

This is an SDK/toolchain wall, not an rcom code error — **a Windows machine
with VS Build Tools is the only honest place to run the MSVC pass.**

The `x86_64-pc-windows-gnu` target (GNU ABI; MinGW-w64 headers bundled with
zig via the same shim technique as §Linux, `zig cc -target x86_64-windows-gnu`)
was cross-checked additionally. **Result: check-blocked, on two walls:**

1. **`mozangle`** (ANGLE shader translator, pulled by the FerroUI win32 GL
   backend) compiles a `.c` file with `-std=c++17`; zig's clang driver rejects
   that combination (`error: invalid argument '-std=c++17' not allowed with
   'C'` where GNU g++ would compile the `.c` as C++). Shim-fixable, listed for
   completeness.
2. **`skia-bindings`** hits a hard **network** wall — its `git-sync-deps`
   fetches Skia's third-party deps fresh for the Windows target and this host
   cannot reach the hosts it needs (verbatim):

```
fatal: unable to access 'https://chromium.googlesource.com/chromium/src/third_party/zlib/':
Failed to connect to chromium.googlesource.com port 443 after 75002 ms: Couldn't connect to server
fatal: unable to access 'https://skia.googlesource.com/third_party/libpng.git/': ...
thread 'main' panicked at .../skia-bindings-0.153.3/build_support/skia/config.rs:294:9:
  `skia/tools/git-sync-deps` failed
error: failed to run custom build command for `skia-bindings v0.153.3`
error: failed to run custom build command for `mozangle v0.7.1`
```

(The Linux targets did **not** need this fetch: skia-bindings' default
`binary-cache` feature downloaded a **prebuilt Linux libskia** from the
rust-skia GitHub releases — the build output's `key.txt` reads
`…-x86_64-unknown-linux-gnu-ganesh-gl-jpegd-jpege-pdf-vulkan` — so the Linux
checks never entered the source build. No prebuilt binary is published for
the `windows-gnu` ABI, which forces the source build and its network fetch.)

**Bottom line for Windows: the Rust tree type-checks through the
platform-independent crates (core serial/reconnect/hotplug — no cfg code) and
every pure-Rust Windows binding, but no honest full-target check is possible
from this host. The MSVC pass needs a Windows machine with VS Build Tools.**

### What Windows users get today (the monitor story)

- **Serial monitor: not available on Windows, by decision.** llcom's bundled
  `serial_monitor.dll` is a closed-source blob with no license statement — the
  #23 verdict skipped the FFI path (design.md's fallback: pty-proxy is
  unix-only, and a pty is not a Windows concept anyway). The page says so
  plainly; `MonitorHub::start_monitor` on Windows returns
  `"serial monitoring is not available on this platform (Windows FFI skipped —
  license; pty proxy is unix-only)"` (`src/core/monitor/hub.rs`,
  `#[cfg(not(unix))]` impl).
- **Revisit path**: build llcom's in-tree **Apache-2.0 `serial_monitor_rs/`
  rewrite from source** as our own DLL (same `MonitorComm(pid, port, cb)`
  ABI) — licensing then comes from clearly-attributed Apache-2.0 source, not
  the opaque blob. That is a phase-4-scale decision, not a cfg fix.
- Everything else is cross-platform by construction: serial I/O through
  `serialport` (blocking reader/writer threads), reconnect state machine and
  hotplug poll over `available_ports()` (1 s) — no OS-specific code in
  `src/core/port/`.

### Windows serial notes (verified from the pinned serialport 4.10.1 source)

- Enumeration goes through the SetupAPI **"Ports (COM & LPT)"** device class
  and returns bare **`COMn`** names (`COM3`), with USB VID/PID from the device
  properties; a registry fallback reads
  `HKLM\HARDWARE\DEVICEMAP\SERIALCOMM` for ports outside that class.
- Opening: `COMPort::open` **auto-prefixes `\\.\`** when the name doesn't start
  with a backslash (`src/windows/com.rs`), so rcom's "open whatever the picker
  returned" path works with both `COM3` and a user-typed `\\.\COM3`. Ports
  ≥ COM10 are the classic reason the `\\.\` prefix is mandatory — serialport
  handles it.

### What a native Windows pass needs

A Windows machine with Rust (MSVC toolchain) + Visual Studio Build Tools
(C++ workload — ring/skia/harfbuzz C/C++ compile at build time):

```powershell
cargo build
cargo test --lib
$env:FERROUI_SMOKE_EXIT_MS=6000; $env:RCOM_AUTODEMO=1; cargo run   # expect: AUTODEMO OK
# reconnect: $env:RCOM_AUTODEMO="reconnect" (18 s window) — expect six auto-reopened cycles
# monitor: not applicable on Windows (see above) — expect the honest platform message
```

Real-hardware checklist: plug a USB-serial adapter → it appears as `COMn` in
the picker with VID/PID-driven description → open/send/receive → unplug →
`!! COMn error` + *waiting for the port to reappear* → replug →
*auto-reconnected* (identity = name + VID/PID). Logs at
`%APPDATA%\rcom\logs`.

---

## Manual pass checklists (per OS, minutes each)

Common preamble on every OS: `RCOM_MOCK` defaults to mock ports in debug
builds (`cargo run`) and the real backend in release builds
(`cargo run --release` / the `dist` binary). **For the real-hardware steps use
`RCOM_MOCK=off cargo run`** (or a release build) so the picker lists real
ports; for the scripted demos leave it alone — the autodemo pins the mock seam
itself.

1. **Build + tests**: `cargo build && cargo test --lib` (expect 225+ passing).
2. **Smoke**: `FERROUI_SMOKE_EXIT_MS=6000 RCOM_AUTODEMO=1 cargo run`
   (macOS: `caffeinate -u -t 5` first) → log line `AUTODEMO OK`.
3. **Reconnect (emulated)**:
   `FERROUI_SMOKE_EXIT_MS=18000 RCOM_AUTODEMO=reconnect cargo run` →
   `AUTODEMO RECONNECT: six unplug cycles fired (1 scripted + 5 rapid)`, every
   cycle `unplug detected → WatchDesired` followed by
   `port returned → auto-reopened MOCK0`.
4. **Monitor (unix only)**:
   `FERROUI_SMOKE_EXIT_MS=10000 RCOM_MONITORDEMO=1 cargo run` →
   `AUTODEMO MONITOR OK` with the virtual port pair printed.
5. **Real hardware**: `RCOM_MOCK=off cargo run`, open the adapter
   (`/dev/cu.*` on macOS, `/dev/ttyUSB*`/`/dev/ttyACM*` on Linux, `COMn` on
   Windows), send bytes, verify RX echo with a loopback plug; then unplug →
   watch → replug and confirm auto-reconnect in the log file under
   `<data>/rcom/logs/`.
6. **Monitor vs a real app (unix)**: start a loopback monitor on the Serial
   monitor tab, point any serial tool at the shown virtual port
   (`/dev/ttys…`), confirm both directions appear with ←/→ + timestamps.

AC mapping: "build + smoke green on all three OSes (documented manual pass)" is
satisfied for macOS (done, this host) and prepared for Linux/Windows (this doc
+ the check results); "reconnect verified on at least two OSes" is **verified
on one (macOS, emulated unplug) and deferred-with-reason for the second** —
the unplug/replug scenario runs through the mock seam, whose state machine
(`src/core/port/reconnect.rs`) is OS-independent by construction (no
`#[cfg]` in `src/core/port/`), but the AC's spirit is per-OS runtime proof,
which needs a second real machine; "per-OS notes live in docs" is this file.

---

## Open items surfaced by this pass (for the owner / other lanes)

- Two pre-existing build warnings in app code (not platform work; left for
  #26's parity sweep): unused import `Border`
  (`src/app/tools_terminal.rs:75`), dead function `entry_button_text`
  (`src/app/controller.rs:2033`).
- Optional Linux packaging note: `serialport`'s `default-features = false`
  would drop the libudev build dependency at the cost of udev-enriched USB
  metadata (names only via `/sys/class/tty`) — a product trade-off, not made
  here.
- Windows monitor revisit: `serial_monitor_rs` Apache-2.0 rebuild (§Windows).
