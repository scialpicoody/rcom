---
name: ferro-crates
description: FerroUI crate map and version anchoring. Use when choosing FerroUI crates or features, wiring dependencies, or resolving FerroUI sources for the pinned version — the shared reference every other ferro skill anchors through.
---

# FerroUI crates

FerroUI ships as ~50 crates released together at one exact version. Consumers pin it (`ferroui-base = "=0.4.0"`); a release's crates resolve only together. Keep every `ferroui-*` crate on the same exact pin (`=x.y.z`), never a semver range — a mixed-pin build mixes releases.

## Anchor to the pin

Before writing or citing any FerroUI API, resolve the pin from the consuming project's `Cargo.toml` — the only filesystem assumption. Sources for `<pin>`, in order:

1. **Registry cache** (offline, authoritative for API): `~/.cargo/registry/src/*/ferroui-<crate>-<pin>/`, present for every dependency once built.
2. **docs.rs** (online): `https://docs.rs/<crate>/<pin>`.
3. **Repo content cache** (online, once per pin): docs, samples, and the workspace `Cargo.toml` are not published to crates.io — fetch the matching tag:
   `git clone --depth 1 --branch v<pin> https://github.com/wieslawsoltes/FerroUI ~/.cache/ferroui/<pin>`
   Reuse when present; confirm with `git -C ~/.cache/ferroui/<pin> describe --tags`. Samples and docs are named entry points inside this content.
4. **A local FerroUI clone** is an accelerator only: verify it matches `<pin>` (`git describe --tags`) before trusting it; the pin wins any disagreement.

Completion bar: every crate, feature, and API you name is one you saw in the pinned version's sources — not memory, and not Avalonia knowledge (FerroUI is Avalonia-shaped but renamed; treat Avalonia recall as a hypothesis to verify).

## Crate families

| Family | Crates | Role |
|---|---|---|
| Core | `ferroui-base`, `ferroui-controls` | Property system, trees, layout, input, styling, binding; the control set |
| App | `ferroui-desktop`, `ferroui` (facade) | Desktop entry and `AppBuilder`; the facade re-exports with platform/theme features |
| XAML | `ferroui-markup`, `ferroui-markup-xaml`, `ferroui-build` (build-dep) | `build.rs` compiles `.xaml` to Rust; `ferroui-markup` is the runtime |
| Declarative | `ferroui-view`, `ferroui-view-macros` | `rsx!`, components, hooks, signals, reconciler — UI without XAML |
| Themes | `ferroui-themes-winui` (default), `-fluent`, `-simple`, `-material`, `-macos` | Each theme is a crate shipping a ControlTheme XAML set |
| Extended controls | `ferroui-controls-charts`, `ferroui-controls-color-picker`, `ferroui-controls-inking`, `ferroui-controls-treedatagrid` (+ `ferroui-treedatagrid-core`), `ferroui-dialogs` | Charts, colour picking, ink canvas, virtualized tree/table grid, managed file dialogs |
| MVVM | `ferroui-reactive`, `ferroui-dynamicdata`, `ferroui-reactiveui`, `ferroui-reactiveui-controls`, `ferroui-splat` | Reactive/MVVM stack, one crate per upstream .NET library |
| Behaviors | `ferroui-xaml-behaviors`, `ferroui-xaml-interactivity`, `ferroui-xaml-interactions*` | Triggers, actions, ~330 behaviors (drag/drop, responsive, animations, events) |
| Testing | `ferroui-headless`, `ferroui-reactive-testing`, `ferroui-reactiveui-testing` | UI tests without a screen; reactive test scheduling |
| Tools | `ferroui-diagnostics` (F12 devtools), `ferroui-hotreload`, `ferroui-mcp` (agent inspection), `ferroui-headless-vnc`, `ferroui-fonts-inter` | Dev-time |
| Backends | `ferroui-skia` (default), `-vello`, `-metal`, `-vulkan`, `-opengl`, `-wayland`, `-x11` | Pulled transitively; rarely named directly |

Membership drifts between releases and some crates stay unpublished. Exact list at the pin: the workspace `Cargo.toml` of the repo content cache, the facade's `Cargo.toml`, or docs.rs.

## Where answers live in the repo content

| Question | Source (repo-relative) |
|---|---|
| Does X exist / what is ported | `docs/porting/CRITICAL-PATH.md`, `TRACKING.md`, `REMAINING.md` |
| Declarative UI model and API | `docs/declarative-ui.md` (React mapping: `declarative-ui-react.md`) |
| MVVM | `docs/mvvm.md` |
| Agent-driven UI inspection | `docs/mcp.md` |
| Release history / breaking changes | `CHANGELOG.md` |
