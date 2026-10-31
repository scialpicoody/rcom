//! The app settings (ticket #25): language, theme, motion, the launch defaults
//! (port/baud/encoding), and the log directory override — persisted as TOML
//! under `<config>/rcom/settings.toml` (the quick-send profile's pattern).
//!
//! Pure data + persistence lives here; the Settings page wiring is
//! [`super::tools_settings`], the look's startup application is
//! [`super::theme`]. The store loads *before* the window and controller are
//! built (called from `main.rs`), so the persisted language and theme are in
//! effect from the first frame. A missing or broken file degrades to the
//! defaults — it must never keep the app from coming up (the quicksend rule).
//!
//! Values (docs/ui-drafts/02-visual-language.md, confirmed by #29/#31):
//! - theme: **Dark** default · Light · FollowOS (§4)
//! - accent: teal `#0E7C8C` ramp overriding the seven `SystemAccentColor*`
//!   resources (§3 — the ramp itself lives in [`super::theme`])
//! - motion: `rcom.motion` Full/Reduced (§7) — an authoring contract, see
//!   [`MOTION`] below
//! - language: system (follow-OS) default · en · zh (the i18n env override
//!   `RCOM_LANG` still wins, so demo/smoke runs can force either locale)

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use rcom::core::codec::TextEncoding;
use rcom::core::logstore::default_log_dir;

/// Which theme variant the app requests (token draft §4). `FollowOs` requests
/// the default variant: the window is a theme-variant root, so the platform's
/// variant applies (the prototype's observed behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    FollowOs,
}

/// Which locale the dictionaries use. `System` follows the process locale
/// (the pre-#25 behaviour); a picked language overrides it. The `RCOM_LANG`
/// environment variable still outranks the setting (demo/smoke tooling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageMode {
    #[default]
    System,
    English,
    Chinese,
}

impl LanguageMode {
    /// The dictionary locale the mode names; `None` = follow the system.
    pub fn locale_id(self) -> Option<&'static str> {
        match self {
            Self::System => None,
            Self::English => Some("en"),
            Self::Chinese => Some("zh"),
        }
    }
}

/// The motion contract (token draft §7, `rcom.motion`): `false` = Full (the
/// default — the theme's own 83 ms hover fades stay, they are below the flicker
/// threshold), `true` = Reduced — the app attaches **zero** transitions of its
/// own (opacity/brush or otherwise). Every app-added `Transitions` setter must
/// check [`Settings::reduced_motion`]; log rows never animate in either mode.
///
/// The persisted settings. Every field defaults, so a partial file (older
/// format, hand-trimmed) loads without losing the missing parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    /// The dictionary locale.
    pub language: LanguageMode,
    /// The theme variant requested at startup and switched live.
    pub theme: ThemeMode,
    /// `rcom.motion` (see [`MOTION_DOC`]): `true` = Reduced.
    pub reduced_motion: bool,
    /// The port name preselected on launch (when it is listed).
    pub default_port: Option<String>,
    /// The baud rate preselected on launch.
    pub default_baud: Option<u32>,
    /// The encoding preselected on launch (a [`TextEncoding::id`]).
    pub default_encoding: Option<String>,
    /// The session-log directory override; empty/absent = the default.
    pub log_dir: Option<String>,
    /// The log's monospaced size in px — the §1.8 zoom step (12/13/14/16).
    #[serde(default = "default_log_zoom")]
    pub log_zoom: u32,
}

/// The §1.8 zoom steps the settings page offers (13 = the default).
pub const LOG_ZOOM_STEPS: [u32; 4] = [12, 13, 14, 16];

/// The serde default for [`Settings::log_zoom`] (13, the §1.8 default).
fn default_log_zoom() -> u32 {
    13
}

static SETTINGS: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();

/// Loads the persisted settings into the process-global (call once, from
/// `main.rs`, *before* the first `t!` so the persisted language applies to the
/// first frame). Returns what was loaded.
pub fn init() -> &'static Settings {
    SETTINGS.get_or_init(Settings::load)
}

/// The loaded settings; `None` before [`init`].
pub fn global() -> Option<&'static Settings> {
    SETTINGS.get()
}

impl Settings {
    /// The config-dir file (`<config>/rcom/settings.toml`), when the platform
    /// provides a config dir.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("rcom").join("settings.toml"))
    }

    /// Loads the persisted settings, falling back to the defaults when the file
    /// does not exist (or cannot be read/parsed — a broken store must not keep
    /// the app from coming up).
    pub fn load() -> Self {
        let path = Self::default_path();
        match Self::load_from(path.as_deref()) {
            Some(settings) => {
                let mut settings = settings;
                // A hand-edited or older file must not leave the zoom off the
                // scale: clamp to the nearest §1.8 step.
                if !LOG_ZOOM_STEPS.contains(&settings.log_zoom) {
                    settings.log_zoom = 13;
                }
                tracing::info!(
                    path = %path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    theme = ?settings.theme,
                    language = ?settings.language,
                    reduced_motion = settings.reduced_motion,
                    log_zoom = settings.log_zoom,
                    "settings loaded"
                );
                settings
            }
            None => {
                tracing::info!("no settings file; using the defaults (dark theme, system language)");
                Self::default()
            }
        }
    }

    /// The [`load`](Self::load) logic over an explicit path — testable without
    /// touching the user's real settings.
    pub fn load_from(path: Option<&std::path::Path>) -> Option<Self> {
        let text = std::fs::read_to_string(path?).ok()?;
        match toml::from_str(&text) {
            Ok(settings) => Some(settings),
            Err(error) => {
                tracing::warn!(%error, "settings file unparsable; using defaults");
                None
            }
        }
    }

    /// Persists the settings (best effort: a failed save keeps the session
    /// running; the page reports it on its status line).
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(Self::default_path().as_deref())
    }

    /// The [`save`](Self::save) logic over an explicit path — testable.
    pub fn save_to(&self, path: Option<&std::path::Path>) -> std::io::Result<()> {
        let Some(path) = path else {
            return Err(std::io::Error::other("no config dir"));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        std::fs::write(path, text)
    }

    /// The encoding behind `default_encoding` (an unknown id degrades to the
    /// UTF-8 default — the selector's first entry).
    pub fn resolved_encoding(&self) -> TextEncoding {
        self.default_encoding
            .as_deref()
            .and_then(TextEncoding::from_id)
            .unwrap_or(TextEncoding::Utf8)
    }

    /// The baud rate behind `default_baud` (absent = the 115200 default).
    pub fn resolved_baud(&self) -> u32 {
        self.default_baud.unwrap_or(115_200)
    }

    /// The log directory: the override when set (and non-empty), else the
    /// platform default (the log store's own choice).
    pub fn resolved_log_dir(&self) -> PathBuf {
        match self.log_dir.as_deref().map(str::trim).filter(|dir| !dir.is_empty()) {
            Some(dir) => {
                tracing::info!(dir, "log directory override from the settings");
                PathBuf::from(dir)
            }
            None => default_log_dir(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rcom-settings-test-{}-{tag}.toml", std::process::id()))
    }

    /// A round trip through TOML keeps every option (modes, the launch
    /// defaults, the log dir override).
    #[test]
    fn toml_round_trip_keeps_all_options() {
        let settings = Settings {
            language: LanguageMode::Chinese,
            theme: ThemeMode::Light,
            reduced_motion: true,
            default_port: Some("COM3".into()),
            default_baud: Some(9_600),
            default_encoding: Some("gbk".into()),
            log_dir: Some("/tmp/rcom-logs".into()),
            log_zoom: 14,
        };
        let path = temp_path("roundtrip");
        settings.save_to(Some(&path)).expect("save");
        let loaded = Settings::load_from(Some(&path)).expect("load");
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded, settings);
    }

    /// The defaults: dark theme (the token draft §4 decision), system
    /// language, no launch defaults — and they serialize to snake_case names.
    #[test]
    fn defaults_are_dark_and_system() {
        let settings = Settings::default();
        assert_eq!(settings.theme, ThemeMode::Dark);
        assert_eq!(settings.language, LanguageMode::System);
        assert!(!settings.reduced_motion);
        assert_eq!(settings.resolved_baud(), 115_200);
        assert_eq!(settings.resolved_encoding(), TextEncoding::Utf8);

        let text = toml::to_string(&settings).expect("serialize");
        assert!(text.contains("theme = \"dark\""), "{text}");
        assert!(text.contains("language = \"system\""), "{text}");
    }

    /// A missing file yields `None` (the caller falls back to the defaults),
    /// and a broken file is `None` too — never a panic.
    #[test]
    fn missing_and_broken_files_degrade_to_none() {
        assert_eq!(Settings::load_from(None), None);
        assert_eq!(
            Settings::load_from(Some(std::path::Path::new("/nonexistent/x.toml"))),
            None
        );
        let path = temp_path("broken");
        std::fs::write(&path, "theme = ===not toml===").expect("write");
        assert_eq!(Settings::load_from(Some(&path)), None);
        std::fs::remove_file(&path).ok();
    }

    /// A partial file fills the missing fields with the defaults (unknown
    /// fields are ignored, missing ones default — older/hand-trimmed files
    /// keep loading).
    #[test]
    fn partial_files_fill_defaults() {
        let path = temp_path("partial");
        std::fs::write(&path, "theme = \"light\"\n").expect("write");
        let loaded = Settings::load_from(Some(&path)).expect("load");
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.theme, ThemeMode::Light);
        assert_eq!(loaded.language, LanguageMode::System);
        assert_eq!(loaded.resolved_baud(), 115_200);
    }

    /// Unknown values degrade to the defaults (serde denies nothing for enums
    /// it cannot match — the parse fails, the caller falls back), and the
    /// encoding id resolves through `TextEncoding::from_id`.
    #[test]
    fn encoding_resolution_and_unknown_id() {
        let mut settings = Settings::default();
        settings.default_encoding = Some("big5".into());
        assert_eq!(settings.resolved_encoding(), TextEncoding::Big5);
        settings.default_encoding = Some("klingon".into());
        assert_eq!(settings.resolved_encoding(), TextEncoding::Utf8);
    }

    /// The log dir override: set and trimmed wins, empty/absent falls back to
    /// the platform default.
    #[test]
    fn log_dir_override_resolves() {
        let mut settings = Settings::default();
        let fallback = default_log_dir();
        assert_eq!(settings.resolved_log_dir(), fallback);
        settings.log_dir = Some("  /tmp/rcom-logs  ".into());
        assert_eq!(settings.resolved_log_dir(), PathBuf::from("/tmp/rcom-logs"));
        settings.log_dir = Some("   ".into());
        assert_eq!(settings.resolved_log_dir(), fallback);
    }

    /// The language modes map to their dictionary ids; System follows.
    #[test]
    fn language_modes_map_to_locales() {
        assert_eq!(LanguageMode::System.locale_id(), None);
        assert_eq!(LanguageMode::English.locale_id(), Some("en"));
        assert_eq!(LanguageMode::Chinese.locale_id(), Some("zh"));
    }

    /// The teal accent ramp matches the token draft §3 table exactly (the
    /// theme's brushes chain onto these seven `SystemAccentColor*` colours).
    #[test]
    fn accent_ramp_matches_the_token_draft() {
        assert_eq!(
            crate::app::theme::ACCENT_RAMP,
            [
                ("SystemAccentColor", 0x0E7C8C),
                ("SystemAccentColorDark1", 0x094E58),
                ("SystemAccentColorDark2", 0x052D33),
                ("SystemAccentColorDark3", 0x000405),
                ("SystemAccentColorLight1", 0x15BBD3),
                ("SystemAccentColorLight2", 0x3BD5EB),
                ("SystemAccentColorLight3", 0x77E2F1),
            ]
        );
    }
}
