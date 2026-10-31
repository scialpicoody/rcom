# 04 — Secondary-page patterns: navigation, headers, empty/error/busy, confirms (draft)

- **Ticket:** #31 (grilling draft — owner confirmation pending; final assembly into `docs/design-ui.md` happens in #33)
- **Refines:** `docs/ui-drafts/01-shell-verdict.md` (the two tab strips), `docs/ui-drafts/02-visual-language.md` (tokens, density, motion), `docs/ui-drafts/03-log-interaction.md` (the daily-driver surface — nothing here contradicts it), `docs/ui-drafts/05-contract.md` (§2.4 error split, §3 state watch).
- **Purpose:** fix the shared patterns every secondary page pulls from — terminal, script editor, tools (net/MQTT/codec), monitor, plot, settings — so each page implementation is fill-in-the-blanks, not a fresh design argument.
- **Method:** every FerroUI capability named here was verified in the pinned sources (`~/.cargo/registry/src/*/ferroui-*-0.4.0-preview.1/`), file + line cited; where the pin does **not** ship a capability the doc says so and names the fallback. Existing-app claims cite the current page modules (read-only; this ticket ships no app code).
- **No code ships with this ticket.**

---

## 0. Decisions at a glance

| # | Question | Decision | Pin capability it rides on |
|---|---|---|---|
| 1 | How pages are reached | **One flat bottom-pane tab strip per session** (the #28 shell); app-global pages ride it too at v1 (Q1) | `TabControl`/`TabItem` (theme `Controls/TabControl.xaml`, `TabItem.xaml`) — already the app's mechanism (`MainWindow.xaml:131`) |
| 2 | Page header | **One compact row: [status line] [primary action] [secondary]** — the tab header is the title; no in-page title at v1 | `TextBlock` + `Button`; density per token draft §5 |
| 3 | Empty states | **One dim caption in the content area naming the next action** — no illustrations (no icon controls at the pin) | `TextBlock` (`TextFillColorSecondaryBrush`, caption size) |
| 4 | Error surfaces | **Inline field error → status line → page log row → window notification**, in that order of locality; **never modal** | `DataValidationErrors` attached props; `WindowNotificationManager` (app-owned); **no InfoBar / no ContentDialog / no native message box at the pin** (§4.1) |
| 5 | Confirm dialogs | **Destructive-only, button-anchored `Flyout`** (delete script, clear plot, quit with open ports); quit guard via `Window::closing` + one-past banner | `Button::set_flyout` + `Flyout`; `Window::closing` + `WindowClosingEventArgs::set_cancel` |
| 6 | Busy states | **Disable the toggling control until the lifecycle event lands + status word "starting…"**; one optional indeterminate `ProgressBar` on the port-open path only | `Control::set_is_enabled` (as the pages already do for `RequiresRestart`); `ProgressBar::set_is_indeterminate` |
| 7 | Tab overflow | **Flat strip to 9 pages; beyond that, group the tool pages under one `Tools` tab** with an in-page selector (no `SelectorBar` at the pin — a `ComboBox` or `Expander` row) | `ComboBox`, `Expander` |

## 1. Navigation model

**Decision: v1 has exactly two navigation surfaces, both tab strips** (the #28 verdict, unchanged):

- **Top strip** — session tabs (`[COM3 ●] [COM7 ○] [+]). It is the window shell; it never hosts pages at v1.
- **Bottom strip** — the page tabs. This is how every secondary page is reached: one click, always visible, zero discovery cost. The app already works this way (`MainWindow.xaml:131` — `BottomTabs` holding Script, Net, MQTT, Plot).

Target tab order (fixed, semantic; pages that don't exist yet slot in, none relocate):

```
[ Script | Terminal | Codec | Net | MQTT | Plot | Settings ]        (Monitor: disabled placeholder — Q5)
```

Rules:

1. **One flat strip; pages are never nested.** design.md's "tools" bucket dissolves into sibling tabs — the app already flattened it (Net and MQTT are separate `TabItem`s, `MainWindow.xaml:203, 259`), and codec (lane1/22) joins as a third sibling. Nested page-in-page would need a second strip or a selector, both of which cost a click and a lookup.
2. **Session-scoped vs app-global.** Every page except Settings is session-scoped content (it reads that session's traffic). Settings is app-global; hosting it in the per-session bottom pane is a v1 simplification (one window, the tab shows the same content in every session) — revisit when multi-window lands. Q1 covers the alternatives.
3. **What changes if the shell grows destinations** (monitor as a first-class surface, a workspace concept): app-global pages move to the **top strip** as non-session tabs (visually distinguished, no port name, no `+` semantics), the bottom strip stays purely session-scoped. No sidebar: #28 rejected it and nothing here reopens that.
4. **Overflow.** Flat strip up to ~9 pages (7 above + headroom). Beyond that, the tool pages (Codec/Net/MQTT) group under one `Tools` tab with an in-page selector row. Pin fact: there is **no `SelectorBar`** at this pin (absent from `ferroui-controls` and from the winui theme's `Controls/` — see the correction in §4.1), so the selector is a `ComboBox` or an `Expander` per tool. Do not build the grouping before the 9th page exists.
5. **Tab headers are text** (token draft §8: no icons at the pin): `Script`, `Net`, `MQTT`, `Plot` — one word, i18n'd via `t!` as today (`tools_net.rs:68`).

## 2. Page-header anatomy

**Decision: the page header is the tab's first row — one compact row, no in-page title.** The bottom pane is ~230 px tall today (`MainWindow.xaml:133`); a second row for a title that the tab header already carries would spend a tenth of the page on repetition. Density rule (token draft §5) wins.

```
┌─ TabItem header: "Net"  ────────────────────────────────────────────────┐
│ [controls/fields …]        [status line ────]  [PRIMARY] [sec] [sec]    │  ← the header row
│ [content area: list / plot / editor / empty caption]                    │
└─────────────────────────────────────────────────────────────────────────┘
```

Slots, left to right:

| Slot | Content | Pin/token basis |
|---|---|---|
| Title | **omitted at v1** — the `TabItem` header *is* the title (`HeaderedContentControl.header`, `ferroui-controls/tab_item.rs:242`). A page promoted to a full-page destination (§1 rule 3) adds a `SubtitleTextBlockFontSize` (20) title row | token draft §6 ramp |
| Working controls | the page's fields/toggles — what the page *does* (host/port for Net, broker fields for MQTT, decoder picker for Plot) | existing XAML rows |
| **Status line** | one per page, trailing the controls: `CaptionTextBlockFontSize` 12. Colour vocabulary below. The existing `NetStatus` / `MqttStatus` / `PlotStatus` TextBlocks (`MainWindow.xaml:234, 325, 359`) become this slot's canonical implementation | `TextBlock` |
| **Primary action** | exactly one accent-filled button per page (`Open`/`Send`, `Start`, `Connect`, `Run`) — `AccentFillColorDefaultBrush` per token draft §3. **Destructive actions never sit here** (§5) | `Button` + theme accent chain |
| Secondary | 1–3 plain buttons (`Stop`, `Clear`, zoom). Beyond ~3, overflow into a `MenuFlyout` on a text `⋯` button (`Button::set_flyout`, `ferroui-controls/button.rs:525, 636`) — pattern only; no page needs it at v1 counts | `flyouts/menu_flyout.rs` |

**Status-line colour vocabulary** (one line, latest state only — it is a now-indicator, not a history):

| State | Colour | Example |
|---|---|---|
| idle / busy | `TextFillColorSecondaryBrush` (dim); busy text ends in `…` | `idle`, `starting…`, `connecting…` (already in `locales/en.yml:82, 111, 134`) |
| ok | `SystemFillColorSuccessBrush` | `connected [::1]:9000` |
| note | amber pair (log-interaction §0 #2) | `bridge on → session 1` |
| error | `SystemFillColorCriticalBrush` | `connection refused` |

Status text is always i18n'd (`t!`, as every existing status call is). The session-level port state keeps its own chip/banner (contract §3) — a page's status line never re-renders the port state.

## 3. Empty states

**Decision: one dim caption in the content area, centered, naming the next action; controls stay enabled.** No illustrations (the pin ships no icon controls — token draft §8), no dialog, no disabled page. The caption is a `TextBlock` in `TextFillColorSecondaryBrush` at caption size; it disappears on first data and never returns (the status line takes over). The page's controls remain usable — they are how the user leaves the empty state, so greying them out would be a trap.

Per page:

| Page | Empty caption (i18n key sketch) | Notes |
|---|---|---|
| Terminal | `terminal_empty` = "Open the port to start the terminal" + dim hint "raw keys captured, Ctrl+letters included" | keyboard capture only means something with a port |
| Script editor | `script_empty` = "Pick a script above, or New" | only reachable when the picker is empty; samples ship built-in so this is a corner |
| Codec | *(lane1/22 owns the copy)* — pattern: "Choose the conversion, paste text" | content owned by lane1; slot fixed |
| Net | `net_empty` = "Set host and port, then Start" | the log `ListBox` shows the caption; status line already reads `idle` |
| MQTT | `mqtt_empty` = "Connect to a broker to subscribe and publish" | same slot |
| Monitor | `monitor_placeholder` = "Serial monitor ships in phase 4 — pty proxy on macOS/Linux; the Windows driver waits on the license check" | the whole tab is a **disabled placeholder** (`TabItem` disabled, tooltip names the phase); not an empty state the user can leave — Q5 decides visible vs hidden |
| Plot | `plot_status_idle` = "no decoder running — pick a script and Start" (`locales/en.yml:133`) — **exists today**; promoted from status-only to also rendering as the plot area's empty caption | the plot surface is blank until decoders tap |
| Settings | — | always populated; no empty state |

Rule: the empty caption names the **primary action verbatim** ("then Start", "pick a script") so the caption and the header's primary button read as one instruction.

## 4. Error surfaces

### 4.1 What the pin actually has (and not)

Verified by grep + listing across `ferroui-controls-0.4.0-preview.1` and `ferroui-themes-winui-0.4.0-preview.1/Controls/`:

| Capability | At the pin | Evidence |
|---|---|---|
| **InfoBar** | **absent** | zero hits for `infobar` in `ferroui-controls`; the winui theme's `Controls/` (83 files) has no `InfoBar.xaml` |
| **ContentDialog / modal message box** | **absent** | no `contentdialog`/`MessageBox`/`MessageDialog` anywhere in `ferroui-controls`, `-desktop`, `-base`; `ferroui-dialogs` ships **managed file choosers only** (`ferroui-dialogs/managed_file_chooser.rs`) |
| Window notification (toast) | **present, app-mounted** | `Notification` { title, message, type, expiration, on_click } (`ferroui-controls/notifications/notification.rs:29, 132-224`), `NotificationType::{Information, Success, Warning, Error}` (`notification_type.rs:4-11`), `WindowNotificationManager::with_host(..)` / `show` / `close_all` (`window_notification_manager.rs:138, 175, 307`); WinUI-styled by the theme (`Controls/NotificationCard.xaml`, `Controls/WindowNotificationManager.xaml`). **The theme's `Window.xaml` template does not host a manager** — the app instantiates its own `WindowNotificationManager::with_host(Some(&window))` |
| Inline field errors | **present** | `DataValidationErrors.Errors/HasErrors/ErrorConverter` attached properties + `set_errors` (`ferroui-controls/data_validation_errors.rs:87-118, 243`); theme template `Controls/DataValidationErrors.xaml` |
| ToolTip | **present** | `ToolTip.tip` attached property, placement, show-delay (`ferroui-controls/tool_tip.rs:70-120`) |
| Flyout (non-modal popover) | **present** | `Flyout` { content } (`flyouts/flyout.rs:59`), `FlyoutBase::is_open` + `attached_flyout` (`flyouts/flyout_base.rs:64, 79, 147`), placement (`flyouts/popup_flyout_base.rs:247`), `Button::set_flyout` (`button.rs:636`), `MenuFlyout` |
| ProgressBar (indeterminate) | **present** | `ProgressBar::is_indeterminate` (`ferroui-controls/progress_bar.rs:372, 447-451`) |

**Correction to token draft §1:** its crate-comparison table lists the winui theme as shipping `InfoBar`, `MenuBar`, `SelectorBar`, `TitleBar`. At this pin those four control themes do **not** exist in `Controls/` (`CommandBar`, `TabView`-family, `Expander`, `SplitView` do). #33 should fix that row when assembling the token doc; nothing else in the token draft depends on them.

Consequence: **the fallback set is status lines + log rows + (app-mounted) window notifications + flyouts.** The spec below is written against that set; if a later FerroUI release ships InfoBar/ContentDialog, §4.2's matrix gets an upgrade path (§8 Q4).

### 4.2 The decision matrix

Ordered by locality — an error appears at the smallest surface that can carry it:

| # | Case | Surface | Mechanism at the pin |
|---|---|---|---|
| 1 | Field validation (bad port number, empty required host, malformed hex in a converter field) | **Inline field error** on the control | `DataValidationErrors::set_errors(&control, ...)` — the theme's template marks the field; the status line may echo, but the field is the message |
| 2 | Immediate command outcome (`Err` from `open()`, `start_channel`, `start_session`, save/delete failures — contract §2.4's `Result` path) | **Status line**, error colour | the page's status slot (§2); e.g. `net_start_failed` (`tools_net.rs:152`) |
| 3 | Page-scoped async event while the page is the selected tab (`NetEventKind::Error`, `DecoderFailed`, MQTT `Error` — the bus path, contract §2.4) | **Status line + page log row + main-log row** | `set_status` + `append_page_row("!! …")` — exactly today's behaviour (`tools_net.rs:245-251`, `tools_mqtt.rs:326-335`); the `!!` row is the durable story and mirrors to the main log (`format_net_row`, `tools_net.rs:341`) |
| 4 | Same, but the page is **not** the selected tab | **Window notification** (`Warning`), `on_click` → select that page | app-owned `WindowNotificationManager::show` + `Notification::with_on_click` (`notification.rs:200-209`); the background page still gets its status line + rows — the notification is *in addition*, never instead |
| 5 | Process-global (`PortKind::Arrived/Removed` hotplug — contract §1.4's "hotplug toasts"; standalone-VM script errors) | **Window notification** (`Information` / `Error`) | same manager; these already have no home page to light up |
| 6 | Fatal/startup (config unreadable, settings store corrupt) | **Window notification** (`Error`) + the affected page's status line | same |

Standing rules:

- **Never modal.** No modal control exists at the pin, and a modal would freeze a streaming log — the app's core loop must not stop for an error. (This is why "dialog" appears only in §5, for *deliberate* destructive acts, and even there it is a flyout, not a modal.)
- **Every error lands as a log row too** (rule 3's `!!` row) — the log is the durable narrative and the file sink mirrors it; a notification that was missed or a status line that moved on is still findable.
- **Status line shows only this page's latest state.** It is not a global ticker; session/port state lives on the session chip + banner (contract §3), tool state lives on the tool.
- Log-row prefixes stay as shipped: `##` note, `!!` error, `==`/`~~` lifecycle (`controller.rs:291, 333, 805`).

## 5. Confirm dialogs

**Decision: confirms are for destructive-and-irreversible acts only, and the mechanism is a button-anchored `Flyout` — there is no ContentDialog at the pin (§4.1).**

Required confirms (the whole list):

| Act | Today | Why confirm |
|---|---|---|
| Delete a saved script | **immediate** (`controller.rs:765-786`) | a saved file is gone; no undo |
| Clear the plot | **immediate** (`tools_plot.rs:154-156`) | captured points cannot be re-emitted from history — a long capture is real work |
| Close a session tab / quit the app with an open port | no guard today | kills the streaming link; auto-reconnect only helps after replug |

Explicitly **not** confirmed (one click to redo, or a durable copy exists elsewhere):

- Quick-send entry delete — the entry is one click to re-add; the store saves on change.
- Log Clear (log-interaction §3) — the file sink keeps the durable copy (contract §1.4); the view is a rendering.
- Encoding switches, start/stop of net/MQTT/decoder — reversible or trivially restartable.
- Delete of a **sample** script — already refused with a note row (`script_sample_readonly`, `controller.rs:769`).

**Mechanism A — in-page destructive acts (delete script, clear plot):** the destructive button gets a `Flyout` (`Button::set_flyout`, `button.rs:636`) opened on click: one line — *"Delete 'name'? This cannot be undone."* — with **[Delete]** (accent, the destructive verb restated) and **[Cancel]**; transient show mode (default) means click-away/Esc cancels. Focus stays local; the pattern is one small builder shared by all pages (`confirm_flyout(label, verb) -> Flyout`), not per-page XAML.

**Mechanism B — quit guard (close tab / quit with open ports):** a `Window::closing` handler (`window.rs:1032`) checks the open-session count; if open, it calls `WindowClosingEventArgs::set_cancel(true)` (`window_closing_event_args.rs:55`) **once** and raises a non-modal amber confirm row in the session area — same slot as the reconnect banner (contract §3): *"2 ports open — close anyway?"* **[Close all]** **[Stay]**. A second closing while that row is visible passes through (`set_cancel(false)`). One-click-past: the user asked to quit, the guard exists to catch accidents, not to nag — Q3 confirms.

## 6. Busy states

**Decision: disable the toggling control until the lifecycle event lands, plus the status word — no spinners.** An in-flight operation is the gap between a command and its confirming event (contract §2.4: commands resolve or events arrive; the UI never polls). During that gap:

1. **The triggering control (and its inverse) disable.** The Start/Stop toggle greys from click until the confirming event: net → `Listening`/`Connected`/`Disconnected`, MQTT → `Connected`/`Disconnected`, plot decoder → `DecoderStarted`/`Stopped`/`Failed`, port open → the `open()` `Result` (contract §2.2: it resolves when actually open). Today the pages set the optimistic label but leave the button live (`tools_net.rs:140-155`) — double-clicks can queue a stop before the start lands. The fix is the disable, not a queue.
2. **Status text carries the wait**: `starting…` / `connecting…` / `decoder starting…` — all keys exist (`locales/en.yml:82, 111, 134`). Dim colour, per §2's vocabulary.
3. **No spinners anywhere else.** The pin ships an indeterminate `ProgressBar` (`progress_bar.rs:372`) and it is allowed in exactly **one** slot: the session toolbar while the port-open future runs longer than ~500 ms (bad drivers can hang `open()`; the contract's `Opening` is a real in-flight phase). Everywhere else the status word is the whole affordance — net/MQTT/decoder operations complete or fail quickly, and a 230 px pane has no room to park a bar. No animated dots or custom glyph spinners (no icon font; motion budget — token draft §7 — is opacity/brush only, and the theme-owned ProgressBar animation is the one sanctioned exception).
4. **One in-flight op per page.** The disabled button enforces it: a second click is dropped, not queued.

## 7. Per-page application

| Page | Reached by | Header primary | Empty state (§3) | Errors (§4) | Confirms (§5) | Busy (§6) |
|---|---|---|---|---|---|---|
| Terminal | bottom tab (to build) | — (send box is the action) | "Open the port to start the terminal" | status line; port errors ride the session chip/banner (contract §3) | none | input disabled while `Closed`; port-open ProgressBar rule |
| Script editor | bottom tab (exists) | `Run` | "Pick a script above, or New" | status + `!!` rows (save/load/delete — `controller.rs:722, 757, 785`) | **delete script** | Run disabled between click and the started `~~` row (`controller.rs:805`) |
| Codec | bottom tab (lane1/22) | lane1's (`Convert`) | lane1's copy | inline field errors for malformed input; status line for core errors | none | none (conversion is instant) |
| Net | bottom tab (exists) | `Start`/`Stop` | "Set host and port, then Start" | matrix 1–4 (`tools_net.rs:245`) | none | Start disabled until `Listening`/`Connected` |
| MQTT | bottom tab (exists) | `Connect`/`Disconnect` | "Connect to a broker…" | matrix 1–4 (`tools_mqtt.rs:326`) | none | Connect disabled until `Connected` |
| Monitor | **disabled placeholder tab** (phase 4) — Q5 | — | placeholder caption | — (own surfaces in phase 4) | — | — |
| Plot | bottom tab (exists) | `Start` decoder | `plot_status_idle` caption in the plot area | status (`plot_status_failed`) + `!!` row | **Clear plot** | Start disabled until `DecoderStarted` |
| Settings | last bottom tab (Q1) | none — changes apply live (contract §1.5 `Settings.Changed`) | — | inline field errors on numeric fields; status line for apply failures | "Reset to defaults" (if shipped) | none — theme/language live-apply (token draft §4) |

## 8. Open questions for the owner

**Q1 — Settings destination: last bottom-pane tab (proposed), a native menu-bar item, or a ⚙ flyout in the session toolbar?**
The tab is one flat strip with zero new APIs. The pin *does* ship a `NativeMenuBar` (`ferroui-controls/native_menu_bar.rs`) but unwired, and a flyout hides the app's one app-global page behind a hover.
*Recommended default: last bottom-pane tab,* promoted to a top-strip app-global tab when the shell grows destinations (§1 rule 3).

**Q2 — Confirm mechanism: button-anchored Flyout (proposed) or an in-page confirm row swapping the header?**
The flyout is local, disposable, and needs no layout juggling; the confirm-row is more discoverable at small pane sizes but adds a state to every page.
*Recommended default: the flyout* — one shared `confirm_flyout(label, verb)` builder; revisit only if flyouts prove hard to hit at 230 px pane height.

**Q3 — Quit guard: one-click-past banner (proposed), confirm on every close, or no guard?**
*Recommended default: one-click-past* — the `closing`-handler + banner in §5 Mechanism B. It catches the accidental Cmd+Q without making quit a two-dialog ritual for daily users.

**Q4 — Window notifications: adopt the app-owned `WindowNotificationManager` (proposed, for §4.2 rows 4–6 only), or log-rows-only in v1?**
The API and theme styling exist at the pin; the app must mount the manager itself (no default host). If the owner wants fewer moving parts, rows 4–6 degrade to status lines + rows with no information loss — only the "page you weren't looking at" prompt is lost.
*Recommended default: adopt,* scoped to background-page errors + hotplug toasts; never for anything the user just clicked (that is what the status line is for). If a later pin ships `InfoBar`, it replaces the row-4 notification, not the status line.

**Q5 — Monitor placeholder: visible disabled tab (proposed) or hidden until phase 4?**
Visible sets expectations (llcom users look for it) and documents the license gate; hidden keeps the strip clean for two more releases.
*Recommended default: visible disabled tab* with the §3 placeholder caption as its tooltip.

**Q6 — Tab overflow: confirm the "group tools under one `Tools` tab past ~9 pages" rule?**
At 7 pages (§1) it never triggers in v1; the rule exists so a future page (serial monitor, workspace) does not relitigate navigation. Pin fact: no `SelectorBar` — the grouped selector is a `ComboBox`/`Expander` row.
*Recommended default: as proposed; build nothing until the 8th real page lands.*
