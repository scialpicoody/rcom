# rcom UI specification

- **Status:** destination spec. Assembled under ticket #33 from the UI drafts (`docs/ui-drafts/01`–`05`), amended by the corrections and prototype decisions those drafts record. The drafts stay as history; **this file is the single authority going forward** — where a draft and this file disagree, this file wins. `docs/design.md` remains the parent product spec (architecture, parity table, roadmap); where the two disagree on UI matters, this file is the UI authority and design.md everything else.
- **Verification pin:** FerroUI `=0.4.0-preview.1` (exact pin, all `ferroui-*` crates together). Every capability named here was verified in the pinned sources by the draft tickets; Appendix A lists what the pin does **not** ship so no implementer chases ghosts.
- **Companion:** the **UI shell + mock core** implementation ticket (created by #33) builds this spec against the mock core; its acceptance criteria are §7 here.

## 0. Decisions at a glance

| # | Question | Decision | Source |
|---|---|---|---|
| 1 | Window shell | **Hybrid: top session tab strip + modernized llcom-classic per-session content** (log + send bar + quick-send + bottom pane). Sidebar navigation deferred | 01 #28 |
| 2 | Theme crate | `ferroui-themes-winui` (the framework default), **`DensityStyle::Compact`** | 02 #29 |
| 3 | Theme default | **Dark**, set explicitly at startup; `Dark / Light / FollowOS` tri-state in settings | 02, proven by 03 §8 |
| 4 | Material | Flat surfaces; acrylic only as a future degradable setting | 02 |
| 5 | Accent | Teal `#0E7C8C` ramp overriding the seven `SystemAccentColor*` resources app-level | 02 |
| 6 | Log direction colours | RX = theme success green `#6CCB5F` (dark); TX = `#4EA3E8`; notes = amber `#F7B24B`; garbled = theme critical pair | 03 (resolves 02 Q4) |
| 7 | Log row | One row = `time │ badge │ content` grid; dual view (hex + decoded text) stacked; badges are translucent chips; hook-rewritten TX rows carry ` ⌁hook` | 03 #30 |
| 8 | View modes | Combo: `text + hex` (default) / `hex only` / `text only`; substring filter over text AND hex; both viewmodel state | 03 #30 |
| 9 | Follow model | Follow toggle (default on) + scroll-up auto-disengages; **no pause-on-hover**; log rows never animate | 03 #30 |
| 10 | Send bar | `HEX │ input (Enter sends) │ Send`; TX rows show post-hook bytes; no send-bar hook toggle (quick-send badge is the indicator) | 03 #30 |
| 11 | Quick-send | Right column; 10 pages (pager); edit-in-place values; `⌁ hook` badge in the header | 03 #30 |
| 12 | Page navigation | One flat bottom-pane tab strip per session; fixed order; text tab headers | 04 #31 |
| 13 | Page pattern | Compact header row (controls · status line · one accent primary action); empty = one dim caption; errors inline → status → page row → window notification, never modal; confirms = destructive-only button-anchored `Flyout`; busy = disable-until-event | 04 #31 |
| 14 | Contract seam | `CoreHandle` is the only core type the UI sees; presentation state is viewmodel state; mock core selected by `RCOM_MOCK` env var | 05 #32 |
| 15 | `LogEvent.hooked` | Added: pre-send hook modified the payload on a TX row (**amendment by #30**) | 03 §6.1 |

---

## 1. Visual language — tokens

Every view pulls from this token table; nothing hard-codes colours, sizes, or durations. `rcom.`-prefixed keys are declared in app resources; theme keys are referenced as-is.

### 1.1 Theme crate and density

**`ferroui-themes-winui =0.4.0-preview.1`** with `WinUITheme::set_density_style(DensityStyle::Compact)` at startup (the #28 prototypes run this way; a tool, not a document app). Compact changes control heights/paddings (`TextControlThemeMinHeight` 32→24, `ComboBoxMinHeight` 32→24, `ListViewItemMinHeight` 40→32), not font sizes. The theme ships light/dark/**high-contrast** dictionary variants; `WinUITheme::high_contrast()` exists as a third variant rcom gets for free once settings exposes the tri-state (a fourth "High contrast" setting state can wait).

Correction applied from #31: the winui theme's `Controls/` does **not** ship `InfoBar`, `MenuBar`, `SelectorBar`, or `TitleBar` control themes at this pin (draft 02 §1's crate-comparison row was wrong; `CommandBar`, `TabView`-family, `Expander`, `SplitView` and the rest of the ~80 control themes are real). Nothing in this spec uses the four absent controls.

### 1.2 Theme variant — dark default, set explicitly at startup

**Dark is the default** (serial consoles are a dark domain; llcom is dark; the log is the app). **Startup requirement (proven by the #30 prototype, draft 03 §8 finding 2): the window otherwise follows the OS — on a light-mode OS the dark-tuned log colours are unreadable. The app must set `RequestedThemeVariant = dark` before the main loop; the prototypes never hit this only because they were never rendered.** Settings (#25) owns the runtime tri-state `Dark | Light | FollowOS`; a change re-sets `Application.RequestedThemeVariant` and the theme re-resolves live (no restart). On Windows the native frame follows via `set_frame_theme_variant`.

### 1.3 Material — flat

Flat, solid backgrounds (`SolidBackgroundFillColorBaseBrush` / `LayerFillColorDefaultBrush`); **no acrylic, no mica in v1.** Real blur at the pin is verified on the Win32 backend only; the theme's acrylic-named brushes are solid stand-ins; macOS/Linux blur is unverified. The design must not depend on blur. A future "translucent backdrop" settings toggle would use `ExperimentalAcrylicMaterial` with `FallbackColor` = the flat token so it degrades cleanly — values are pre-defined in the token table below, nothing else blocks on it.

### 1.4 Accent — teal ramp

**`#0E7C8C`** (teal-cyan, hue ≈ 188°), applied by declaring the seven `SystemAccentColor*` `<Color>` resources at application level (resource lookup walks element → logical tree → global styles, so app-level declaration shadows the theme's). The app overrides all seven explicitly (values computed with the theme's own step algorithm) so they can be hand-tuned later without re-deriving:

| Resource key | Value | Where the theme spends it |
|---|---|---|
| `SystemAccentColor` | `#0E7C8C` | light-variant attention brush, accent strokes |
| `SystemAccentColorDark1` | `#094E58` | **light** variant: `AccentFillColorDefaultBrush` |
| `SystemAccentColorDark2` | `#052D33` | **light** variant: `AccentTextFillColorPrimaryBrush` |
| `SystemAccentColorDark3` | `#000405` | rarely consumed |
| `SystemAccentColorLight1` | `#15BBD3` | free for app use (pressed/selected states) |
| `SystemAccentColorLight2` | `#3BD5EB` | **dark** variant: `AccentFillColorDefaultBrush`, `SystemFillColorAttentionBrush` |
| `SystemAccentColorLight3` | `#77E2F1` | **dark** variant: `AccentTextFillColorPrimary/SecondaryBrush` |

App-side accent tokens (reference the brushes, never the hexes): `rcom.accent.fill` = `AccentFillColorDefaultBrush` (open button, active tab indicator, selection, primary CTA); `rcom.accent.text` = `AccentTextFillColorPrimaryBrush` (links, focused values); `rcom.accent.selection` = `SystemControlHighlightListAccentVeryHighBrush` (list/log row selection).

Rationale: far enough from the log's verdict hues (success green ≈ 130°, critical pink/red, amber) that the accent never reads as a log verdict; far enough from stock-Windows blue that screenshots show rcom's identity; mid lightness so the theme's shade algorithm works in both variants.

### 1.5 Surfaces

| Token | Value (dark / light) | Used by |
|---|---|---|
| `rcom.backdrop` | `SolidBackgroundFillColorBaseBrush` (#202020 / #F3F3F3) | window root, log pane background |
| `rcom.surface` | `LayerFillColorDefaultBrush` | quick-send card, bottom pane, dialogs |
| `rcom.surface.raised` | `CardBackgroundFillColorDefaultBrush` | hover cards, flyouts |
| *(future)* `rcom.backdrop.acrylic` | `ExperimentalAcrylicMaterial { TintColor=#202020 (dark) / #F3F3F3 (light), TintOpacity=0.85, FallbackColor = flat token }`, hint `[acrylic_blur, blur, none]` | window root only, settings-gated |

### 1.6 Log semantics colours

| Token | Value (dark) | Used by |
|---|---|---|
| `rcom.log.rx` | `SystemFillColorSuccessBrush` (`#6CCB5F` dark / `#0F7B0F` light) | RX badge text + wash |
| `rcom.log.tx` | **`#4EA3E8`** (app resource) | TX badge text + wash |
| `rcom.log.note` | **amber `#F7B24B`** (app resource) | note rows, reconnect/wait banners, hook badge |
| `rcom.log.garbled` | `SystemFillColorCriticalBrush` (`#FF99A4` dark) + `CriticalBackgroundBrush` row wash | garbled-row flagging |

Resolution record (draft 02 §9 Q4 → draft 03 §0/§1): TX is the prototype-rendered `#4EA3E8` — the draft's first pick `#0B57D0` is near-invisible on `#202020`. Notes are **amber `#F7B24B`, not `SystemFillColorAttentionBrush`**: after the teal accent override, Attention *is* the accent on dark and a note would read as a selection highlight. Badges are direction colour at ~18% alpha behind 100%-alpha text (translucent chips — the label survives colour-blindness and bad projectors; the wash makes a wall of rows scannable by direction). llcom muscle memory preserved: green = device, blue = you.

### 1.7 Spacing and corner radius

`rcom.space.{0,1,2,3,4,6,8}` = 0 / 4 / 8 / 12 / 16 / 24 / 32 px — the only values in app `Margin`/`Padding`/`Spacing` (theme control templates excluded; when a WinUI resource exists for a control's padding, e.g. `TextControlThemePadding`, use the resource). Corner radius: `ControlCornerRadius` 4 (controls), `OverlayCornerRadius` 8 (dialogs/flyouts) — theme resources, automatic.

### 1.8 Type ramp and monospace

UI text rides the theme ramp (`CaptionTextBlockFontSize` 12, `Body…`/`ControlContentThemeFontSize` 14, `BodyStrong…` 14 semibold, `BodyLarge…` 18, `Subtitle…` 20; `Title…` sizes unused — the tab strip is the shell, v1 has no in-window titles). **Every surface that shows or takes bytes uses the monospace chain** (one shared constant): `rcom.font.mono` = `Cascadia Mono, Consolas, Menlo, DejaVu Sans Mono` (`FontFamily::parse` comma fallback list — lands on a real mono face on Windows 10+/macOS/most Linux).

`rcom.font.mono.size` = **13** on every byte surface (log, HEX, send box, quick-send values, terminal, Lua editor) and **11** for timestamps/badges (resolves draft 02 Q5 via draft 03 Q5: unify at 13; the per-session zoom step 12/13/14/16 in #25's presentation settings absorbs taste). The UI family stays the platform default — `ferroui-fonts-inter` is not in rcom's dependency tree and is not added for v1.

### 1.9 Motion

**rcom adds no choreography.** The motion budget goes entirely to state feedback: opacity and brush colours only, never layout (Width/Height/Margin/FontSize) or scroll offsets. Durations reference the theme's tokens: `ControlFasterAnimationDuration` 83 ms (hover/press/selection/chip state), `ControlFastAnimationDuration` 167 ms (flyouts), `ControlNormalAnimationDuration` 250 ms (**hard cap** for anything app-added), easing `ControlFastOutSlowInKeySpline`.

**Log rows never animate:** new rows appear instantly, autoscroll jumps (no smooth scroll), no per-row transitions — a 1 k row/s stream must not pay per-row transition cost. Tab selection is an instant background swap (the theme's 83 ms brush fade suffices; no tab slide/grow). The reconnect/wait banner appears instantly; colour cross-fades only.

`rcom.motion` = `Full` (default) / `Reduced` — an rcom setting, **not** an OS read (the pin has no reduced-motion API). When Reduced: the app attaches zero transitions (no `Transitions` setter outside control templates); theme-internal 83 ms hover fades stay.

### 1.10 Icons

**Text labels in v1** (tab titles, `+`, send button — the #28 shell already works this way). Text glyphs "✕"/"+" are universal characters, not icon-font glyphs. If icons come later: `PathIcon` with rcom-owned `StreamGeometry` resources (`rcom.icon.*`, 16×16 ink, current-colour) — **never the symbol font** (`FontIcon`/`SymbolIcon` do not exist at the pin; the theme itself draws WinUI glyphs as path geometry because Segoe Fluent Icons is Windows-only and non-redistributable). Emoji in the log is content rendering, not UI iconography.

### 1.11 Consolidated token table

| Token | Value | Source of truth |
|---|---|---|
| theme crate | `ferroui-themes-winui =0.4.0-preview.1` | `Cargo.toml` |
| theme density | `DensityStyle::Compact` | startup code |
| `rcom.theme` | `Dark` default · `Light` · `FollowOS` | convention → `Application.RequestedThemeVariant` (set **dark explicitly at startup**) |
| `rcom.backdrop` | `SolidBackgroundFillColorBaseBrush` | theme resource |
| `rcom.surface` | `LayerFillColorDefaultBrush` | theme resource |
| `rcom.surface.raised` | `CardBackgroundFillColorDefaultBrush` | theme resource |
| `SystemAccentColor` + Dark1..3 + Light1..3 | `#0E7C8C` / `#094E58` `#052D33` `#000405` / `#15BBD3` `#3BD5EB` `#77E2F1` | app resource dictionary (7 overrides) |
| `rcom.accent.fill` / `.text` / `.selection` | `AccentFillColorDefaultBrush` / `AccentTextFillColorPrimaryBrush` / `SystemControlHighlightListAccentVeryHighBrush` | theme resources |
| `rcom.log.rx` | `SystemFillColorSuccessBrush` | theme resource |
| `rcom.log.tx` | `#4EA3E8` | app resource |
| `rcom.log.note` | `#F7B24B` | app resource |
| `rcom.log.garbled` | `SystemFillColorCriticalBrush` + `CriticalBackgroundBrush` | theme resources |
| `rcom.space.{0,1,2,3,4,6,8}` | 0/4/8/12/16/24/32 | convention |
| corner radius | `ControlCornerRadius` 4 · `OverlayCornerRadius` 8 | theme resources |
| `rcom.font.mono` | `Cascadia Mono,Consolas,Menlo,DejaVu Sans Mono` | code constant (one definition) |
| `rcom.font.mono.size` | 13 byte surfaces · 11 timestamps/badges | convention |
| UI ramp | 12 / 14 / 18 / 20 (Caption/Body/BodyLarge/Subtitle) | theme resources |
| durations | 83 / 167 / 250 ms (ControlFaster/Fast/Normal) | theme resources |
| easing | `ControlFastOutSlowInKeySpline` | theme resource |
| `rcom.motion` | `Full` default · `Reduced` | convention (setting) |
| icons | text labels (v1); `rcom.icon.*` geometry via `PathIcon` (later) | convention |

---

## 2. Information architecture and navigation

### 2.1 The window shell — hybrid verdict (draft 01, ticket #28)

**(b) top tab strip as the window shell + (c) modernized llcom-classic as the per-session content layout.** The (a) SplitView sidebar is deferred until a workspace concept becomes a first-class destination.

```
┌ session tabs: [COM3 ●] [MOCK1 ○] [+] ──────────────────────────────┐  ← shell: session tab strip
│  ← per-session content (llcom classic) ──────────────────────────  │
│ ┌ log view (hex+text rows, view modes/filter/follow) ┐ ┌ quick-send ┐ │
│ │                                                    │ │ 10 pages   │ │
│ └────────────────────────────────────────────────────┘ └────────────┘ │
│ [port ▾][baud ▾][open][enc ▾] [HEX ☐][send box][Send]               │
│ [ script | terminal | codec | net | mqtt | plot | monitor | settings ]│  ← bottom pane
└──────────────────────────────────────────────────────────────────────┘
```

Reasons (from #28): matches design.md's UI sketch and `examples/SHELL_CANDIDATES.md`'s independent convergence ("converge on B as the window shell"); session tabs make multi-session (#35) first-class rather than a retrofit; (c) preserves llcom muscle memory where daily-driver parity matters most (log + quick-send).

### 2.2 Two tab strips

v1 has exactly two navigation surfaces, both tab strips:

- **Top strip — session tabs** (`[COM3 ●] [COM7 ○] [+]`). It is the window shell; it never hosts pages at v1. `●`/`○` = port open/closed state on the tab header; header text = port name.
- **Bottom strip — page tabs.** Every secondary page is reached here: one click, always visible, zero discovery cost. This is already the app's mechanism (`MainWindow.xaml` `BottomTabs`).

Target bottom-pane tab order (fixed, semantic; the two pages that don't exist yet slot in, nothing relocates):

```
[ Script | Terminal | Codec | Net | MQTT | Plot | Monitor | Settings ]
```

Rules:

1. **One flat strip; pages are never nested.** design.md's "tools" bucket dissolves into sibling tabs (the app already flattened it; codec and monitor shipped as siblings).
2. **Session-pane vs app-global.** Every page except Settings lives in the per-session bottom pane. The session screen (log, send bar, quick-send) and the Script/Terminal pages read that session's traffic directly; the tool pages (Codec, Net, MQTT, Monitor, Plot) show their own tool state but ride the pane and can bridge into the session's traffic. Settings is app-global; hosting it in the per-session bottom pane is a v1 simplification (one window; the tab shows the same content in every session) — revisit when multi-window lands.
3. **If the shell grows destinations** (workspace concept), app-global pages move to the top strip as non-session tabs (visually distinguished; no port name, no `+` semantics) and the bottom strip stays purely session-scoped. No sidebar — #28 rejected it.
4. **Overflow.** Flat strip up to ~9 pages (the 8 above + headroom). Beyond that, the tool pages (Codec/Net/MQTT) group under one `Tools` tab with an in-page selector row (`ComboBox`/`Expander` — there is no `SelectorBar` at the pin). Do not build the grouping before the 9th page exists.
5. **Tab headers are text** (`Script`, `Net`, …), i18n'd via `t!` as everywhere.

### 2.3 Monitor tab status (correction to draft 04)

Draft 04 proposed Monitor as a **disabled placeholder tab** (phase 4). That was superseded by ticket #23, which **shipped** the serial monitor as a live page: unix pty-pair proxy (rcom creates the pty pair, relays traffic between the target app's virtual port and an upstream — loopback or a real serial port), rows with direction + timestamps in the page and the main log; the Windows FFI path was **skipped on the license verdict** (llcom's bundled `serial_monitor.dll` is a closed-source blob with no stated license; verdict recorded on #23). Consequence for this spec: Monitor is an ordinary page in the strip (§4.8). On platforms without pty support the page renders its empty state naming the platform story ("pty proxy (this platform) — Windows FFI: skipped — license") with controls disabled — the *pattern* of a limited page, not a placeholder tab. The #23 revisit path (build llcom's Apache-2.0 `serial_monitor_rs/` rewrite as our own DLL) belongs to phase 4, not to this spec.

---

## 3. Main window — the session screen

### 3.1 Layout

Per session tab: log view on top-left; quick-send panel docked right (~300 px); the session toolbar + send bar row between log and bottom pane; bottom pane below (§4). Spacing per §1.7 (16 px pane gutters, 8 px related-control gaps, flush log↔toolbar join).

### 3.2 Session toolbar (port row)

`[port ▾] [refresh] [baud ▾] [open/close] [encoding ▾]` + session state chip + (while the port-open future runs longer than ~500 ms) the one sanctioned indeterminate `ProgressBar`. Hardware params + encoding are session config (contract §5.3 boundary rule); encoding switches re-render history from `log_snapshot()` without new traffic. Parity/stop-bit controls are **disabled while the port is open** (changing them mid-stream is `Err(RequiresRestart)`).

Session state chip + banner (contract §5.4): `Closed` grey · `Open` green · `WatchDesired` amber + "waiting for port…" banner + log `Note` rows at both edges. The chip/banner reflect only session/port state; a tool page's status line never re-renders it.

### 3.3 Log view

**Row anatomy** — a three-column grid, monospace throughout:

```
08:24:31.497  [RX]   54 32 34 2e 33          ← hex line, dim (TextFillColorSecondary), 13 mono
                     T=24.3                   ← decoded text line, primary, 13 mono
```

- **Columns:** timestamp 92 px fixed (all content aligns), badge 48 px, content `*`. Timestamps render local `HH:MM:SS.mmm` at 11 mono.
- **Dual view (llcom parity):** one row shows hex AND decoded text stacked; the hex line is dimmer. The hex string is the preformatted emit-time `hex` (lowercase, space-separated); the view never re-formats bytes.
- **Direction badges:** `RX` / `TX` / `NOTE` translucent chips (§1.6 colours, ~18% alpha wash, 11 mono).
- **Note rows** (port opened, encoding switched, script started): no hex, one dim italic line prefixed `— `, amber. Not row-sized banners.
- **Garbled rows** (`garbled: true`): text + hex in the critical pink over the dark-red wash — the wash is what makes one bad row findable in a fast stream.
- **Hook marker:** a TX row whose payload was rewritten by the pre-send hook shows a ` ⌁hook` suffix on its text line (consumes `LogEvent.hooked`, §5.2 — amendment by #30).
- **Wrapping:** both lines wrap, always. No horizontal scrollbar, no clipping, no ellipsis — horizontal scrolling breaks the follow model. A 4 KB single-chunk line grows the row tall: accepted for v1.
- **Rendering:** virtualized list (the pin has no datagrid/charts shortcut here — `ItemsControl`/`ListBox` on a virtualizing panel). Rows come from the log store / `log_snapshot()`, not from raw bus replay (§5.1 no-replay rule).

**View modes and filter** (log toolbar, above or beside the rows):

- **View mode combo:** `text + hex` (default) / `hex only` / `text only`. Three renderings of one row, mutually exclusive by construction; flipping re-renders history in place from the store — no new traffic.
- **Substring filter:** live (per keystroke), case-insensitive, matching **both** the decoded text and the hex view (a hex needle finds the raw frame when the text is garbage). Filter first, then render mode. Status: `shown N / M rows`.
- **Hide-sent checkbox** (default off) — TX rows filtered from the view only (store/file stay complete). **Timestamps checkbox** (default on). Both ride the same presentation-state memo as filter/mode (contract §5.3).
- **Clear** empties the log view (the session's view only; the file sink keeps the durable copy — never confirmed, per §4.1's not-confirmed list).

**Follow (auto-scroll) model:**

- An explicit **follow toggle, default on**; follow engages `scroll_to_end()` on every appended batch and on filter/mode changes that grow the list — an instant jump, no smooth scroll.
- **Scrolling up auto-disengages follow** (the checkbox unticks; status reads `PAUSED` / `follow on`). The trigger for "I want to read" is "I scrolled up", not cursor position.
- **Rejected: pause-on-hover** — it freezes the stream exactly when the user moves into the log to select/copy, and a hover-paused log looks identical to a quiet port.
- Deliberately not built: scroll-back-to-bottom auto-re-engages (threshold jitter at a streaming bottom edge; the checkbox is one click). Revisit only on owner request.

### 3.4 Send bar

`[HEX ☐] [input — Enter sends] [Send]`, monospace input.

- **Enter sends** (keyboard-first; the button is the discoverable affordance). llcom parity.
- **HEX toggle switches parsing, not presentation:** on = whitespace-separated hex pairs → bytes (bad input → a note row with the offending text, never a silent fail). Off = the text's bytes in the current session encoding.
- **The TX row always shows post-hook bytes** — what the wire actually saw (contract: `write()` returns the post-hook `TxReceipt`; the emitted row's text/hex are the rewritten payload). With the hook armed, the row carries ` ⌁hook`.
- **No send-bar hook toggle ships.** The hook is armed by the pre-send hook on the Script page (§4.2); the send bar's hook state is read-only, indicated by the quick-send header badge (§3.5). The #30 prototype's checkbox was a mock stand-in.
- Not in the bar: an encoding selector (session config — the toolbar's job) and a per-send "add CRLF" toggle (newline is part of the text).

### 3.5 Quick-send panel

Right column, ~300 px:

- **Header:** `Quick send` + the TX-hook indicator badge — `⌁ hook` (amber on translucent amber) while a pre-send hook is armed and non-trivial, `hook off` (dim) otherwise; flips reactively with the hook state. Placement on quick-send (not the send bar) is the llcom reading: quick-send is where pre-baked commands live, so that is where "your sends will be rewritten" matters most.
- **Pager:** the pin's `PipsPager`, **10 pages** (`N/10` label; `max_visible_pips` truncates gracefully if the count ever grows).
- **Entries:** **edit-in-place is the whole interaction** — the value is a TextBox, always; nothing to "open". Enter in a value sends it; edits survive page switches (each entry owns its value state; paging re-keys the visible rows, not the values). `label │ value │ send` per row; ~32 px row height (compact density territory).
- **Entry count:** the store holds **unlimited entries per page** (design.md parity + the shipped persisted store); 10 pages is the pager. At compact density ~6 rows fit without scrolling — the prototype's 6-entry grid is the presentation target, not a cap. Quick-send entry delete is not confirmed (§4.1).
- **Entries send as plain text** (the value's bytes, through the session TX path — hook applies). Per-entry/per-page HEX mode: **none in v1**; the send bar's HEX toggle covers hex payloads; add a per-page toggle only on demonstrated need.

### 3.6 Session tabs (multi-session, ticket #35)

One tab per session (`SessionId`); `+` opens a new-tab flow (port picker → `create_session`); `●`/`○` on the header mirrors the chip state. Each tab has fully independent presentation state (scrollback, filter, mode, follow, quick-send page) — presentation is viewmodel state, never core state. Cross-session safety (contract §5.3): a tab must never process another session's data events — filter by `session_id` everywhere; `subscribe_all()` exists for the port list and hotplug toasts. Closing a tab with an open port hits the quit-guard pattern (§4.1 confirms, Mechanism B). Same port in two sessions is a serial-layer error (`PortBusy`) surfaced per session, never global.

---

## 4. Bottom-pane pages

### 4.1 The shared page pattern (draft 04, ticket #31)

Every bottom-pane page composes the same five patterns. Each page's row in §4.2–4.9 only names its slots.

**Header — one compact row, no in-page title** (the `TabItem` header is the title; the pane is ~230 px, a second row for a repeated title is waste). Slots, left to right:

| Slot | Rule |
|---|---|
| Working controls | the page's fields/toggles — what the page does |
| **Status line** | one per page, trailing the controls, 12 px caption; latest state only (a now-indicator, not history). Colours: idle/busy = dim, busy ends in `…`; ok = success green; note = amber pair; error = critical. Always i18n'd (`t!`) |
| **Primary action** | exactly one accent-filled button per page (`Run`, `Start`, `Connect`, `Convert`). Destructive actions never sit here |
| Secondary | 1–3 plain buttons; beyond ~3, overflow into a `MenuFlyout` on a text `⋯` button (pattern only; no page needs it at v1 counts) |

A page promoted to a full-page destination (§2.2 rule 3) adds a 20 px title row.

**Empty states — one dim caption in the content area, centered, naming the next action; controls stay enabled** (they are how the user leaves the empty state; greying them out is a trap). No illustrations (no icon controls at the pin). The caption names the primary action verbatim ("…then Start"). It disappears on first data and never returns.

**Errors — never modal** (no modal control exists at the pin, and a modal would freeze a streaming log). Ordered by locality — the smallest surface that can carry it:

| # | Case | Surface |
|---|---|---|
| 1 | Field validation (bad port number, empty host, malformed hex) | inline field error on the control (`DataValidationErrors`) |
| 2 | Immediate command outcome (`Err` from open/start/save/delete) | status line, error colour |
| 3 | Page-scoped async event while the page is selected | status line + page log row (`!! …`) + main-log row (the `!!` row is the durable story) |
| 4 | Same, page not selected | app-mounted **window notification** (`Warning`), `on_click` → select that page — in addition, never instead |
| 5 | Process-global (hotplug arrive/remove, standalone-VM script errors) | window notification (`Information`/`Error`) |
| 6 | Fatal/startup (config unreadable) | window notification (`Error`) + affected page's status line |

Standing rules: every error lands as a log row too (the log is the durable narrative; the file sink mirrors it); status lines show only that page's latest state; log-row prefixes stay as shipped (`##` note, `!!` error, `==`/`~~` lifecycle). The window-notification manager is app-mounted (`WindowNotificationManager::with_host` — the theme styles it; the template does not host one), adopted for rows 4–6 only, never for anything the user just clicked.

**Confirms — destructive-and-irreversible acts only**, mechanism = button-anchored `Flyout` (no ContentDialog at the pin): one line — *"Delete 'name'? This cannot be undone."* — with [Verb] (accent, verb restated) and [Cancel]; one shared `confirm_flyout(label, verb)` builder, not per-page XAML. The required list: **delete a saved script**, **clear the plot**, **close session tab / quit the app with open ports**. Explicitly not confirmed: quick-send entry delete, log Clear (the file sink keeps the durable copy), encoding switches, start/stop of net/MQTT/decoder, deleting a sample script (already refused with a note row). **Quit guard (Mechanism B):** a `Window::closing` handler with open sessions cancels the close **once**, shows a non-modal amber banner in the session area (*"2 ports open — close anyway?"* [Close all] [Stay]); a second closing passes through. One-click-past: the guard catches accidents, it does not nag.

**Busy — disable the toggling control until the lifecycle event lands, plus the status word; no spinners.** Start/Stop greys from click until the confirming event (net → `Listening`/`Connected`/`Disconnected`; MQTT → `Connected`/`Disconnected`; decoder → started/stopped/failed; port open → the `open()` `Result`), which also enforces one in-flight op per page (a second click is dropped, not queued). The indeterminate `ProgressBar` is allowed in exactly one slot: the session toolbar while the port-open future runs >~500 ms. Everywhere else the status word is the whole affordance.

### 4.2 Script page (exists — ticket #19)

Bottom tab `Script`. Header: script picker, name box, New/Save/Delete, **Run** (primary) / Stop; the TX pre-send hook row (one script per session, empty = disabled — arming it is what flips the quick-send `⌁ hook` badge and puts ` ⌁hook` markers on TX rows). Empty state: "Pick a script above, or New" (samples ship built-in, so this is a corner). Errors: status + `!!` rows for save/load/delete; deleting a saved script is confirmed (flyout); deleting a sample is refused with a note row. Busy: Run disabled between click and the started `~~` row. **Editor realities:** a monospaced `TextBox` (13 mono) — there is no code-editor control at the pin; syntax highlighting is later. A misbehaving script (infinite loop) must be stoppable without freezing the UI (#19's shipped generation-counter pattern).

### 4.3 Terminal page (to build — ticket #34)

Bottom tab `Terminal`. The page turns the keyboard into the port:

- **Raw key capture, ctrl+letters included** (design.md terminal row): keystrokes go straight to the open port as raw bytes — ctrl+letter combos arrive as control bytes. Local-echo toggle (default off).
- **Traffic still renders in the session log** (the page is an input surface, not a second log) and **still passes through the Lua pre-send hook** — TX rows from the terminal carry the ` ⌁hook` marker when rewritten. (`TxOrigin::Terminal` exists in the contract for exactly this.)
- **Pattern slots (§4.1):** header = local-echo toggle (secondary); the send surface is the action — no separate primary button; empty state "Open the port to start the terminal" + dim hint "raw keys captured, Ctrl+letters included"; **input disabled while the session is `Closed`**; port errors ride the session chip/banner (the page adds no error surface of its own); port-open ProgressBar rule applies.
- **Key-capture realities:** capture only means something with an open port; the page must not steal keys while disabled or while another control has focus. The disabled input is a deliberate exception to §4.1's "controls stay enabled" empty-state rule: the way out of the empty state is the session toolbar's open control, not the page's own input. No pin capability is named here beyond ordinary key handling — the exact input API is ticket #34's to verify at the pin (ferro-dev bar: nothing is used that is not seen in the pinned sources).

### 4.4 Codec page (exists — ticket #22)

Bottom tab `Codec`. Header: operation selector (charset conversion A→B with swap, hex/base64/percent codecs, garbled recovery), charsets, input-is-hex toggle, **Convert** (primary). Two monospaced text areas (input editable, output read-only + selectable); recovery candidates render as a click-to-apply list. Errors: inline field errors for malformed input; status line for core errors. No confirms, no busy state (conversion is instant). Empty-state copy is lane1's (#22 owns it; slot fixed by the pattern).

### 4.5 Net page (exists — ticket #20)

Bottom tab `Net`. Header: protocol/mode selectors, TLS toggle, host + port, **Start/Stop** (primary, disabled until the confirming event). Bridge toggle (net ⇄ serial). Page log `ListBox`. Errors: matrix rows 1–4 (§4.1). Empty state: "Set host and port, then Start". Bridged traffic appears in the session log as ordinary data (origin `NetBridge`) — the page's own list is the tool's story, the session log is the session's.

### 4.6 MQTT page (exists — ticket #21)

Bottom tab `MQTT`. Header: broker endpoint + credentials + TLS, **Connect/Disconnect** (primary, disabled until `Connected`); subscription row (filter + QoS + add/remove); publish row (topic + QoS + retain + payload); bridge row (topic ⇄ serial toggle). Page log `ListBox`. Errors: matrix rows 1–4. Empty state: "Connect to a broker to subscribe and publish".

### 4.7 Plot page (exists — ticket #24)

Bottom tab `Plot`. Header: decoder script picker + **Start** (primary) / Stop, Pause, Clear; status line. The curve area is the shipped custom render control (`plot_view.rs` — there is no charts crate at the pin; design.md's pin-gap consequence, now built): dark plot field, grid, one polyline per channel (fixed 8-colour palette keyed by sorted channel order), legend, min/max labels. **Data path (supersedes the contract draft's per-point `PlotTap` events — decision by #24):** decoders append points into the process-global `PlotStore` (bounded ring per channel); the UI pulls one decimated snapshot per pump tick (~30 fps); `Event::Plot` on the bus carries lifecycle notices only. **Pause freezes the plot only** — the stream and log keep flowing (the mock scenarios rely on this). Zoom = time-window halve/double via buttons and wheel (1 s–600 s); **Clear is confirmed** (a long capture is real work). Empty state: the existing `plot_status_idle` copy, promoted from status-only to also rendering as the plot area's empty caption. Errors: status + `!!` row; a decoder error inside `decode` fails the decoder once (one `!!` row), never per chunk.

### 4.8 Monitor page (exists — ticket #23; see §2.3)

Bottom tab `Monitor`. Header: upstream selector (loopback virtual device / real ports) + baud + **Start/Stop** (primary) + status. Virtual-port row: copyable read-only path + the platform story. Captured rows (direction relative to the monitored app + timestamps) in the page and mirrored to the main log; the durable file gets tool-marked rows. **Honesty copy is part of the spec:** the page says plainly that the monitor sees what flows through rcom's relay, not a passive wire tap. Windows: the FFI path is skipped (license verdict on #23) — the page renders its platform story with controls disabled on platforms without pty support (§2.3). Errors: status line; matrix row 4 notification if capture fails in the background.

### 4.9 Settings page (to build — ticket #25)

Bottom tab `Settings` (last — §2.2 rule 2). No primary action — **changes apply live** (`Event::Settings(SettingsEvent::Changed { path })` → the app re-reads the store; theme and language live-apply, no restart). Contents: theme tri-state (`Dark` default / `Light` / `FollowOS`), language (zh/en), default port/baud/encoding, log directory, per-session presentation zoom step (12/13/14/16), motion (`Full`/`Reduced`). Inline field errors on numeric fields; "Reset to defaults" (if shipped) is the page's only confirm. Settings is app-global content hosted in the per-session pane (v1 simplification).

---

## 5. UI↔Core contract

Assembled from draft 05 (#32) with the #30 amendments folded in and **marked `AMENDED by #30`**. All Rust is illustrative type sketching, not a commitment to signatures. Where this section and design.md disagree, design.md wins. §5.8 records where the already-shipped core code has diverged from the draft — advisory for the shell ticket, not an amendment.

### 5.1 The seam in one picture

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
│ bus: broadcast<Event>                logstore: ring buffer + file sink│
│ BackendFactory ──► SerialBackend trait ◄── serialport impl             │
│                                        ◄── MockBackend (scenarios) ◄──┐│
└────────────────────────────────────────────────────────────────────────┘
                                                   swap point: ═════════╝
```

The swap is **inside core, below the session layer**: real and mock are two `SerialBackend` implementations behind one `BackendFactory`. UI code, session code, bus, log store, Lua — all unchanged across the swap.

**Bus delivery semantics** (what the UI must design for): one global `broadcast<Event>` (capacity ~1024). **No replay** — history comes from snapshot APIs (`log_snapshot()`, `watch` receivers), never from the bus. **Lagged consumers** get `Lagged(n)` (oldest events gone, channel alive): the required viewmodel policy is log a `tracing` warning, drop incremental state, re-pull `log_snapshot()` + `watch` values, continue — the view self-heals, it never freezes. **Ordering:** per-session in wire order; across sessions no guarantee. **No persistence** — the file sink is the durable copy. **Payloads** are cheap-clone byte types; hex/text rendering happens at the edges.

### 5.2 The event set

design.md names five; they are the variants of one enum, names kept verbatim: `Rx, Tx, Log, PortEvent, ScriptOut` (draft 05 §1.3 has the full sketch). The pieces the UI consumes:

- **Data events** (`Rx`/`Tx`): per-session chunk in one direction, timestamped; the raw feed. `Tx` is **post-hook** (what the wire saw), tagged with a `TxOrigin` (`UiSend, QuickSend, Script, NetBridge, Terminal, MockControl`) that drives the quick-send highlight and the hide-sent filter.
- **`Log(LogEvent)`** — one decoded log row, consumed by the log view AND the file sink:

```rust
pub struct LogEvent {
    pub session_id: SessionId,
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub direction: Direction,   // Rx | Tx | Note
    pub raw: Bytes,             // authoritative — the log store keeps this
    pub text: String,           // convenience decode at emit time (current encoding, lossy)
    pub hex: String,            // preformatted hex view (lowercase, space-separated)
    pub garbled: bool,          // decoder hit invalid sequences (feeds recovery UI)
    pub hooked: bool,           // AMENDED by #30 — pre-send hook modified the payload; TX rows only
}
```

  Design note: `raw` is the single source of truth; the store persists `raw` + direction + timestamp; `text`/`hex` are emit-time conveniences. When the user switches encoding, the viewmodel re-renders by re-reading `log_snapshot()` with the new encoding — it does not wait for the bus to re-deliver history. This keeps the file sink and the screen byte-identical and makes encoding switches affect history. **`hooked` is true when the pre-send hook changed the payload on a TX row (post-hook bytes are what `raw`/`text`/`hex` already carry); the log view renders it as the hook marker.** *(AMENDED by #30: the `hooked` field and that sentence — neither `TxOrigin` (tags the sender) nor `TxReceipt` (reaches the sender, not the row) carried it.)*
- **`Port(PortEvent)`** — session lifecycle (`Opened`, `Closed { by_user }`, `Error { message }`, `Unplugged`, `WatchDesired`, `Reopened`) and process-global hotplug (`Arrived(PortInfo)`, `Removed(PortInfo)`; 1 s poll diffing, no coalescing).
- **`ScriptOut(ScriptOutEvent)`** — `Print`, `Error`, `CrossSend { to, bytes }` (rcom.ports cross-sends; the target logs a normal Tx), and `PlotTap` — **`PlotTap` is superseded by the shipped pull model** (§5.8, #24): the per-point data path is the plot store, not the bus.
- **Provisional variants** (§1.5 of the draft — their tickets own the final schema; the shell ticket may stub them, and net/MQTT/monitor/plot already shipped theirs): `Net(NetEvent)`, `Mqtt(MqttEvent)`, `Settings(SettingsEvent::Changed { path })`, `Monitor(MonitorEvent)`, `HookTrace` (debug-only, provisional, Lua ticket's).

**Publisher/subscriber matrix** (draft 05 §1.4) stands as written: session reader → VM/codec/decoders/UI; write path → codec/UI; codec + note emitters → log view + file sink; reconnect machine → chip/banner; hotplug poller → port list/toasts; mlua → script panel/plot/target session.

### 5.3 Session-handle API and the boundary rule

`CoreHandle` (the one object the UI holds): `list_ports()`, `watch_ports()`, `create_session(config)`, `sessions()`, `subscribe_all()`. `SessionHandle` (clonable): `id()`, `state()` and `config()` as `watch` receivers (last-value semantics, never lag), commands `open()` / `close()` / `write(data, origin)` / `set_config(patch)`, `subscribe()` (filtered ordered stream — exactly this session's events), `log_snapshot(limit)` (ring-buffer read, re-decoded now). `write()` is async through the Lua pre-send hook and returns `TxReceipt { bytes }` — post-hook — so a rewriting hook is visible to the sender. `open()` resolves when the port is actually open (or first error).

`set_config`: baud/flow apply live; encoding applies to decoding only; **parity/stop while open → `Err(RequiresRestart)`** — the UI disables those controls while open.

**Boundary rule:** hardware params + `encoding` are session config. *Presentation* options are **viewmodel state, never core state** — hex display, timestamps on/off, hide-sent, autoscroll, font size; **plus view mode (`text + hex` / `hex only` / `text only`), substring filter, and follow/auto-scroll** *(AMENDED by #30: the list grows by those three — the prototype treats them as the same kind of viewmodel state)*. A second UI against the same core may legitimately show different filters.

Multi-session readiness: N concurrent sessions, one tab per `SessionId`, no global mutable port config; same port twice = `PortBusy` on the second `open()`, surfaced per session; UI must not assume "my tab only sees my writes" — filter by `session_id` everywhere; `subscribe_all()` for port list, hotplug toasts, future multi-port views. Dropping a handle does not close the session; `close()` is explicit; quitting drops `CoreHandle` and closes everything.

### 5.4 State observable

`SessionState { Closed, Open, WatchDesired }` — design.md's reconnect machine, verbatim:

| Transition | Trigger | UI expectation |
|---|---|---|
| `Closed → Open` | user `open()` ok | chip green; controls enable |
| `Open → WatchDesired` | `Error`/`Unplugged` (auto-reopen was armed) | chip amber + banner "waiting for port…"; log `Note` row |
| `WatchDesired → Open` | port returns (`Reopened`) | chip green; log `Note` row; stream resumes |
| `WatchDesired → Closed` | user `close()` | watching cancelled; chip grey |
| `Open → Closed` | user `close()` | chip grey |

Auto-reopen only when the user left it open. Delivered via `watch` (coalescing — UI reads latest, never counts updates) **and** as `PortEvent`s (for banners/log notes): watch cannot express "this just happened".

### 5.5 How errors surface

**Synchronous validation and command outcomes → `Result`; everything after `open()` succeeded → bus events. The UI never polls.** `SessionError { Config, RequiresRestart, PortBusy, NotFound, Hook, Closed, Io }`. Immediate errors dialog at the command site (status line, error colour); asynchronous failures arrive as `PortEvent`s + the `state()` watch with no `Result` involved. App diagnostics are `tracing`, never user-facing; user-facing text comes from `PortKind::Error { message }` + i18n templates. The presentation-side rules are §4.1's error matrix.

### 5.6 ViewModel boundaries

| Layer | Owns | Must never contain |
|---|---|---|
| `core/` | sessions, bytes, reconnect machine, log store, Lua, codecs; emits events | FerroUI types, signals, i18n strings |
| `app/viewmodels/` | per-session + per-page VMs; pump bus/watch → signals; commands calling `SessionHandle`; **presentation state (§5.3 list)**; encode/decode for display | rsx!/XAML, control types, direct backend access |
| `app/` views | views bound to VM signals, page layout, i18n keys | tokio types, event matching, core knowledge |

One background pump task per VM `recv()`s the filtered stream and writes signals; views only read signals. VMs are plain Rust over signals — testable headless. **The VM layer is what makes the mock→real swap invisible: a VM written against `SessionHandle` cannot tell a `MockBackend` from a serial port.**

### 5.7 The mock seam and its mechanism

**Seam:** `BackendFactory` inside core (`SerialBackend`: open/close/write + a stream of reads; real factory = `serialport`, mock factory = `MockBackend` scenario players). **Selection: compiled in always, selected at runtime by env var — no cargo feature** (a feature would fork `src/app` with `#[cfg]` and break the zero-UI-changes promise; design.md keeps the mock as a permanent parallel seam, not a test rig):

```
RCOM_MOCK = off | all | echo | noisy | framed | unplug
  off (default in release builds)  → real backend only
  all (default in debug builds)    → MOCK0 echo, MOCK1 noisy, MOCK2 framed, MOCK3 unplug
  <scenario>                       → mock backend with just that scenario as MOCK0
```

Rules that make the seam verifiable: **`src/app` contains zero `RCOM_MOCK` references or mock conditionals** — UI mock affordances key off `PortInfo::kind == Mock(_)` (port badge) and `SessionHandle::mock_controls()` being `Some` (the drawer) only. Mock ports participate in hotplug UX through the same `Port` events as real ports. Playback controls ride the session handle as an optional extension trait so the main API stays mock-free.

### 5.8 Shipped-code reconciliation (advisory — not an amendment)

The core lane shipped ahead of this spec (#15–#24 closed, work merged). Where the shipped code differs from the draft sketches above, these are the **decisions of record** the shell ticket builds on; the sketches stay illustrative:

| Draft sketch | Shipped reality | Consequence for the shell ticket |
|---|---|---|
| Separate `RxEvent`/`TxEvent` | one `DataEvent { session_id, direction: Rx\|Tx, timestamp, bytes }` on the bus; `GLOBAL_SESSION_ID = 0` for process-global | Equivalent semantics (direction + bytes + when); no `seq` — rows order by arrival from the per-session task; UI keys off `direction`, not event variant |
| Rich `LogEvent` (raw/text/hex/garbled/hooked) | lean `LogEvent { level, timestamp, message }`; row assembly (timestamp + badge + hex + text) lives app-side over data events; the file sink writes lossless hex lines | The log VM keeps assembling rows app-side; the row model gains `garbled` (from the codec's detection) and the hook marker (from the write path's post-hook knowledge) — where exactly those flags ride is the shell ticket's implementation choice; §3.3's rendering contract is the requirement |
| `ScriptOutKind::PlotTap` per point | **superseded by #24:** process-global `PlotStore` (bounded ring per channel), UI pulls decimated snapshots per pump tick; `Event::Plot` = lifecycle notices only | Build nothing against per-point bus events; the plot page already consumes the store |
| Provisional `Net`/`Mqtt`/`Monitor`/`Plot` events | shipped as real events (nettools/mqtt/monitor/plot tickets own their schemas) | UI consumes them as shipped; §5.2's "provisional" applies only to `Settings` and `HookTrace` |
| Mock = four scenarios + `MockControl` | shipped: `MOCK0` echo plug with unplug/replug (`MockPlug`), driven by the autodemo | The scenario set (noisy/framed/unplug scripts), `MockControl`, `RCOM_MOCK` selection, and the mock drawer are the shell ticket's to add (§6) |

---

## 6. Mock core scope

### 6.1 `MockBackend` and playback controls

`MockBackend: SerialBackend` plays a **script** — a timeline of items: `Rx { after_ms, bytes }`, `Error { after_ms, message }`, `Unplug { after_ms }`, `Reappear { after_ms }`, `Note { text }`. Playback controls (`MockControl`): `pause(on)`, `set_speed(0.25×..8×)`, `step()`, `inject_rx(bytes)`, `trigger(Unplug | Error | Reappear | scenario extras)`, `restart()`. They surface as a **Mock drawer** in the session area, visible only for sessions whose `mock_controls()` is `Some`. Timelines are wall-clock-relative, scaled by speed; every scenario is deterministic at 1× with no injected triggers.

### 6.2 Scenario A — echo device (`MOCK0` / `RCOM_MOCK=echo`)

Open +100 ms: `Note` "rcom mock echo device v1"; ongoing: every `write()` echoed back byte-exact as `Rx` after 30 ms. **Demonstrates:** the full send→receive loop without hardware — send bar and quick-send produce a Tx row then an Rx row each; view modes flip both; the substring filter matches either; hide-sent leaves only echoes; a pre-send hook that rewrites TX is **visible in the echo** (what comes back is the post-hook bytes) with the hook marker on the row.

### 6.3 Scenario B — noisy sensor (`MOCK1` / `RCOM_MOCK=noisy`)

Every 500 ms: a 16-byte ASCII frame `"<ms>,T=23.4,P=1013.2\r\n"`; p≈0.2/tick: 32 random bytes with runs of 0x80–0xFF (garbled); p≈0.05/tick: a frame split mid-UTF-8 across two chunks. **Demonstrates:** encoding switching (GBK ↔ UTF-8 re-renders history from `log_snapshot`); garbled-row flagging; hex view for binary junk; follow under sustained load; chunk-boundary decode. Also the lagged-consumer path: at 8× the view must self-heal (§5.1 policy), not freeze.

### 6.4 Scenario C — framed protocol (`MOCK2` / `RCOM_MOCK=framed`)

10 frames/s: `AA 55 | len u8 | seq u16 | temp f32 | pressure f32 | volts f32 | crc8`; trigger `CorruptFrame`: one frame with a bad CRC / short payload. **Demonstrates:** the plot pipeline end to end — the sample decoder validates header+CRC, feeds the plot store, and the plot page renders 3 live curves at ~10 Hz; mock pause freezes the *plot* (decoders idle) while the log keeps filling; the corrupt trigger exercises decoder error paths (skip + note, no crash). (The plot engine and the bundled decoder already shipped via #24 — what this scenario adds is the scripted feed behind `RCOM_MOCK=framed`.)

### 6.5 Scenario D — unplug/error mid-stream (`MOCK3` / `RCOM_MOCK=unplug`)

Open +100 ms: `Note` "sensor link up"; every 1 s: a status line; open +10 s (or `trigger(Unplug)`): unplug → `Removed` + session `Unplugged`; unplug +5 s (adjustable 1–30 s): `Reappear` → `Arrived` → auto-reopen (`Reopened`). **Demonstrates:** the reconnect machine as UX — chip `Open → WatchDesired → Open`, amber banner with wait reason, log continuity across the reconnect (one scrollback, `Note` rows at both edges); repeated rapid unplugs (5× back-to-back) must not hang the reader or leak sessions; `close()` during `WatchDesired` cancels watching cleanly.

---

## 7. Shell-ticket acceptance criteria — `UI shell + mock core`

Verification mode is design.md's: build + smoke + manual demo (no CI). Every criterion is demonstrable with mock only — no hardware anywhere. *(AC 2 and AC 3 are the #30-reworded versions, marked; the rest are draft 05 §6 verbatim, with one clarifying parenthetical added to AC 5.)*

1. **Boot to traffic:** with `RCOM_MOCK=echo`, launching the app lists `MOCK0`; opening it at any baud shows the banner row within ~1 s. No real port present at any point.
2. **Send/receive loop** *(AMENDED by #30)*: on `MOCK0`, sending from the send bar and from a quick-send page produces a Tx row + echo Rx row each; the view-mode combo (`text + hex` / `hex only` / `text only`) re-renders history without new traffic; the substring filter matches decoded text and hex alike and applies to history and live rows; hide-sent leaves echoes only.
3. **Hook visibility** *(AMENDED by #30)*: with the sample pre-send hook active (rewrites TX), `MOCK0`'s echo shows the *rewritten* bytes, the send bar receipt reflects them, and the Tx log row carries the hook marker (`LogEvent.hooked`) next to the post-hook bytes.
4. **Encoding + garble:** on `MOCK1`, switching encoding to GBK re-renders history without new traffic; garbled rows are flagged; hex view shows the raw junk.
5. **Plot pipeline:** on `MOCK2`, loading the sample decoder renders 3 curves at ~10 Hz; mock pause freezes the plot (decoders idle) while the log keeps filling — stream continues, per §6.4. *(The plot page itself shipped via #24; this criterion verifies the scripted feed.)*
6. **Reconnect UX:** on `MOCK3`, the scripted unplug moves the chip `Open → WatchDesired → Open` with banner + log `Note` rows; five rapid `trigger(Unplug)` cycles in a row leave the session usable (no hang, no duplicate tabs).
7. **Multi-session:** `RCOM_MOCK=all` with `MOCK0` and `MOCK1` open — two tabs, independent scrollbacks, no cross-talk; closing one tab leaves the other streaming.
8. **Self-heal:** `MOCK1` at 8× speed for 60 s: log view stays responsive; after a deliberate stall, the view recovers via snapshot (gap visibly closes) rather than freezing or erroring.
9. **Seam cleanliness (code review, not demo):** `src/app` has no `RCOM_MOCK` or mock-specific `#[cfg]`; UI mock affordances key off `PortInfo::kind == Mock(_)` and `mock_controls()` only. Swapping `RCOM_MOCK=off` + a real port changes nothing in `src/app`.
10. **Lifecycle:** quitting the app with a mock session open exits cleanly (no hung reader); a closed mock session's port can be reopened into a fresh session.

---

## 8. Decisions taken by default (owner may revisit)

The drafts left open questions for the owner. Under #33's autonomous mandate this spec adopts each draft's recommended default; any of them is a one-line revisit:

| Question | Adopted |
|---|---|
| 02 Q1 — accent hue | teal `#0E7C8C` + derived ramp (§1.4) |
| 02 Q2 — theme default | Dark default, `Dark/Light/FollowOS` tri-state in settings (§1.2) |
| 02 Q3 — log type size | 13 on all byte surfaces, 11 timestamps/badges; per-session zoom step 12/13/14/16 in #25 (§1.8) |
| 02 Q4 + 03 Q1/Q2 — log colours | TX `#4EA3E8`; notes amber `#F7B24B`; badge chips (§1.6) |
| 02 Q5 — bundled UI font | none for v1; platform default UI family (§1.8) |
| 02 Q6 — acrylic | flat v1; pre-defined degradable token, no toggle yet (§1.3) |
| 03 Q3 — follow model | toggle + scroll-up disengage; no hover-pause; no scroll-back re-engage (§3.3) |
| 03 Q4 — quick-send HEX | none in v1 (§3.5) |
| 03 Q5 — mono size | 13 everywhere bytes are typed or shown (§1.8) |
| 03 Q6 — timestamps + hide-sent | both ship as toolbar checkboxes (timestamps on, hide-sent off) (§3.3) |
| 04 Q1 — settings destination | last bottom-pane tab (§4.9) |
| 04 Q2 — confirm mechanism | button-anchored flyout, one shared builder (§4.1) |
| 04 Q3 — quit guard | one-click-past banner (§4.1) |
| 04 Q4 — window notifications | adopt, rows 4–6 of the error matrix only (§4.1) |
| 04 Q5 — monitor placeholder | **superseded by #23** — Monitor shipped as a live page (§2.3, §4.8) |
| 04 Q6 — tab overflow | rule recorded; build nothing before the 9th page (§2.2 rule 4) |
| 05 Q1–Q6 — contract | as proposed in draft 05 (global bus + filtered per-session stream; raw-first log rows; single `RCOM_MOCK` var; post-hook-only `Tx` + provisional `HookTrace`; async `write()` through the hook; `RequiresRestart` for parity/stop while open) |

---

## Appendix A — pin realities: what does NOT exist at `=0.4.0-preview.1`

Verified in the pinned sources by draft tickets #28–#32. **No implementer may build against anything in this list.**

| Missing | Consequence for rcom |
|---|---|
| Charts crate (`ferroui-controls-charts` unpublished) | plot is the shipped custom render control (#24) |
| Code-editor control | Lua editor stays a monospaced `TextBox`; highlighting later |
| Tree/data-grid crate | no tree/table surfaces; log on virtualized `ItemsControl`/`ListBox` |
| `InfoBar` | no banner control — status lines, log rows, notifications, flyouts are the fallback set (§4.1) |
| `ContentDialog` / any modal / native message box | **never modal** (§4.1); confirms are button-anchored `Flyout`s; file dialogs only via `ferroui-dialogs`' managed choosers |
| `FontIcon` / `BitmapIcon` / `SymbolIcon` (only `IconElement` + `PathIcon`) | text labels in v1; owned path geometry later (§1.10); the theme draws its own glyphs as geometry because the Segoe symbol font is Windows-only and non-redistributable |
| `SelectorBar`, `MenuBar`, `TitleBar` control themes in the winui theme (draft 02's crate table was corrected on this) | none used; tab overflow selector is a `ComboBox`/`Expander` row |
| Reduced-motion OS API | `rcom.motion` is an rcom setting + authoring rule (§1.9) |
| Real acrylic blur outside Win32 (macOS/Linux unverified; theme acrylic brushes are solid stand-ins; native platform reports no blur) | flat v1; the future acrylic token degrades to the flat colour (§1.3) |
| `ferroui-view`'s `styles:` attribute; `CommandBar.primary_commands` as an rsx attribute (it is a `FerroList` — assemble imperatively) | pin facts from the #28 prototypes, recorded so nobody re-trips on them |
| App-level spacing scale in the theme | `rcom.space.*` is an rcom convention (§1.7) |
| `ferroui-fonts-inter` in rcom's tree | not added for v1 (§1.8) |

Platform notes: on-window rendering failed on the macOS 26/M4 dev host with a `-6661` display-timer panic **only while the display was asleep** — `caffeinate -u` before GUI runs is the whole workaround (draft 03 §8 finding 1). Dark must be set explicitly at startup or the window follows the OS (§1.2).

## Appendix B — sources

| This spec's section | Drafted by |
|---|---|
| §1 tokens | `docs/ui-drafts/02-visual-language.md` (#29), corrected by #31, colours resolved by #30 |
| §2 IA + §4.1 patterns | `docs/ui-drafts/01-shell-verdict.md` (#28) + `docs/ui-drafts/04-page-patterns.md` (#31), monitor corrected per #23 |
| §3 main window | #28 hybrid + `docs/ui-drafts/03-log-interaction.md` (#30) |
| §5 contract + §6 scenarios + §7 ACs | `docs/ui-drafts/05-contract.md` (#32), amended by #30; shipped-code reconciliation from the closed core-lane tickets (#15–#24) |
