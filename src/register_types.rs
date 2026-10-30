//! The type table of this crate: its namespace, its classes, what it states about itself for
//! markup, and the loader table of its compiled markup.

use crate::{App, MainWindow};
use ferroui_base::metadata::MarkupAssembly;
use ferroui_base::TypeInfo;

/// The dotted namespace of the modules of this crate.
const NAMESPACES: &[(&str, &str)] = &[("rcom", "Rcom")];

/// What this crate states about itself for markup; the name is the authority of the URIs of
/// its documents and assets.
pub static ASSEMBLY: MarkupAssembly = MarkupAssembly {
    name: "Rcom",
    crate_name: "rcom",
    xmlns_definitions: &[],
    xmlns_prefixes: &[],
    metadata: &[],
};

/// The classes of the namespace (`X::TYPE`).
const TYPES: &[&TypeInfo] = &[App::TYPE, MainWindow::TYPE];

/// Registers the namespace, the types, the assembly and the compiled markup of this crate
/// (and of the crates it is built on). Cheap and idempotent.
pub fn register_types() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // The runtime library of compiled markup (the markup extensions the compiled
        // documents create), which registers the controls.
        ferroui_markup_xaml::register_types();
        ferroui_themes_winui::register_types();
        TypeInfo::register_namespaces(NAMESPACES);
        TypeInfo::register_all(TYPES);
        MarkupAssembly::register(&ASSEMBLY);
        // The loader table of the compiled markup, which the build of the crate generates.
        crate::compiled_markup::register();
    });
}
