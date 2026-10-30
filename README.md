# rcom

a serial tool write in rust with lua

A [FerroUI](https://github.com/wieslawsoltes/FerroUI) application generated from
[FerroTemplate](https://github.com/scialpicoody/FerroTemplate)
(UI style: **xaml**, theme: **winui**, FerroUI `=0.4.0-preview.1`).

## Getting started

```sh
cargo run

# Close the window after n milliseconds (a smoke run):
FERROUI_SMOKE_EXIT_MS=2000 cargo run

# The build that is shipped (whole-program optimisation, symbols stripped):
cargo build --profile dist
```

## What is inside

- A `lib` + `bin` split: `src/lib.rs` holds the interface, `src/main.rs` the entry point, so
  the interface compiles as a testable unit (`cargo test`).
- The interface in XAML: `App.xaml` (the application and its theme) and `MainWindow.xaml`,
  each compiled to Rust source by `build.rs` (`ferroui-build`). A class is populated from its
  document by `initialize_component()` (`src/app.rs`, `src/main_window.rs`).
- The developer tools on F12 in a debug build: the wiring is in place and commented out in
  `Cargo.toml` and `src/main.rs`, ready once `ferroui-diagnostics` publishes to crates.io.
- Pinned FerroUI dependencies: every FerroUI crate is named at exactly `=0.4.0-preview.1`.
- A `dist` release profile and CI (format, clippy, tests on Linux, Windows and macOS).

## Platform prerequisites

The first build downloads a prebuilt Skia for the target and builds HarfBuzz from source
with a C++ compiler; it can take a while.

| Platform | Needs |
|---|---|
| macOS | macOS 11 or later, the Xcode command line tools |
| Windows | x64 or ARM64, the MSVC build tools |
| Linux | the development packages of fontconfig and FreeType (`libfontconfig1-dev`, `libfreetype6-dev`); the system libraries (Xlib, GTK, EGL, ...) are opened at run time, none is linked |

See the [platform documents](https://github.com/wieslawsoltes/FerroUI/tree/main/docs/porting)
of FerroUI for the details, and for the browser, iOS and Android hosts, which this template
does not scaffold.

## Where to read on

- [FerroUI](https://github.com/wieslawsoltes/FerroUI): the framework, its README and samples.
- [Declarative Rust UI](https://github.com/wieslawsoltes/FerroUI/blob/main/docs/declarative-ui.md):
  `rsx!`, builders, signals, components and hooks (the `xaml` style).
- [Themes](https://github.com/wieslawsoltes/FerroUI/tree/main/src#readme): what the
  `winui` theme and the others provide.
