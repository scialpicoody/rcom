# 02 — Visual language: theme, material, density, motion (draft)

- **Ticket:** #29 (grilling draft — owner confirmation pending; final assembly into `docs/design-ui.md` happens in #33)
- **Refines:** `docs/design.md` (UI layout) and `docs/ui-drafts/01-shell-verdict.md` (the #28 verdict: top tab strip + modernized llcom-classic session layout).
- **Purpose:** fix the visual language so every view written from now on pulls from one token table instead of hard-coded values.
- **Method:** every FerroUI capability named below was read in the pinned sources (`~/.cargo/registry/src/*/ferroui-*-0.4.0-preview.1/`); files are cited per section. Nothing here is recalled from Avalonia or WinUI folklore — where the pin does not ship a capability, the doc says so.
- **No code ships with this ticket.** Resource keys under the `rcom.` prefix and the `launch_themed` snippets are illustrative of *where* a token lives, not an implementation commitment.

---

## 0. Decisions at a glance

| # | Question | Decision | Pin capability it rides on |
|---|---|---|---|
| 1 | Theme crate | **`ferroui-themes-winui`** (confirmed) | `WinUITheme` + full WinUI 3 resource set |
| 2 | Material | **Flat surfaces; acrylic is an optional, degradable setting** | `TransparencyLevelHint` + `ExperimentalAcrylicMaterial` (real blur verified on Win32 only) |
| 3 | Accent colour | **Teal `#0E7C8C` ramp, overriding the 7 `SystemAccentColor*` resources app-level** | `SystemAccentColors` resource chain |
| 4 | Dark / light | **Dark default; tri-state Dark/Light/Follow-OS in settings (#25)** | `Application.RequestedThemeVariant` |
| 5 | Density | **`DensityStyle::Compact` (as the #28 prototypes already run)** | `WinUITheme::set_density_style` + 4px spacing scale (convention) |
| 6 | Type ramp | UI = theme ramp (Body 14 base); **log/hex/send/terminal = monospace fallback chain** | `FontFamily::parse` comma list; theme font-size keys |
| 7 | Motion | **Theme's own duration tokens; opacity/brush only; log rows never animate; explicit reduced-motion setting** | `Transitions` + `Control*AnimationDuration` tokens |
| 8 | Icons | **Text labels for v1; if icons later, `PathIcon` + app-owned geometry — never the symbol font** | `PathIcon` (`Data: Geometry`); theme's `GlyphGeometry*` precedent |

---

## 1. Theme crate — `ferroui-themes-winui` (confirmed)

**Decision: the WinUI 3 theme, exactly as the #28 prototypes styled with it.** `WinUITheme.xaml` merges ~60 resource dictionaries ported from microsoft-ui-xaml at commit `7e4b0c2096be402f51cc8f7d203ebf67cff183d0` (generic.xaml + XamlControlsResources; header comment of `ferroui-themes-winui-0.4.0-preview.1/WinUITheme.xaml`), with light / dark / **high contrast** dictionary variants per document.

Why winui, against the alternatives shipped at this pin:

| Crate | What it ships (read in sources) | Why not |
|---|---|---|
| `ferroui-themes-winui` | Faithful WinUI 3 port: `TabView`, `CommandBar`, `Expander`, `InfoBar`, `MenuBar`, `SelectorBar`, `TitleBar`, `SplitView`, … (82 control themes in `Controls/`); `DensityStyle::Compact`; `high_contrast()` variant; accent follows the **system** (`Accents/system_accent_colors.rs`) | — (this is the one) |
| `ferroui-themes-fluent` | Avalonia Fluent port; same `DensityStyle`; **plus `palettes()` (`ColorPaletteResourcesCollection`) for over-riding the accent** (`fluent_theme.rs:157`) | A second visual language to document; the tab-strip shell was prototyped against WinUI's `TabView` resources; its built-in accent palette is nice but the winui override is one resource dictionary anyway |
| `ferroui-themes-simple` | Minimal visual set (80 control themes, non-WinUI look) | Third language, no WinUI-class surface; nothing in rcom's target look |

Extra winui facts that matter downstream:

- It is the framework's **default theme** (ferroui-native README: "the WinUI 3 theme (`ferroui-themes-winui`), its default theme").
- `WinUITheme::high_contrast()` exists as a third `ThemeVariant` (`winui_theme.rs:100`) alongside `ThemeVariant::light()/dark()` (`ferroui-base/styling/theme_variant.rs:57-67`). The theme's dictionaries are keyed `Default` (= dark) / `Light` / `{WinUITheme.HighContrast}` (seen throughout `Resources/*.xaml`). rcom gets high contrast for free once #25 exposes the tri-state — a four-state (add "High contrast") can wait.
- Accent in this theme is **not overridable via a palette object** (winui has none — `winui_theme.rs` says so); you override the accent *resources* (§3). That is the one fluent advantage we give up, and it costs us ~10 lines of XAML.

## 2. Material — flat by default; acrylic only as a degradable setting

**Decision: flat, solid backgrounds.** `ApplicationPageBackgroundThemeBrush` / `SolidBackgroundFillColorBase` everywhere; **no acrylic, no mica in v1.** Offer "translucent backdrop" later as a settings toggle only if someone asks, built exactly on the API below so it degrades to flat.

What the pin actually has (all verified):

- **Window transparency levels**: `WindowTransparencyLevel::{none, transparent, blur, acrylic_blur, mica}` (`ferroui-controls/window_transparency_level.rs`), requested via `TopLevel.TransparencyLevelHint` (an ordered `WindowTransparencyLevelCollection`) with the platform's answer observable as `ActualTransparencyLevel` (`ferroui-controls/top_level.rs:338-354`).
- **In-app acrylic**: `ExperimentalAcrylicMaterial` (`TintColor`, `TintOpacity`, `MaterialOpacity`, `FallbackColor`, `BackgroundSource`, `PlatformTransparencyCompensationLevel`) in `ferroui-base/media/experimental_acrylic_material.rs`, drawn by the `ExperimentalAcrylicBorder` control (`ferroui-controls/experimental_acrylic_border.rs`), which auto-compensates per the host's `ActualTransparencyLevel`.
- **Platform reality**: the Win32 backend implements real composition effects (`WinUIGaussianBlurEffect`, acrylic backdrop brush in `ferroui-win32/win_rt/composition/win_ui_effect_base.rs`) and reports compensation levels `(transparent 1.0, blur 0.8, acrylic 0.0)` (`ferroui-win32/window_impl.rs:2346`). The default/native platform reports `(1.0, 0.0, 0.0)` — **transparent yes, blur no** (`ferroui-native/top_level_impl.rs:435`). macOS/Linux blur is *not* verified at this pin (and the #28 verdict records that on-window rendering could not even be demonstrated on the macOS 26 dev host — `-6661` display-timer panic).
- The theme's `AcrylicBrush_themeresources.xaml` ships acrylic-named brushes but they are **solid-colour stand-ins** (e.g. `SystemControlAcrylicWindowBrush` = solid `SystemChromeMediumColor`; `DesktopAcrylicTransparentBrush` = fully transparent) — there is no real material behind them.

Consequence: **the visual language must not depend on blur.** Design every surface to look correct flat; the acrylic tokens below exist only so a future toggle has defined values.

| Token | Value (dark) | Value (light) | Source of truth | Used by |
|---|---|---|---|---|
| `rcom.backdrop` | `SolidBackgroundFillColorBaseBrush` (#202020 / #F3F3F3) | same key | theme resource (`Common_themeresources_any.xaml`) | window root, log pane background |
| `rcom.surface` | `LayerFillColorDefaultBrush` | same | theme resource | quick-send card, bottom pane, dialogs |
| `rcom.surface.raised` | `CardBackgroundFillColorDefaultBrush` (#0DFFFFFF on dark) | same | theme resource | hover cards, flyouts |
| *(future)* `rcom.backdrop.acrylic` | `ExperimentalAcrylicMaterial { TintColor=#202020, TintOpacity=0.85, FallbackColor=#202020 }`, hint `[acrylic_blur, blur, none]` | TintColor=#F3F3F3 | code, per settings toggle | window root only; **fallback colour always = the flat token** |

## 3. Accent colour — teal ramp, overriding the `SystemAccentColor*` resources

**Decision: a calm technical teal as rcom's own accent, applied by overriding the seven `SystemAccentColor*` colour resources at application level.** Not "follow the OS accent": rcom's RX/TX semantics, log highlighting and quick-send accents need a hue that is stable across machines and screenshots (a serial tool's log colours are part of its users' muscle memory — llcom users recognise their green/blue rows).

How the mechanism works at the pin (read in `Accents/system_accent_colors.rs`):

- The theme serves `SystemAccentColor`, `SystemAccentColorDark1..3`, `SystemAccentColorLight1..3` from a `ResourceProvider` that reads the platform's accent (Windows `UISettings`), defaulting to WinUI's `#0078D7`, and computes the six shades by fixed HSL-lightness steps (dark: −28.5/255, −49/255, −74.5/255; light: +39/255, +70/255, +103/255 — `system_accent_colors.rs:170-193`).
- Every accent brush in the theme chains onto those colours with `DynamicResource`: on **dark**, `AccentFillColorDefaultBrush` ← `SystemAccentColorLight2`, `AccentTextFillColorPrimaryBrush` ← `SystemAccentColorLight3`; on **light**, `AccentFillColorDefaultBrush` ← `SystemAccentColorDark1`, accent text ← `Dark2` (`Common_themeresources_any.xaml:135-137, 339-341`). Secondary/tertiary accent fills are the same colour at Opacity 0.9/0.8.
- Resource lookup walks element → logical tree → global styles, so a colour resource declared at **application level shadows the theme's** (`ferroui-base/styled_element.rs:567-577`). Override is therefore pure XAML: declare the seven `<Color x:Key="SystemAccentColor…">` in app resources; no theme patching.

**The rcom accent: `#0E7C8C`** — teal-cyan, hue ≈ 188°, saturation 82%, lightness 30%. Rationale: far enough from the status hues the log already uses (success green ≈ 130°, error pink/red, amber) that accent never reads as a log verdict; far enough from Windows' default blue (#0078D7, ≈ 206°) that a stock-Windows screenshot shows rcom's identity; mid lightness so the theme's own shade algorithm produces a usable ramp in *both* variants (dark surfaces consume Light2/Light3, light surfaces consume Dark1/Dark2).

The seven tokens (values computed with the theme's own step algorithm, so they are exactly what the theme would derive if only the base were overridden; the app overrides all seven explicitly so they can be hand-tuned later without re-deriving):

| Resource key (override at app level) | Value | Where the theme spends it |
|---|---|---|
| `SystemAccentColor` | `#0E7C8C` | light-variant attention brush, accent strokes |
| `SystemAccentColorDark1` | `#094E58` | **light** variant: `AccentFillColorDefaultBrush`, accent text (Dark2) |
| `SystemAccentColorDark2` | `#052D33` | **light** variant: `AccentTextFillColorPrimaryBrush` |
| `SystemAccentColorDark3` | `#000405` | rarely consumed (near-black tail of the ramp) |
| `SystemAccentColorLight1` | `#15BBD3` | (free for app use: pressed/selected states) |
| `SystemAccentColorLight2` | `#3BD5EB` | **dark** variant: `AccentFillColorDefaultBrush`, `SystemFillColorAttentionBrush` |
| `SystemAccentColorLight3` | `#77E2F1` | **dark** variant: `AccentTextFillColorPrimary/SecondaryBrush` |

App-side tokens (never hard-code the hexes a second time):

| Token | Value | Source of truth | Used by |
|---|---|---|---|
| `rcom.accent.fill` | `AccentFillColorDefaultBrush` | theme resource (after the override) | open button, active tab indicator, selection highlights, primary CTA |
| `rcom.accent.text` | `AccentTextFillColorPrimaryBrush` | theme resource | links, focused-values in the port toolbar |
| `rcom.accent.selection` | `SystemControlHighlightListAccentVeryHighBrush` | theme resource (`Common_themeresources.xaml`) | list/log row selection |

## 4. Dark / light default — dark, with a runtime tri-state

**Decision: dark is the default** (serial consoles are dark-domain tools; llcom is dark; the log is the app). Mechanism at the pin:

- Variants: `ThemeVariant::default()`, `::light()`, `::dark()` (`ferroui-base/styling/theme_variant.rs:57-67`), plus `WinUITheme::high_contrast()` (`winui_theme.rs:100`).
- App level: `Application.RequestedThemeVariant` / `ActualThemeVariant` styled properties (`ferroui-controls/application.rs:185-206`); any element can scope a subtree with `ThemeVariantScope` (`ferroui-controls/theme_variant_scope.rs`) — rcom does not need it in v1.
- Runtime switch = set `Application.RequestedThemeVariant` again; every `ThemeDictionaries`-keyed document in the theme re-resolves (verified pattern: `Default`/`Light` dictionaries in `Resources/Common_themeresources_any.xaml`). The #32 contract already carries the trigger: `Event::Settings(SettingsEvent::Changed { path })` (§1.5 of `05-contract.md`) → the app sets the variant; theme + language live-apply, no restart.
- On Windows the native frame follows: the win32 backend takes `set_frame_theme_variant` (`ferroui-win32/window_impl.rs:2352`), so the title bar goes dark with the content.

| Token | Value | Source of truth | Used by |
|---|---|---|---|
| `rcom.theme` | `Dark` (default) · `Light` · `FollowOS` — settings field, owner question Q2 | convention → `Application.RequestedThemeVariant` | #25 settings page; startup path sets it before the window opens |

## 5. Density and spacing — compact, on a 4px scale

**Decision: `WinUITheme::set_density_style(DensityStyle::Compact)`** — the #28 prototypes already launch this way (`examples/shell_b.rs:75-77`: "a tool, not a document app"). The compact dictionary is WinUI's own (`DensityStyles/Compact.xaml`, ported from `compact_themeresources.xaml`) and the theme looks it up first when set (`winui_theme.rs:160-169`).

What Compact actually changes (measured: normal value → compact value, from `Resources/*.xaml` vs `DensityStyles/Compact.xaml`):

| Resource key | Normal | Compact | rcom beneficiary |
|---|---|---|---|
| `ControlContentThemeFontSize` / `ContentControlFontSize` | 14 | 14 | (unchanged; compact is about heights/padding) |
| `TextControlThemeMinHeight` | 32 | **24** | send bar, quick-send entry, toolbar combos |
| `TextControlThemePadding` | 10,5,6,6 | **2,2,6,1** | same |
| `ComboBoxMinHeight` | 32 | **24** | port/baud/encoding selectors |
| `ListViewItemMinHeight` | 40 | **32** | log rows, port list, quick-send entries |
| `TreeViewItemMinHeight` | (n/a) | **24** | future tools-page trees |

**rcom spacing scale (convention tokens — the pin has per-control thickness resources like `TextControlThemePadding` but no app-level spacing scale, so we define one; 4px base, named for intent):**

| Token | Value | Source of truth | Used by |
|---|---|---|---|
| `rcom.space.0` | 0 | convention | flush joins (log pane ↔ send bar) |
| `rcom.space.1` | 4 | convention | icon↔label, chip inner padding, log-row inner leading |
| `rcom.space.2` | 8 | convention | related-control gaps (port ▾ / baud ▾ / open), quick-send cell padding |
| `rcom.space.3` | 12 | convention | group gaps inside the toolbar, bottom-pane tab padding |
| `rcom.space.4` | 16 | convention | pane gutters (log ↔ quick-send), card padding |
| `rcom.space.6` | 24 | convention | page margins, dialog content padding |
| `rcom.space.8` | 32 | convention | between major regions (rare; settings page) |

Rules: only these values in `Margin`/`Padding`/`Spacing` (theme control templates excluded — they carry their own ported values); when a WinUI resource exists for a control's padding (e.g. `TextControlThemePadding`), use the resource, not the scale.

## 6. Type ramp — WinUI sizes for UI, monospace chain for bytes

**Decision:** UI text rides the theme's font-size resources; **every surface that shows or takes bytes** (log view, HEX columns, send box, quick-send entries, terminal page, Lua editor) uses a monospace fallback chain. Sizes are the WinUI ramp as shipped — do not invent intermediate sizes.

Theme ramp verified in `Resources/TextBlock_themeresources*.xaml` (Compact keeps Body at 14):

| Resource key | Value | rcom use |
|---|---|---|
| `CaptionTextBlockFontSize` | 12 | toolbar labels, tab-strip secondary text, chip text, timestamps |
| `BodyTextBlockFontSize` / `ControlContentThemeFontSize` | 14 | default control text, buttons, combos |
| `BodyStrongTextBlockFontSize` | 14 (semibold style) | pane headers ("quick-send", "script") |
| `BodyLargeTextBlockFontSize` | 18 | reconnect banner text |
| `SubtitleTextBlockFontSize` | 20 | page/dialog titles |
| `TitleTextBlockFontSize` | 28 | unused (v1 has no in-window titles — the tab strip is the shell) |
| `TitleLargeTextBlockFontSize` / `DisplayTextBlockFontSize` | 40 / 68 | unused |

Monospace at the pin:

- The default UI family is the platform's (`IFontManagerImpl::get_default_font_family_name`, `ferroui-base/platform/i_font_manager_impl.rs:11`) — no theme-shipped font. `ferroui-fonts-inter` exists in the release's crate family but is **not in rcom's dependency tree**; do not add it for v1 (see open question Q5).
- `FontFamily::parse` takes a **comma-separated fallback chain** (`ferroui-base/media/font_family.rs:28`), which is exactly how the #28 prototypes already ship monospace (`examples/shell_common/mod.rs:22-24`): `Cascadia Mono, Consolas, Menlo, DejaVu Sans Mono` — lands on a real mono face on Windows 10+/macOS/most Linux; the UI framework renders whatever the OS resolves.

| Token | Value | Source of truth | Used by |
|---|---|---|---|
| `rcom.font.mono` | `FontFamily::parse("Cascadia Mono,Consolas,Menlo,DejaVu Sans Mono")` | code constant (one definition, shared) | log view, HEX column, send box, quick-send entries, terminal, Lua editor, plot axis ticks |
| `rcom.font.mono.size` | 13 (log + HEX) · 14 (send box, terminal) | convention (owner question Q3) | same |
| `rcom.font.ui` | unset (platform default family) | theme default | everything else |

## 7. Motion — opacity and brush only, theme durations, log never animates

**Decision: rcom adds no choreography.** A serial tool's core loop is "bytes arrive, rows appear" — motion budget goes entirely to state feedback. Where a transition helps (hover, selection, chip state changes), use the theme's own duration tokens and nothing faster/slower.

What the pin ships (all read, not assumed):

- **Transitions**: a `Transitions` property collection with 17 typed transitions (`ferroui-base/animation/transitions/`: `Brush`, `Color`, `Double`, `Float`, `Thickness`, `CornerRadius`, `Point`, `Size`, `TransformOperations`, `BoxShadows`, …). Key-frame `Animation`/`KeyFrames` also exist (`ferroui-base/animation/key_frames.rs`).
- **The theme's own usage is the house style** — hover/press brush cross-fades and the ToggleSwitch knob: `<BrushTransition Property="Background" Duration="0:0:0.083" />` (Button, `Controls/Button.xaml`), `<DoubleTransition Property="Canvas.Left" Duration="0:0:0.367" Easing="0.1,0.9,0.2,1" />` (ToggleSwitch knob, `Controls/ToggleSwitch.xaml`).
- **Duration tokens** exist and should be referenced, not re-literalised (`Resources/Common_themeresources_any.xaml:604-608`): `ControlFasterAnimationDuration` **83 ms**, `ControlFastAnimationDuration` **167 ms**, `ControlNormalAnimationDuration` **250 ms**, easing `ControlFastOutSlowInKeySpline` = `0,0,0,1`.
- **No reduced-motion API at the pin** (no `reduced` / `animations-enabled` query anywhere in `ferroui-base/platform/`) — reduced motion is therefore an rcom setting and an authoring rule, not something we can read from the OS.

Rules:

| Rule | Detail |
|---|---|
| Animatable properties | **Opacity, brush colours, and the theme's own template transitions only.** Never animate layout (`Width`/`Height`/`Margin`/`FontSize`) or scroll offsets |
| Durations | hover/press/selection → `ControlFasterAnimationDuration` (83 ms); chip/banner state colour → same; flyouts/dialogs → `ControlFastAnimationDuration` (167 ms); hard cap for anything app-added: `ControlNormalAnimationDuration` (250 ms) |
| Easing | `ControlFastOutSlowInKeySpline` (0,0,0,1) for enter/exit; linear for progress only |
| **Log rows** | **never animate.** New rows appear instantly; autoscroll jumps, no smooth-scroll; no entrance/fade-in per row (a 1 k row/s stream must not pay per-row transition cost) |
| Tab strip | selection = instant background swap (the theme's 83 ms brush fade is enough); no tab slide/grow |
| Reconnect chip/banner | colour cross-fade only (83 ms); the amber "waiting for port…" banner appears instantly, no slide |
| Reduced motion | `rcom.motion` = `Full` (default) / `Reduced`. When Reduced: the app attaches **zero** transitions (the one-line rule: no `Transitions` setter outside control templates); theme-internal 83 ms hover fades stay (below the flicker threshold, and removing them would mean forking control themes) |

## 8. Icons — text labels in v1; `PathIcon` + owned geometry if icons come

What the pin has (read, not guessed):

- `ferroui-controls` ships **`IconElement` (base) + `PathIcon` (`Data: Option<Ref<Geometry>>`)** — `icon_element.rs`, `path_icon.rs:35`. **No `FontIcon`, no `BitmapIcon`, no `SymbolIcon`** at this pin.
- The theme itself needs Segoe Fluent Icons glyphs (WinUI templates) but the font is Windows-only and non-redistributable, so it **draws them as path geometry resources** — `GlyphGeometry<codepoint>` StreamGeometry keys in `Resources/Glyphs.xaml`, with `SymbolThemeFontFamily` ("Segoe Fluent Icons,Segoe MDL2 Assets", `Resources/Generic.xaml:16`) remaining Windows-bound. That is the framework telling us the pattern: **geometry, not symbol fonts.**

Decision for rcom:

| Aspect | Decision |
|---|---|
| v1 | **Text labels only** — the #28 shell already works this way (tab titles, `+` button, send button). Nothing in the llcom-classic layout needs icons to be usable |
| If/when icons are needed | `PathIcon` with rcom-owned `StreamGeometry` resources in an app dictionary (`rcom.icon.*`, 16×16 ink, 1px strokes, current-color via the brush the template binds); never the symbol font |
| Tab close/`+` | text glyphs ("+", "✕") are acceptable — universal characters, not icon-font glyphs |
| Emoji in log | platform emoji rendering exists (HarfBuzz + Skia colour emoji, framework README) — logs render whatever the device sent; that is content, not UI iconography |

---

## 9. Consolidated token table

Every rcom token in one place (`rcom.` prefix = declared in app resources; theme keys = used as-is, referenced by key).

| Token | Value | Source of truth | Used by |
|---|---|---|---|
| **Theme** | | | |
| theme crate | `ferroui-themes-winui =0.4.0-preview.1` | `Cargo.toml` (present) | app root |
| theme density | `WinUITheme::set_density_style(DensityStyle::Compact)` | code (startup), as `examples/shell_b.rs` | whole app |
| `rcom.theme` | `Dark` default · `Light` · `FollowOS` | convention → `Application.RequestedThemeVariant` | startup, #25 settings |
| **Surfaces** | | | |
| `rcom.backdrop` | `SolidBackgroundFillColorBaseBrush` | theme resource | window root, log pane |
| `rcom.surface` | `LayerFillColorDefaultBrush` | theme resource | quick-send card, bottom pane |
| `rcom.surface.raised` | `CardBackgroundFillColorDefaultBrush` | theme resource | flyouts, hover cards |
| *(future)* `rcom.backdrop.acrylic` | `ExperimentalAcrylicMaterial` teal-tinted, fallback = `rcom.backdrop` | code, settings-gated | window root only |
| **Accent** (app-level `<Color>` overrides) | | | |
| `SystemAccentColor` | `#0E7C8C` | app resource dictionary | theme accent chain |
| `SystemAccentColorDark1..3` | `#094E58` `#052D33` `#000405` | app resource dictionary | light-variant accent fills/text |
| `SystemAccentColorLight1..3` | `#15BBD3` `#3BD5EB` `#77E2F1` | app resource dictionary | dark-variant accent fills/text |
| `rcom.accent.fill` | `AccentFillColorDefaultBrush` | theme resource | open button, active tab, selection |
| `rcom.accent.text` | `AccentTextFillColorPrimaryBrush` | theme resource | links, focus text |
| **Log semantics** | | | |
| `rcom.log.rx` | `SystemFillColorSuccessBrush` (#6CCB5F dark / #0F7B0F light) | theme resource | RX direction marker |
| `rcom.log.tx` | accent-adjacent blue — see Q4 (prototype: `#0B57D0`) | convention → app resource | TX direction marker |
| `rcom.log.note` | `SystemFillColorAttentionBrush` (= accent on dark) or amber — see Q4 | convention → app resource | session notes, reconnect banners |
| `rcom.log.garbled` | `SystemFillColorCriticalBrush` (#FF99A4 dark / #C42B1C light) + `CriticalBackgroundBrush` row wash | theme resource | garbled-row flagging |
| **Spacing** | `rcom.space.{0,1,2,3,4,6,8}` = 0/4/8/12/16/24/32 | convention | all layout gaps/paddings |
| **Corner radius** | `ControlCornerRadius` 4 · `OverlayCornerRadius` 8 | theme resource (`CornerRadius_themeresources.xaml`) | controls (auto), dialogs/flyouts (auto) |
| **Type** | | | |
| `rcom.font.mono` | `Cascadia Mono,Consolas,Menlo,DejaVu Sans Mono` | code constant | log, HEX, send, terminal, editor |
| `rcom.font.mono.size` | 13 (log/HEX) · 14 (send/terminal) | convention (Q3) | same |
| UI ramp | `CaptionTextBlockFontSize` 12 · `Body…` 14 · `BodyLarge…` 18 · `Subtitle…` 20 | theme resources | all non-byte text |
| **Motion** | | | |
| durations | `ControlFasterAnimationDuration` 83 ms · `ControlFastAnimationDuration` 167 ms · `ControlNormalAnimationDuration` 250 ms | theme resources | all app-added transitions (cap: 250 ms) |
| easing | `ControlFastOutSlowInKeySpline` (0,0,0,1) | theme resource | enter/exit |
| `rcom.motion` | `Full` (default) · `Reduced` | convention (setting) | gates every app-added `Transitions` setter |
| **Icons** | text labels (v1); `rcom.icon.*` StreamGeometry via `PathIcon` (later) | convention | tab strip, toolbars |

## 10. Open questions for the owner

**Q1 — Accent hue: teal `#0E7C8C` as proposed, or a different technical hue?**
Alternatives seen while choosing: deeper petrol `#0B6E78`, colder cyan `#0E7C9C`, or simply following the OS accent (rejected: unstable log colour identity across machines).
*Recommended default: `#0E7C8C` with the derived 7-shade ramp in §3.* Ship it once, look at real screenshots in both variants, adjust only if it fights the success-green on screen.

**Q2 — Theme default: dark-as-proposed, or FollowOS default?**
*Recommended default: **Dark** default with an explicit `Dark | Light | FollowOS` tri-state in settings (#25).* Serial work happens in dark terminals; FollowOS as default would give new users a white log on macOS light mode. FollowOS remains available for OS-synced users. High contrast comes free via `WinUITheme::high_contrast()` — expose a fourth state only if someone needs it.

**Q3 — Log type size: 13 px mono (proposed), 12 px (denser), or 14 px (matches Body)?**
*Recommended default: 13 for log/HEX, 14 for send/terminal, plus a per-session zoom step (12/13/14/16) in #25's presentation settings.* 12 gets cramped for hex pairs at long sessions; 14 wastes ~20% rows. The zoom step matters more than the default.

**Q4 — Log direction colours: keep llcom's green/blue pairing?**
Proposed: RX = `SystemFillColorSuccessBrush` (theme), TX = a fixed blue `#4EA3E8`-family token (readable on dark; the prototype's `#0B57D0` is too dark for the dark theme), notes = `SystemFillColorAttentionBrush`, garbled = `SystemFillColorCriticalBrush`.
*Recommended default: as proposed, final TX/note hexes picked against real dark-theme screenshots during the shell implementation ticket (not blocking this doc).* The llcom muscle-memory argument (green = device, blue = you) is the tiebreaker if the owner wants to keep llcom's exact hues instead.

**Q5 — Bundle a UI font (`ferroui-fonts-inter`) for a consistent cross-OS UI face?**
The pin supports it (the crate exists in the release family) but it is not in rcom's tree, and platform-default UI text is one less thing to ship.
*Recommended default: no for v1* — the log/mono chain carries the identity; revisit only if macOS/Windows UI text side by side looks jarring in real use.

**Q6 — Acrylic backdrop: confirm "flat v1, optional setting later", or invest in it now?**
*Recommended default: flat v1.* Verified pin facts: real blur exists on Win32 only; the native platform reports no blur; the theme's acrylic brushes are solid stand-ins. A toggle would be a small settings surface + one `ExperimentalAcrylicMaterial` at the window root with `FallbackColor` = the flat token, so nothing in this doc blocks it — but nothing in rcom's audience demands it either.
