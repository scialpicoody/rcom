//! The entry point of the application.
//!
//! ```text
//! cargo run
//! FERROUI_SMOKE_EXIT_MS=2000 cargo run
//! cargo build --profile dist
//! ```
//!
//! With `FERROUI_SMOKE_EXIT_MS=<n>` the main window is closed after `n` milliseconds, which
//! ends the main loop.

use rcom::App;
use ferroui_controls::AppBuilder;
use ferroui_desktop::AppBuilderDesktopExtensions;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit_code = AppBuilder::configure::<App>()
        .use_platform_detect()
        // The developer tools on F12 in a debug build: uncomment together with the
        // `ferroui-diagnostics` dependency in Cargo.toml, once it publishes:
        // .after_setup(attach_dev_tools)
        .start_with_classic_desktop_lifetime(&args);
    std::process::ExitCode::from(exit_code as u8)
}

/* Uncomment together with the `ferroui-diagnostics` dependency in Cargo.toml:
/// The developer tools on F12 in a debug build.
fn attach_dev_tools(builder: &AppBuilder) {
    #[cfg(debug_assertions)]
    if let Some(application) = builder.instance() {
        use ferroui_diagnostics::DevToolsExtensions;

        application.attach_dev_tools();
    }
    #[cfg(not(debug_assertions))]
    let _ = builder;
}
*/
