# Shell candidates — three rendered main-window layouts (issue #28)

Three runnable FerroUI windows under `examples/`, each filled with the **same
mock content** (throwaway inline data, no real core), styled with
`ferroui-themes-winui` at the pinned `=0.4.0-preview.1`:

```text
cargo run --example shell_a   # sidebar navigation
cargo run --example shell_b   # top tab strip
cargo run --example shell_c   # modernized llcom classic
```

Each also takes `--selfcheck` (headless: mounts the real view, walks the
logical tree, checks the mock content is present) and honours
`FERROUI_SMOKE_EXIT_MS=<n>` (closes the window after `n` ms; provided by
`ferroui_view::launch_themed`).

The mock (shared, `examples/shell_common/mod.rs`): ~50 log rows (timestamps,
HEX + decoded text, RX/TX/info directions, two deliberately long wrapping
lines, GBK and UTF-8 non-ASCII rows), a send bar, a 6-entry quick-send panel
with a 1/10 page indicator, a port toolbar, and a Script / Tools / Plot bottom
pane. The send bar actually appends TX rows; quick-send entries fill the send
box; port-bar toggles log info rows — enough to feel the shell, all mock.

---

## Candidate A — `shell_a.rs`, sidebar navigation

```
┌──────────┬──────────────────────────────────────────────┐
│ rcom     │ COM3 ● — 115200 8N1 — UTF-8 — log auto-saved │
│ SESSIONS │ [port ▾][baud ▾][enc ▾][hex]      [Open ◉]   │
│ COM3 ●   │ ┌ log (HEX + decoded, wraps) ─┐ ┌ quick-send ┐
│ COM7 ○   │ │                             │ │ 1/10 ▫▫▫…  │
│ + new    │ │                             │ │ 6 entries  │
│ WORKSPACE│ └─────────────────────────────┘ └────────────┘
│ Scripts… │ [send box                               ][Send]
│ Settings │ [ Script | Tools | Plot ]                      │
└──────────┴──────────────────────────────────────────────┘
```

Controls used (all verified in the pinned sources): `SplitView`
(`display_mode: Inline`, `pane_placement: Left`, `open_pane_length`),
plain `StackPanel`+`Button` rail for the pane, `Grid` content, `ScrollViewer`
+ `StackPanel` log, `DockPanel`/`TextBox`/`Button` send bar, `PipsPager`,
`TabControl` bottom pane, `ComboBox`, `ToggleSwitch`, `CheckBox`, `Separator`.

- Pros: scales to many workspaces (sessions + scripts + tools + settings live
  in one rail); the log gets a full row width; a clear home for features that
  have no llcom equivalent (net monitor, plot); most "modern app" looking.
- Cons: two clicks to reach another session (rail select, then context);
  quick-send competes with the log for width; the rail is mostly idle for a
  serial tool whose real estate is the log; heaviest chrome of the three.

## Candidate B — `shell_b.rs`, top tab strip

```
┌ [COM3 ●] [COM7 ○] [+] ────────────────────────────────┐
│ [New session][Open/close][Clear log][Save log]  (⋯)   │ ← CommandBar
│ [port ▾][baud ▾][enc ▾][hex]               [Open ◉]   │
│ ┌ log ───────────────────────────┐ ┌ quick-send ─────┐
│ └────────────────────────────────┘ └─────────────────┘
│ [send box                                     ][Send] │
├ [ Script | Tools | Plot ] ────────────────────────────┤
└───────────────────────────────────────────────────────┘
```

Controls: `TabControl`/`TabItem` (tab per session, `header`), `CommandBar`
with `CommandBarButton`s in `primary_commands` / `secondary_commands`
(`FerroList<Rc<dyn ICommandBarElement>>` — assembled imperatively, see below),
session content inside the selected tab, bottom pane docked full width.

- Pros: the honest multi-session model (each tab = its port, quick-send set,
  Lua VM and log — matches design.md's session architecture one-to-one);
  `CommandBar` gives session actions a standard home with overflow; best
  log width with quick-send visible.
- Cons: tools/bottom pane are per-window, not per-session (awkward: whose
  script editor is it?); horizontal tab growth with many sessions; the
  CommandBar row spends vertical space on rarely-used actions.

## Candidate C — `shell_c.rs`, modernized llcom classic

```
┌ [port ▾][baud ▾][enc ▾][hex]                [Open ◉] ┐
│ ┌ log (HEX + decoded) ───────┐ ┌ quick-send ────────┐ │
│ │                            │ │ Quick send    1/10 │ │
│ │                            │ │ 6 entries          │ │
│ └────────────────────────────┘ └────────────────────┘ │
│ [send box                                     ][Send] │
├ [ Script | Tools | Plot ] ────────────────────────────┤
└───────────────────────────────────────────────────────┘
```

Controls: `Grid` 2-column layout, `ScrollViewer`+`StackPanel` log,
`DockPanel` send bar, `PipsPager` page indicator, `TabControl` bottom pane,
same port toolbar. No shell chrome at all — everything is one flat session.

- Pros: zero learning cost for llcom users (the target audience); the fewest
  clicks per action — log, send box, quick-send all one glance away; most
  vertical space to the log; simplest tree (fewest controls, easiest to keep
  themes/keyboard consistent).
- Cons: multi-session has no home (the fatal one long-term — design.md wants
  simultaneous sessions); tools pane is always visible even when unused;
  quick-send column steals width from the log permanently.

## Recommendation

**Rank: C > B > A for the first release; converge on B for the shell.**

1. **C (llcom classic)** is the right *default session view*: it maximises the
   log (the product's core), keeps send + quick-send within one glance, and is
   the lowest-risk migration for llcom users. Ship it as the single-session
   layout.
2. **B (tab strip)** is the right *window shell around it*: tabs solve the
   simultaneous-sessions requirement that C cannot, and the `CommandBar` is a
   natural session-action row. A pragmatic synthesis is C's content inside B's
   tab strip (tabs + llcom layout per tab), with tools as a per-session tab in
   the bottom pane rather than a window-wide one.
3. **A (sidebar)** fits a tool suite, not a serial session: it optimises
   navigation between features rcom does not have yet, at the cost of log
   width and session switching. Revisit only when the workspace (scripts,
   plot, net tools) becomes a first-class destination.

## Implementation notes for the design track

- **Dependency added** (the only `Cargo.toml` change, same exact pin):
  `ferroui-view = { version = "=0.4.0-preview.1", features = ["desktop"] }`.
  Its `desktop` feature pulls `ferroui-themes-fluent` (a `launch` default;
  the shells pass the WinUI theme to `launch_themed` instead).
- **How a window boots without XAML**: `ferroui_view::launch_themed(theme,
  view, started)` — platform detect, theme into `app.styles()`, window mount,
  main loop, and the `FERROUI_SMOKE_EXIT_MS` timer are all framework-provided.
  Theme: `WinUITheme::new()` + `set_density_style(DensityStyle::Compact)`
  + `as_style()`.
- **rsx gotchas found at this pin** (all hit and worked around here):
  - attribute values are Rust *expressions*: an element literal cannot be an
    attribute value — build it in a `let` first (`pane:` of `SplitView`);
  - a bare `name(args)` inside `rsx!` children parses as a builder method
    call — expression children need `{ fn_call() }` braces;
  - a dynamic list (`for_each`) must mount into a *panel* host, not a plain
    `ContentControl` — `ScrollViewer { StackPanel { { for_each(..) } } }`,
    not `ScrollViewer { { for_each(..) } }` ("a content holds one child").
- **CommandBar** has no `primary_commands` property for the generated rsx
  setters (it is a `FerroList` of `Rc<dyn ICommandBarElement>`), so it is
  assembled imperatively (`CommandBarButton::new()` + `as_command_bar_element()`)
  and handed to the tree as a control. Fine for a prototype; a wrapper
  component would clean this up.
- **`DockPanel.dock:` attached property** works in rsx (`DockPanel.dock:
  Dock::Right` → `.dock_panel_dock(..)`); same for `Grid.row`/`Grid.column`.
- No charts / tree-datagrid / code-editor crates are published at this pin
  (confirmed in the registry cache) — the bottom pane stays a placeholder, as
  docs/design.md already records.

## Verification status

- `cargo build --examples`: green, no warnings (also `cargo test` green).
- `--selfcheck` for all three shells: green — each mounts its real view
  headlessly, finds 42 timestamped log lines and every content needle
  (incl. the GBK/UTF-8 rows).
- **On-window rendering could not be demonstrated on the machine this was
  authored on**: FerroUI's native render timer fails to start there
  (`ferroui-native` render timer, native error `-6661`). This is
  environmental, not the examples' code: the untouched `cargo run` skeleton
  app fails at the same point, and a 5-line Swift probe
  (`CVDisplayLinkCreateWithActiveCGDisplays`) returns the same `-6661` on
  this macOS 26 host even with `NSApplication` initialised — the API FerroUI
  relies on is broken/deprecated here. On a host where that API works, the
  smoke runs (`FERROUI_SMOKE_EXIT_MS=3000 cargo run --example shell_X`) are
  the intended check; `--selfcheck` covers the built tree everywhere.
