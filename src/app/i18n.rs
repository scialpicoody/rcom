//! Locale selection: the i18n dictionaries live in `locales/` (loaded by
//! `rust_i18n::i18n!` in the crate root); this module picks the active locale at
//! startup. `RCOM_LANG` wins (`zh`/`en`), else the process locale is derived from
//! `LANG`/`LC_ALL` (any `zh*` prefix), else English.

/// The locales rcom ships dictionaries for.
pub const SUPPORTED: [&str; 2] = ["en", "zh"];

/// Picks and sets the process locale (call once, before the first `t!`).
pub fn init_locale() {
    rust_i18n::set_locale(&pick_locale());
}

/// The locale to use: `RCOM_LANG` if it names a shipped dictionary, else the
/// system locale's language part when shipped, else `"en"`.
pub fn pick_locale() -> String {
    pick_from(
        std::env::var("RCOM_LANG").ok(),
        std::env::var("LC_ALL").or_else(|_| std::env::var("LANG")).ok(),
    )
}

/// The pure decision behind [`pick_locale`]: `rcom_lang` overrides the system
/// locale value; only shipped dictionaries are accepted.
fn pick_from(rcom_lang: Option<String>, system: Option<String>) -> String {
    if let Some(lang) = rcom_lang {
        let lang = lang.trim().to_lowercase();
        if SUPPORTED.contains(&lang.as_str()) {
            return lang;
        }
    }
    let language = system
        .unwrap_or_default()
        .to_lowercase()
        .split(['.', '@', '_'])
        .next()
        .unwrap_or_default()
        .to_string();
    if SUPPORTED.contains(&language.as_str()) {
        language
    } else {
        "en".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only shipped locales are accepted from the environment.
    #[test]
    fn rcom_lang_wins_when_supported() {
        assert_eq!(pick_from(Some("zh".into()), Some("en_US.UTF-8".into())), "zh");
        assert_eq!(pick_from(Some("en".into()), Some("zh_CN.UTF-8".into())), "en");
        // Unknown values fall through to the system locale.
        assert_eq!(pick_from(Some("klingon".into()), Some("zh_CN".into())), "zh");
        assert_eq!(pick_from(Some("klingon".into()), None), "en");
    }

    /// `zh_CN.UTF-8` style values map to the `zh` dictionary.
    #[test]
    fn system_lang_maps_to_its_language() {
        assert_eq!(pick_from(None, Some("zh_CN.UTF-8".into())), "zh");
        assert_eq!(pick_from(None, Some("en_US.UTF-8".into())), "en");
        assert_eq!(pick_from(None, Some("C".into())), "en");
        assert_eq!(pick_from(None, None), "en");
    }
}
