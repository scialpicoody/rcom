//! a serial tool write in rust with lua
//!
//! A FerroUI application whose interface is written in XAML and compiled to Rust source by
//! the build script of the crate (`build.rs`): `App.xaml` (the application, with its theme)
//! and `MainWindow.xaml` (a window with "Hello, World!" in its middle), each of which
//! populates the class it names.

// The compiled markup of a document names the types of this crate by the name of the crate.
extern crate self as rcom;

// The i18n dictionaries (`locales/en.yml`, `locales/zh.yml`), compiled in; `t!`
// resolves through the items this generates. English falls back for missing keys.
rust_i18n::i18n!("locales", fallback = "en");

pub mod core;

mod app;
mod main_window;
mod register_types;

// One module per compiled document and `compiled_markup` (the loader table of the compiled
// markup of the crate and `register()`), which the build script of the crate generates.
ferroui_markup_xaml::include_compiled_xaml!();

pub use app::App;
pub use app::i18n::init_locale;
pub use app::settings::init as init_settings;
pub use main_window::MainWindow;
pub use register_types::{register_types, ASSEMBLY};

#[cfg(test)]
mod tests {
    use crate::register_types::ASSEMBLY;

    /// What the crate states about itself for markup.
    #[test]
    fn assembly_states_the_crate() {
        assert_eq!(ASSEMBLY.crate_name, "rcom");
        assert_eq!(ASSEMBLY.name, "Rcom");
    }
}
