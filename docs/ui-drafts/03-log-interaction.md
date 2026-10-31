# 03 — Log view + send/quick-send interaction (draft)

- **Ticket:** #30 (prototype — owner confirmation pending; final assembly into `docs/design-ui.md` happens in #33)
- **Refines:** `docs/ui-drafts/01-shell-verdict.md` (the per-session content layout), `docs/ui-drafts/02-visual-language.md` (the tokens this surface consumes), `docs/ui-drafts/05-contract.md` (§1.3 `Log` payload + §6 shell-ticket ACs — amended by this doc, §6 below).
- **Prototype:** `examples/log_prototype.rs` (+ `examples/log_prototype_common/`) — run it: `cargo run --example log_prototype`; headless: `cargo run --example log_prototype -- --selfcheck`; smoke: `FERROUI_SMOKE_EXIT_MS=5000 cargo run --example log_prototype` (wake the display first: `caffeinate -u -t 5`). `src/` untouched.
- **Method:** every control, property and event named here was used by the prototype and verified against the pinned sources (`ferroui-*-0.4.0-preview.1` in the registry cache). The prototype was run on the dev host and screenshotted — this ticket is the first UI on this host to be *seen*, not just built (see §8).

---

## 0. Decisions at a glance

| # | Question | Decision |
|---|---|---|
| 1 | Row anatomy | One row = `time │ badge │ content` grid; content = hex line + decoded text line stacked (llcom dual view); everything wraps; notes distinct; garbled rows washed |
| 2 | Direction colours | RX = theme success green, TX = blue `#4EA3E8`, notes = amber `#F7B24B` (not the theme Attention brush — it would follow the teal accent); badges are translucent chips, not plain text |
| 3 | Auto-scroll vs pause-on-hover | **"follow" toggle + scroll-up auto-disengages** — no pause-on-hover |
| 4 | Filter / view modes | Substring filter over text AND hex, live; view mode = combo (text + hex / hex only / text only) |
| 5 | Send bar | `HEX │ input (Enter sends) │ Send`; sends post-hook bytes; hook arm-state is a mock stand-in for the script page |
| 6 | Quick-send | Right column, 10 pages × 6 entries, edit-in-place values, `⌁ hook` badge in the header |
| 7 | Contract | `LogEvent` gains `hooked: bool`; §6 AC 2/3 amended to name the filter/mode behaviour and the hook marker |

---

## 1. Row anatomy (log rows, badges, timestamps, wrapping)

**What the prototype shows:** ~200 mounted rows + live arrivals, mixed RX/TX/notes; a GBK device banner (`品牌：测试`) and UTF-8 Chinese lines (`状态: OK`, `心跳：1`) with their true wire bytes in the hex line; 64-byte binary bursts that wrap; one garbled row (junk high bytes) in the critical pair; a hook-rewritten TX row (`poll;CHK=2E`).

**Decision — the row is a three-column grid, monospace throughout:**

```
08:24:31.497  [RX]   54 32 34 2E 33          ← hex line, dim (TextFillColorSecondary)
                     T=24.3                   ← decoded text line, primary
```

- **Columns:** timestamp 92px fixed (all content aligns regardless of text length), badge 48px, content `*`. Timestamps render local, `HH:MM:SS.mmm`, `rcom.font.mono` at 11 (content at 13 — the token draft §6 chain).
- **Dual view (llcom parity):** one log row shows the hex view AND the decoded text, stacked; the hex line is dimmer than the text line. The hex string is the preformatted `LogEvent.hex` (lowercase, space-separated — contract §1.3); the view never re-formats bytes.
- **Direction badges:** `RX` / `TX` / `NOTE` chips — direction colour as text on a translucent wash of the same colour (~18% alpha), 10px mono. Chips beat the shell draft's plain coloured text: the label survives colour-blindness and bad projectors, the wash makes a wall of rows scannable by direction at a glance. llcom's green/blue muscle memory is preserved (green = device, blue = you).
- **Note rows** (port opened, encoding switched, script started): no hex, one dim italic line prefixed `— `, amber. Distinct at a glance from data rows; not row-sized banners.
- **Garbled rows** (`LogEvent.garbled`): text + hex in the theme's critical pink (`#FF99A4` dark) over a dark-red wash (`#442726`). The wash is what makes a single bad row findable in a fast stream.
- **Hook marker:** a TX row whose payload was rewritten by the pre-send hook shows a ` ⌁hook` suffix on its text line. This is the UI consumption of the new `LogEvent.hooked` flag (§6).
- **Wrapping policy:** both lines `TextWrapping::Wrap`, always. No horizontal scrollbar, no clipping, no ellipsis. A serial log's worst line is exactly the one you need to read; horizontal scrolling breaks the follow model (the newest row is always bottom-right). Cost: a 4 KB single-chunk line grows the row tall — accepted for v1; if it hurts, a per-row expand affordance is the escape hatch, not a policy change.
- **Info/status styling** is the note row treatment above (dim italic + amber); there is no separate "status" row kind — the contract has exactly `Rx | Tx | Note`.

**Colour decision (refines token draft §9 / its open Q4):** RX = `SystemFillColorSuccessBrush` dark value `#6CCB5F`; TX = **`#4EA3E8`** — the token draft already proposed this family over the prototype-draft `#0B57D0`, which is near-invisible on `#202020`; notes = **amber `#F7B24B`, not `SystemFillColorAttentionBrush`** — after the #29 teal accent override, the Attention brush *is* the accent and a note would read as a selection highlight. Amber keeps notes clear of both the accent (teal) and the verdict colours (green/red). Badges use the direction colour at 18% alpha behind 100%-alpha text.

## 2. Auto-scroll: "follow" toggle + scroll-up disengages (not pause-on-hover)

**What the prototype shows:** the log streaming at mixed rates (5 busy ticks, 3 medium, 4 quiet per 3 s cycle), the view pinned to the newest row while "follow" is on, the status bar reading `follow on`; scrolling up disengages follow automatically (the checkbox unticks, status reads `PAUSED`); re-engage by ticking it.

**Decision: an explicit follow toggle, default on, plus scroll-up auto-disengage. Rejected: pause-on-hover.** Why:

1. Hover-pause makes the log's core reading gesture impossible: moving the mouse *into* the log to select/copy text freezes the stream, and leaving unfreezes it — the log fights the user precisely when they are working with it. The trigger for "I want to read" is not "cursor position" but "I scrolled up".
2. Scroll-up auto-disengage is the llcom muscle-memory path and needs no discovery: the log stops only after the user has already taken control of the viewport.
3. The toggle makes the state *visible* (status bar: `follow on` / `PAUSED`), which hover-pause cannot — a paused-by-hover log looks identical to a quiet port, and "why did it stop?" is the worst question a serial tool can provoke.

Deliberately **not** built: scroll-back-to-bottom re-engages follow automatically. Threshold detection at a streaming bottom edge is fiddly (off-by-one-row jitter re-engages follow while reading), and the checkbox is one click. Revisit only if the owner asks.

Mechanics: follow engages `ScrollViewer::scroll_to_end()` on every appended batch (and on filter/mode changes that grow the list) — an instant jump, no smooth scrolling: log rows never animate (token draft §7). New rows appear instantly, no per-row transition.

## 3. Filter + view modes

**What the prototype shows:** a filter box and a `text + hex / hex only / text only` combo in the log toolbar; the status bar reading `shown N / M rows`; the selfcheck asserting `P1=` narrows 201 → 43 rows and that hex-only hides decoded text while text-only hides hex.

**Decision:**

- **Substring filter, live (per keystroke), case-insensitive, matching BOTH the decoded text and the hex view.** Matching hex matters: with a hex needle (`p1 =3d` or `50 31`) you can find the raw frame when the text is garbage. This is a superset of llcom's text filter at no cost — the row already carries both strings (contract §1.3).
- **View modes are a combo, not checkboxes:** `text + hex` (default, the dual view), `hex only` (wire truth, densest — this is llcom's HEX mode), `text only`. Mutually exclusive by construction: they are three renderings of one row, not three features. Flipping the mode re-renders history in place — no new traffic, because rows re-render from the store (the prototype re-keys its rows on mode; the real VM re-renders from `log_snapshot()`).
- Filter and modes compose (filter first, then render mode). A `Clear` button empties the log (mock of the session's log-store clear).

Deliberately **not** in the prototype: hide-sent (a direction filter, contract §6 AC 2 expects it) and timestamp on/off — both are one-checkbox presentation filters for the implementation ticket; the prototype covers the mechanism they ride on (the same filtered-rows memo). Listed in §7 so they don't get lost.

## 4. Send bar

**What the prototype shows:** `☐ HEX │ text, or hex pairs when HEX is checked (e.g. 01 A0 FF) — Enter sends │ ☐ pre-send hook │ [Send]`, monospace input; Enter sends; a TX row appends with the local timestamp and, with the hook armed, the post-hook bytes (`poll\r\n` → `poll;CHK=2E\r\n`) and the ` ⌁hook` marker; invalid hex pairs append a note row instead of failing silently.

**Decision:**

- **Enter sends** (keyboard-first; the button is the discoverable affordance). llcom parity.
- **HEX toggle switches parsing, not presentation.** On: whitespace-separated hex pairs → bytes (bad input → note row with the offending text). The TX row then shows the sent bytes in both views like any other row.
- **The TX row always shows post-hook bytes** — what the wire actually saw (contract §1.3 `Tx.bytes` post-hook, §2.2 `TxReceipt`). With the hook armed, the row's text/hex are the *rewritten* payload and the ` ⌁hook` marker says so.
- **The `pre-send hook` checkbox is a mock stand-in**, placed in the send bar only so the prototype can arm the hook in one click. In the real app the hook is armed by loading a pre-send script (script page / #19's TX hook); the send bar's hook state is **read-only** — the quick-send badge (§5) is its indicator. No send-bar hook toggle ships.

Deliberately **not** in the bar: an encoding selector (session config, port toolbar — contract §2.1 boundary rule) and a per-send "add CRLF" toggle (v1 sends what you type; newline is part of the text).

## 5. Quick-send panel

**What the prototype shows:** a 300px right column — header (`Quick send` + `⌁ hook` / `hook off` badge), a 10-pip `PipsPager` with an `N/10` label, six rows of `label │ value │ send` per page; values are TextBoxes edited **in place** (no dialog, no separate form) and survive page switches; Enter in a value sends it; page 1 carries the llcom six (`ping`, `poll`, `read temp`, `reset`, `version`, `scan`), pages 2–10 generated device-command patterns.

**Decision:**

- **Edit-in-place is the whole interaction.** The value is a TextBox, always; there is nothing to "open". This is the one behaviour the ticket called out explicitly and the prototype confirms it: editing page 1's `poll` to `poll 2` and paging away and back keeps the edit. (Implementation note: each entry owns its value signal; the page switch re-keys the six visible rows but the signals live outside them.)
- **10 pages × 6 entries** (llcom's layout, compact density: `ListViewItemMinHeight` 32 territory). The pager is the pin's `PipsPager` — 10 pips fit at 300px with `max_visible_pips: 10`; if a future page count grows, `max_visible_pips` truncates gracefully.
- **TX-hook indicator badge** in the panel header: `⌁ hook` (amber text on translucent amber) while a pre-send hook is armed, `hook off` (dim) otherwise; it flips reactively with the hook state. Placement — on the quick-send panel, not the send bar — is the llcom reading: quick-send is where pre-baked commands live, so that is where "your sends will be rewritten" matters most. With #19's contract, the badge state = "a pre-send hook is loaded and non-trivial"; the prototype's checkbox mock is a stand-in for that session state.
- **Entry sends are plain-text in this prototype** (the value string's bytes). HEX for quick-send entries is an open question (§7 Q4) — llcom's exact per-page HEX affordance needs an owner check; typed hex pairs through the send bar cover the need meanwhile.

## 6. Contract amendments (exact proposed wording)

This ticket amends `docs/ui-drafts/05-contract.md` §1.3 (`Log` payload) and §6 (shell-ticket ACs), per that doc's header note. **#33 should apply these when assembling `docs/design-ui.md`.**

### 6.1 §1.3 — `LogEvent` gains `hooked: bool`

The log view must mark TX rows the pre-send hook rewrote (the ` ⌁hook` marker, §1 above). Neither `TxOrigin` (tags the *sender*, not the rewrite) nor `TxReceipt` (reaches the sender, not the log row) carries it. Amend the `LogEvent` sketch to:

```rust
pub struct LogEvent {
    pub session_id: SessionId,
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub direction: Direction,
    pub raw: Bytes,          // authoritative — the log store keeps this
    pub text: String,        // convenience decode at emit time (current encoding, lossy)
    pub hex: String,         // preformatted hex view (lowercase, space-separated)
    pub garbled: bool,       // decoder hit invalid sequences (feeds recovery UI)
    pub hooked: bool,        // NEW — pre-send hook modified the payload; TX rows only
}
```

With one sentence added to the §1.3 design note: *"`hooked` is true when the pre-send hook changed the payload on a TX row (post-hook bytes are what `raw`/`text`/`hex` already carry); the log view renders it as a hook marker."* Everything else in §1.3 survives contact with the prototype unchanged: `raw`-authoritative + emit-time conveniences is exactly the split the row renderer consumes, and `Direction::Note` covers the note-row anatomy.

*(Related, outside this ticket's amendable set — recommendation for #33:* §2.1's presentation-state list ("hex display, timestamps on/off, hide-sent, autoscroll, font size") should grow **view mode (text+hex / hex-only / text-only), substring filter, follow/auto-scroll** — the prototype treats them as the same kind of viewmodel state. *§6.1 above does not change §2.1 text.*

### 6.2 §6 — AC 2 and AC 3 amended

**AC 2 (send/receive loop), amended wording:**

> 2. **Send/receive loop:** on `MOCK0`, sending from the send bar and from a quick-send page produces a Tx row + echo Rx row each; the view-mode combo (`text + hex` / `hex only` / `text only`) re-renders history without new traffic; the substring filter matches decoded text and hex alike and applies to history and live rows; hide-sent leaves echoes only.

**AC 3 (hook visibility), amended wording:**

> 3. **Hook visibility:** with the sample pre-send hook active (rewrites TX), `MOCK0`'s echo shows the *rewritten* bytes, the send bar receipt reflects them, and the Tx log row carries the hook marker (`LogEvent.hooked`) next to the post-hook bytes.

**AC 5 and the rest of §6: untouched** (AC 5 is the plot pipeline — lane 2's). AC 4 (encoding + garble) already matches what the prototype renders: garbled rows flagged (§1's critical pair), hex view showing the raw junk.

## 7. Open questions for the owner

**Q1 — TX blue `#4EA3E8` as the final `rcom.log.tx` value?**
The prototype's rendered value (readable on `#202020`, clearly distinct from success green and the teal accent). *Recommended default: yes.* Check once against a real dark screenshot at #33; the alternative (llcom's exact `#0B57D0`) is too dark for the dark theme.

**Q2 — Notes amber `#F7B24B` instead of `SystemFillColorAttentionBrush`?**
Deviation from token draft §9 forced by the teal accent override (Attention would render teal and read as selection). *Recommended default: amber; amend `rcom.log.note` in the token table to the literal amber pair (text `#F7B24B`, wash ~18% alpha) when #33 assembles it.*

**Q3 — Confirm the follow model (toggle + scroll-up disengage), no pause-on-hover?**
The ticket asked to pick one and justify (§2). *Recommended default: as decided.* Hover-pause fights selection/copy; the toggle makes pause visible.

**Q4 — HEX mode for quick-send entries: per page, per entry, or none in v1?**
llcom's exact affordance needs a parity check against a running llcom. *Recommended default: none in v1* — the send bar's HEX toggle covers hex payloads; add a per-page toggle only on demonstrated need.

**Q5 — Mono size for byte-entry surfaces: unify on 13?**
The prototype ran the send box at 13 and quick-send values at 12; the token draft says 13 log / 14 send. *Recommended default: 13 everywhere bytes are typed or shown (log, HEX, send box, quick-send values), 11 for timestamps/badges; the per-session zoom step (12/13/14/16, token draft Q3) absorbs taste.* If the owner prefers the draft's 14 send box, that is a one-line change.

**Q6 — Timestamps hideable, and hide-sent as a toolbar checkbox?**
Both are presentation filters the contract expects (§2.1 boundary list; §6 AC 2) that the prototype left out to keep the toolbar at the ticket's scope. *Recommended default: add both as checkboxes in the implementation ticket (timestamps default on, hide-sent default off); no contract impact.*

## 8. Evidence + two pin/platform findings

- `cargo build --examples` green (shell examples untouched); `--selfcheck` exits 0 — 201 timestamped lines mounted, dual-view/GBK/UTF-8/wrapped-line/badge needles found, hex-only and text-only modes verified as mirrors, `P1=` filter narrows 201 → 43.
- `FERROUI_SMOKE_EXIT_MS=5000` smoke run exits 0; a 20 s run screenshotted twice at ~9 s and ~11 s shows the row count climbing 231 → 240 with the view pinned to the newest rows (follow working), dark theme, teal accent on the controls, badges/dual-view/garbled wash rendering as described.
- **Finding 1 — the #28 verdict's `-6661` render panic does not reproduce with a woken display.** `caffeinate -u -t 5` before launch and FerroUI windows render fine on this macOS 26 / M4 host (two shells ran headless-checked only; this ticket *saw* a window). "Wake the display before GUI runs" is the whole workaround; SHELL_CANDIDATES.md's "could not be demonstrated" should be read as "display was asleep".
- **Finding 2 — the dark default must be set explicitly; the window follows the OS otherwise.** First render came up light (macOS light mode) and the dark-tuned row colours were unreadable. Setting `RequestedThemeVariant = dark` on the window (a theme-variant root at the pin) before the main loop fixes it — the prototype does this in its `started` callback. The app must do the same at startup (token draft §4 already says so; treat this as the proof, and note the shell candidates never hit it because they were never rendered).

## 9. Note for #33 (contract sections changed)

1. **§1.3** — `LogEvent` gains `pub hooked: bool` (pre-send hook modified the payload; TX rows only) + one sentence in the §1.3 design note (§6.1 above has the exact wording).
2. **§6** — AC 2 and AC 3 reworded (§6.2 above has the exact wording); AC 5 untouched.
3. **Recommendation only (not an amendment):** §2.1's presentation-state list grows view mode + substring filter + follow (§6.1 note).
4. **Token draft (`02`) updates for #33:** `rcom.log.tx` = `#4EA3E8`, `rcom.log.note` = amber `#F7B24B` pair (Q1/Q2), badge chips added to the log-semantics row, `rcom.font.mono.size` possibly unified at 13 (Q5), dark-variant-at-startup promoted from "convention" to a proven startup step (§8 finding 2).
