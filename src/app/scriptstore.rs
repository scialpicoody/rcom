//! The script panel's persistence (ticket #19): named Lua scripts as `.lua` files
//! under the platform config dir (`<config>/rcom/scripts/<name>.lua`, `dirs`), plus
//! the built-in samples bundled from `src/core/lua/scripts.rs` — loadable into the
//! editor but never written over.
//!
//! Pure data + file IO lives here; the editor wiring is the controller's
//! (`super::controller`). A broken store must never keep the panel from coming up:
//! listing a missing/unreadable dir yields an empty list, not an error.

use std::path::PathBuf;

/// The built-in sample scripts of the picker (selecting one loads it into the
/// editor under its name, as if it were a file). `LLCOM_README_SAMPLE` is the
/// standalone uart-processing demo; `SEND_CONVERT_HEX` the one-line TX hook.
pub struct SampleScript {
    pub name: &'static str,
    pub code: &'static str,
}

pub const SAMPLES: &[SampleScript] = &[
    SampleScript {
        name: "sample: llcom README",
        code: rcom::core::lua::scripts::LLCOM_README_SAMPLE,
    },
    SampleScript {
        name: "sample: tx hook (fromHex)",
        code: rcom::core::lua::scripts::SEND_CONVERT_HEX,
    },
];

/// Whether `name` is one of the built-in samples (samples are read-only).
pub fn sample_by_name(name: &str) -> Option<&'static SampleScript> {
    SAMPLES.iter().find(|sample| sample.name == name)
}

/// The scripts directory (`<config>/rcom/scripts`), when the platform provides a
/// config dir.
pub fn scripts_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("rcom").join("scripts"))
}

/// Strips a script name to a safe file stem: path separators and control
/// characters out, trimmed, non-empty. The UI saves whatever the user typed under
/// this name (the picker shows the sanitized form on reload).
pub fn sanitize_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':' | '\0'))
        .collect();
    let trimmed = cleaned.trim();
    let trimmed = trimmed.trim_start_matches('.');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn script_path(dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    sanitize_name(name).map(|name| dir.join(format!("{name}.lua")))
}

/// Lists the saved script names (file stems), sorted.
pub fn list(dir: Option<&std::path::Path>) -> Vec<String> {
    let Some(dir) = dir else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lua"))
        .filter_map(|entry| entry.path().file_stem()?.to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// Loads one script's code; `None` when it does not exist or cannot be read.
pub fn load(dir: Option<&std::path::Path>, name: &str) -> Option<String> {
    std::fs::read_to_string(script_path(dir?, name)?).ok()
}

/// Saves `code` under `name` (creating the directory on the way). Returns the
/// sanitized name the script is reachable under.
pub fn save(dir: Option<&std::path::Path>, name: &str, code: &str) -> std::io::Result<String> {
    let Some(dir) = dir else {
        return Err(std::io::Error::other("no config dir"));
    };
    let Some(name) = sanitize_name(name) else {
        return Err(std::io::Error::other("empty script name"));
    };
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.lua"));
    std::fs::write(&path, code)?;
    Ok(name)
}

/// Deletes one saved script; missing files are fine.
pub fn delete(dir: Option<&std::path::Path>, name: &str) -> std::io::Result<()> {
    let Some(path) = script_path(
        dir.ok_or_else(|| std::io::Error::other("no config dir"))?,
        name,
    ) else {
        return Err(std::io::Error::other("empty script name"));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rcom-scripts-{}-{tag}", std::process::id()))
    }

    /// Save → list → load round trip; names sort; the `.lua` suffix stays out of
    /// the picker.
    #[test]
    fn save_list_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let saved = save(Some(&dir), "hello", "print('hi')\n").expect("save");
        assert_eq!(saved, "hello");
        save(Some(&dir), "  beep ", "return 1").expect("save");

        let names = list(Some(&dir));
        assert_eq!(names, vec!["beep", "hello"]);
        assert_eq!(load(Some(&dir), "hello").as_deref(), Some("print('hi')\n"));

        delete(Some(&dir), "hello").expect("delete");
        assert_eq!(list(Some(&dir)), vec!["beep"]);
        assert_eq!(load(Some(&dir), "hello"), None);
        // Deleting twice is fine.
        delete(Some(&dir), "hello").expect("re-delete");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Names sanitize to safe file stems: separators and dots out, empty is an
    /// error — `../x` can never escape the scripts dir.
    #[test]
    fn names_sanitize_to_safe_stems() {
        assert_eq!(sanitize_name("../evil"), Some("evil".into()));
        assert_eq!(sanitize_name("a/b\\c:d"), Some("abcd".into()));
        assert_eq!(sanitize_name("  spaced  "), Some("spaced".into()));
        assert_eq!(sanitize_name("..."), None);
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("中文脚本"), Some("中文脚本".into()));
        // And the sanitized name cannot traverse: save/load/delete all funnel
        // through `script_path`.
        let dir = temp_dir("sanitize");
        let saved = save(Some(&dir), "../outside", "x").expect("save");
        assert_eq!(saved, "outside");
        assert!(dir.join("outside.lua").exists());
        assert!(!dir.parent().unwrap().join("outside.lua").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Missing dir / no config dir degrade to empty lists and `None` loads.
    #[test]
    fn missing_store_degrades_quietly() {
        assert!(list(None).is_empty());
        assert!(list(Some(std::path::Path::new("/nonexistent/rcom-scripts"))).is_empty());
        assert_eq!(load(None, "x"), None);
        assert!(save(None, "x", "y").is_err());
        assert!(load(Some(std::path::Path::new("/nonexistent/rcom-scripts")), "x").is_none());
    }

    /// The built-in samples are non-empty, distinct and discoverable by name.
    #[test]
    fn samples_are_loadable() {
        assert!(SAMPLES.len() >= 2);
        for sample in SAMPLES {
            assert!(!sample.code.trim().is_empty(), "{} empty", sample.name);
            assert!(sample_by_name(sample.name).is_some());
        }
        assert!(sample_by_name("nope").is_none());
    }
}
