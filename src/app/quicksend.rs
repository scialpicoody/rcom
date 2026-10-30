//! The quick-send store: ten independent pages of label + payload entries
//! (ticket #17 — llcom parity: "quick-send, unlimited entries, 10 pages"),
//! persisted as TOML under the platform config dir (`dirs`).
//!
//! Pure data + persistence lives here; the panel wiring is the controller's
//! (`super::controller`). The entry payload is a *spec*: text is encoded with the
//! selector's encoding at send time, hex is parsed to bytes — so a saved entry
//! follows encoding switches instead of freezing bytes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How many pages the quick-send bar has (llcom parity).
pub const PAGE_COUNT: usize = 10;

/// One quick-send entry: a display label plus the payload to send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSendEntry {
    pub label: String,
    /// The payload: text to encode at send time, or hex text when [`Self::hex`].
    pub payload: String,
    /// `true` — `payload` is hex (`"0F 0A"`), sent as raw bytes.
    #[serde(default)]
    pub hex: bool,
}

impl QuickSendEntry {
    pub fn text(label: impl Into<String>, payload: impl Into<String>) -> Self {
        Self { label: label.into(), payload: payload.into(), hex: false }
    }

    pub fn hex(label: impl Into<String>, payload: impl Into<String>) -> Self {
        Self { label: label.into(), payload: payload.into(), hex: true }
    }
}

/// One page of entries. Unlimited length; the panel scrolls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSendPage {
    #[serde(default)]
    pub entries: Vec<QuickSendEntry>,
}

/// The persisted quick-send profile: exactly [`PAGE_COUNT`] pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSendStore {
    pub pages: Vec<QuickSendPage>,
}

impl Default for QuickSendStore {
    fn default() -> Self {
        Self { pages: vec![QuickSendPage::default(); PAGE_COUNT] }
    }
}

impl QuickSendStore {
    /// The store with the factory seeds on page 1 (fresh installs demo something;
    /// once a profile file exists it wins verbatim).
    pub fn with_seed_entries() -> Self {
        let mut store = Self::default();
        store.pages[0].entries = vec![
            QuickSendEntry::text("ping", "ping\r\n"),
            QuickSendEntry::text("ver (AT)", "AT+VER?\r\n"),
            QuickSendEntry::text("你好 (GBK test)", "你好，串口"),
            QuickSendEntry::hex("reset (hex)", "A0 01 FF"),
        ];
        store
    }

    /// The config-dir file (`<config>/rcom/quick-send.toml`), when the platform
    /// provides a config dir.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("rcom").join("quick-send.toml"))
    }

    /// Loads the persisted store, falling back to the seeded default when the file
    /// does not exist (or cannot be read/parsed — a broken profile must not keep
    /// the panel from coming up).
    pub fn load() -> Self {
        let path = Self::default_path();
        match Self::load_from(path.as_deref()) {
            Some(store) => {
                let entries: usize = store.pages.iter().map(|page| page.entries.len()).sum();
                tracing::info!(
                    path = %path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    entries,
                    "quick-send profile loaded"
                );
                store
            }
            None => {
                tracing::info!("no quick-send profile; using the seeded defaults");
                Self::with_seed_entries()
            }
        }
    }

    /// The [`load`](Self::load) logic over an explicit path — testable without
    /// touching the user's real profile.
    pub fn load_from(path: Option<&std::path::Path>) -> Option<Self> {
        let text = std::fs::read_to_string(path?).ok()?;
        let parsed: Result<QuickSendStore, _> = toml::from_str(&text);
        match parsed {
            Ok(store) => Some(store.padded_to_page_count()),
            Err(error) => {
                tracing::warn!(%error, "quick-send profile unparsable; using defaults");
                None
            }
        }
    }

    /// Persists the store (best effort: a failed save keeps the session running;
    /// the in-memory panel stays authoritative).
    pub fn save(&self) {
        let _ = self.save_to(Self::default_path().as_deref());
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

    fn padded_to_page_count(mut self) -> Self {
        if self.pages.len() < PAGE_COUNT {
            self.pages.resize(PAGE_COUNT, QuickSendPage::default());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default store has ten independent pages; the seeds sit on page 1 only.
    #[test]
    fn default_store_has_ten_pages_and_seeds_page_one() {
        let store = QuickSendStore::with_seed_entries();
        assert_eq!(store.pages.len(), PAGE_COUNT);
        assert!(!store.pages[0].entries.is_empty());
        assert!(store.pages[1].entries.is_empty());
        assert!(store.pages.iter().all(|page| page.entries.len() < 100));
    }

    /// A round trip through TOML keeps entries, payload escapes and the ten-page
    /// shape (hex flags, \r\n payloads).
    #[test]
    fn toml_round_trip_keeps_entries_and_page_count() {
        let mut store = QuickSendStore::default();
        store.pages[0].entries = vec![
            QuickSendEntry::text("ping", "ping\r\n"),
            QuickSendEntry::hex("reset", "A0 01 FF"),
        ];
        store.pages[6].entries = vec![QuickSendEntry::text("你好", "你好，串口")];

        let path = std::env::temp_dir().join(format!("rcom-quicksend-test-{}.toml", std::process::id()));
        store.save_to(Some(&path)).expect("save");
        let loaded = QuickSendStore::load_from(Some(&path)).expect("load");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded, store);
        assert_eq!(loaded.pages.len(), PAGE_COUNT);
        assert_eq!(loaded.pages[0].entries[0].payload, "ping\r\n");
        assert!(loaded.pages[0].entries[1].hex);
        assert_eq!(loaded.pages[6].entries[0].label, "你好");
        // The later pages restored to empty, not lost.
        assert!(loaded.pages[9].entries.is_empty());
    }

    /// A missing file yields `None` (the caller falls back to defaults), and a
    /// truncated store is padded back to ten pages.
    #[test]
    fn load_missing_file_is_none_and_short_files_are_padded() {
        assert_eq!(QuickSendStore::load_from(None), None);
        assert_eq!(QuickSendStore::load_from(Some(std::path::Path::new("/nonexistent/x.toml"))), None);

        let mut store = QuickSendStore::default();
        store.pages[0].entries = vec![QuickSendEntry::text("solo", "x")];
        let path = std::env::temp_dir().join(format!("rcom-quicksend-pad-{}.toml", std::process::id()));
        store.save_to(Some(&path)).expect("save");
        let loaded = QuickSendStore::load_from(Some(&path)).expect("load");
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.pages.len(), PAGE_COUNT);
        assert_eq!(loaded.pages[0].entries, vec![QuickSendEntry::text("solo", "x")]);
    }
}
