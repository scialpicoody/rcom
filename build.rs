//! Compiles the two markup documents of the application to Rust source (`ferroui-build`;
//! docs/porting/xaml.md, 9.6): `App.xaml` and `MainWindow.xaml`, each as a group of its own,
//! against the type models — the scan of the sources of this crate and the models of the
//! crates it is built on, which Cargo hands to this script as `DEP_<CRATE>_XAMLMETA`.
//! `ferroui_markup_xaml::include_compiled_xaml!()` includes what the run writes.

use ferroui_build::{Build, TypeSystem, XamlGroup};

fn main() {
    Build::from_env()
        // Compile against the build-time type models: the script links no framework crate.
        .type_system(TypeSystem::Model)
        // Compile bindings, as upstream applications do by default.
        .default_compile_bindings(true)
        .compile_group(
            XamlGroup::new("compiled_app")
                .documents(&[("App.xaml", include_str!("App.xaml"))])
                .class_document("App.xaml"),
        )
        .compile_group(
            XamlGroup::new("compiled_main_window")
                .documents(&[("MainWindow.xaml", include_str!("MainWindow.xaml"))])
                .class_document("MainWindow.xaml"),
        )
        .run();
}
