# UI draft 01 — shell verdict (ticket #28)

> Recorded by the orchestrator under this session's autonomous mandate; the owner may revisit.

## Decision

**Hybrid: (b) top tab strip as the window shell + (c) modernized llcom classic as the per-session content layout.** The (a) SplitView sidebar is deferred until a workspace concept becomes a first-class destination.

```
┌ tabs: [COM3 ●] [COM7 ○] [+] ────────────────────────────┐  ← shell (b): TabControl strip
│  ← per-session content (c): llcom classic →             │
│ ┌ log view (hex+text rows) ──┐ ┌ quick-send (1/10) ┐    │
│ │                            │ │ [entry] [send]    │    │
│ └────────────────────────────┘ └───────────────────┘    │
│ [port ▾][baud ▾][open][enc ▾][hex] [send box] [send]    │
│ [ script | terminal | tools | plot ]  (bottom pane)     │
└─────────────────────────────────────────────────────────┘
```

## Reasons

1. Matches `docs/design.md`'s UI sketch; the design doc and `examples/SHELL_CANDIDATES.md` converge independently.
2. Session tabs make multi-session (ticket #35) first-class rather than a retrofit.
3. (c) preserves llcom muscle memory where daily-driver parity matters most (log + quick-send).
4. The candidates memo ranked C > B > A for first release and "converge on B as the window shell" — this hybrid is exactly that.

## Rejected / deferred

- (a) sidebar navigation: extra navigation depth a serial tool doesn't need at v1; revisit if tools multiply.

## Evidence & constraints (from the #28 prototypes)

- `examples/shell_{a,b,c}.rs` build + headless selfcheck green (42 mock rows, non-ASCII content asserted).
- On-window rendering could not be demonstrated on the dev host: FerroUI's native render timer panics `-6661` (`CVDisplayLinkCreateWithActiveCGDisplays`) on macOS 26 / Apple M4 — reproduced on the untouched scaffold. Wake the display (`caffeinate -u`) before GUI runs.
- Pin facts: `CommandBar.primary_commands` is a `FerroList` (assemble imperatively, not via rsx attribute); dynamic `for_each` lists need `ScrollViewer > StackPanel`, not bare `ContentControl`; `ferroui-view`'s `styles:` attribute does not exist at `=0.4.0-preview.1`.
- Dep note: `Cargo.toml` gained `ferroui-view = { version = "=0.4.0-preview.1", features = ["desktop"] }` for the examples (same exact pin).
