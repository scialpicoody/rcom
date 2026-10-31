//! The look (ticket #25, docs/ui-drafts/02-visual-language.md §3/§4): the theme
//! variant applier and the teal accent override.
//!
//! Mechanisms, verified against the pin's sources:
//! - **Variant**: the window is a theme-variant root, so its
//!   `RequestedThemeVariant` (`ThemeVariant::requested_theme_variant_property`,
//!   `ferroui-base` `styling/theme_variant.rs`) becomes the actual variant for
//!   the whole tree — the proven pattern of `examples/log_prototype.rs`
//!   (pre-main-loop dark default). A runtime switch re-resolves every
//!   `ThemeDictionaries`-keyed document live (`Default`/`Light` pairs
//!   throughout the winui theme's resources). `FollowOs` requests the default
//!   variant: the window then follows the platform.
//! - **Accent**: the theme serves the seven `SystemAccentColor*` colour
//!   resources and chains every accent brush onto them with `DynamicResource`
//!   (`Accents/system_accent_colors.rs` + `Common_themeresources_any.xaml`);
//!   resource lookup walks the logical tree, so colours declared on the window
//!   shadow the theme's. Declaring the ramp on the window (like the prototype)
//!   recolours the accent in *both* variants without touching the theme.
//! - **Density**: the compact dictionary is requested in `App.xaml`
//!   (`<WinUITheme DensityStyle="Compact"/>` — `winui_theme.rs` looks the
//!   compact resources up first when the property is set).

use std::rc::Rc;

use ferroui_base::media::Color;
use ferroui_base::styling::ThemeVariant;
use ferroui_base::{BoxedValue, Ref, StyledElement};

use super::settings::ThemeMode;
use crate::MainWindow;

/// The teal accent ramp of the token draft §3 — the seven
/// `SystemAccentColor*` resource keys and their `0xRRGGBB` values (the same
/// numbers the theme would derive from the base colour with its own shade
/// algorithm; the app declares all seven explicitly so they can be hand-tuned
/// later without re-deriving).
pub const ACCENT_RAMP: [(&str, u32); 7] = [
    ("SystemAccentColor", 0x0E7C8C),
    ("SystemAccentColorDark1", 0x094E58),
    ("SystemAccentColorDark2", 0x052D33),
    ("SystemAccentColorDark3", 0x000405),
    ("SystemAccentColorLight1", 0x15BBD3),
    ("SystemAccentColorLight2", 0x3BD5EB),
    ("SystemAccentColorLight3", 0x77E2F1),
];

/// The persisted look at startup: the requested theme variant and the accent
/// ramp, both on the window (a theme-variant root), before the controller
/// attaches and before the main loop runs. **Dark is the default** — the
/// settings store's `ThemeMode` default is dark, and a missing settings file
/// lands there too.
pub fn apply_startup_look(window: &Ref<MainWindow>) {
    let theme = super::settings::global()
        .map(|settings| settings.theme)
        .unwrap_or_default();
    apply_theme_variant(window, theme);
    apply_accent(window);
}

/// Requests the variant for `mode` on the window — the startup path and the
/// Settings page's live theme switch share this (setting the property again
/// re-resolves the theme's dictionaries, no restart).
pub fn apply_theme_variant(window: &Ref<MainWindow>, mode: ThemeMode) {
    let variant = match mode {
        ThemeMode::Dark => ThemeVariant::dark(),
        ThemeMode::Light => ThemeVariant::light(),
        ThemeMode::FollowOs => ThemeVariant::default(),
    };
    let element: &Ref<StyledElement> = window.upcast_ref();
    element.set_value(ThemeVariant::requested_theme_variant_property(), Some(variant));
}

/// Declares the teal `SystemAccentColor*` colours on the window's resources —
/// they shadow the theme's platform accent (lookup walks the logical tree), so
/// every accent brush in the theme resolves teal in both variants.
pub fn apply_accent(window: &Ref<MainWindow>) {
    let element: &Ref<StyledElement> = window.upcast_ref();
    let dict = element.resources();
    for (key, rgb) in ACCENT_RAMP {
        let color = Color::from_rgb(
            ((rgb >> 16) & 0xff) as u8,
            ((rgb >> 8) & 0xff) as u8,
            (rgb & 0xff) as u8,
        );
        let boxed: BoxedValue = Rc::new(color);
        dict.set(key, Some(boxed));
    }
}
