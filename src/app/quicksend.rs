//! The quick-send store: ten independent pages of label + payload entries
//! (ticket #17 — llcom parity: "quick-send, unlimited entries, 10 pages"),
//! persisted as TOML under the platform config dir (`dirs`).
//!
//! Pure data + persistence lives here; the panel wiring is the controller's
//! (`super::controller`). The entry payload is a *spec*: text is encoded with the
//! selector's encoding at send time, hex is parsed to bytes — so a saved entry
//! follows encoding switches instead of freezing bytes.

use std::path::PathBuf;
use std::sync::Arc;

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

    /// A port name as a file-safe stem (ticket #35): `/dev/tty.usbserial-1420`
    /// → `_dev_tty.usbserial-1420`. Letters, digits, dot, dash, underscore
    /// survive; everything else collapses to `_`.
    pub fn sanitize_port_name(port: &str) -> String {
        port.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// The per-port override file (`<config>/rcom/quick-send-<stem>.toml`),
    /// when the platform provides a config dir.
    pub fn override_path(port: &str) -> Option<PathBuf> {
        dirs::config_dir().map(|dir| {
            dir.join("rcom")
                .join(format!("quick-send-{}.toml", Self::sanitize_port_name(port)))
        })
    }

    /// The tab's view of the quick-send set for `port` (ticket #35): the
    /// port's own override file when one exists, else the shared default
    /// profile. The first edit through a tab bound to `port` snapshots the
    /// then-current set into the port's file — from then on the port owns its
    /// set (the model: session-scoped overrides over a shared default).
    pub fn load_for(port: &str) -> Self {
        let path = Self::override_path(port);
        if let Some(store) = Self::load_from(path.as_deref()) {
            tracing::info!(port, entries = store.pages.iter().map(|page| page.entries.len()).sum::<usize>(), "quick-send port override loaded");
            return store;
        }
        Self::load()
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

    /// Persists the store for a bound port's tab: the port's own override
    /// file; `None` (an unbound tab) writes the shared default profile
    /// (ticket #35's per-session quick-send sets).
    pub fn save_for(&self, port: Option<&str>) {
        let path = match port {
            Some(port) => Self::override_path(port),
            None => Self::default_path(),
        };
        let _ = self.save_to(path.as_deref());
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

/// The `apiQuickSendList` provider (parity sweep #26): reads the persisted
/// quick-send profile per call — llcom's script API reads the live panel list;
/// rcom's panel state lives per tab on the UI thread, and the file is
/// authoritative after every panel mutation (add/save/delete persist), so the
/// last-saved profile is the honest cross-thread view. A bound port reads its
/// override layer, the standalone panel reads the shared default.
pub struct QuickSendFileLookup {
    /// The port whose override file to read (`None` = the shared default).
    port: Option<String>,
}

impl QuickSendFileLookup {
    /// The shared-default-profile lookup (the standalone script panel).
    pub fn global() -> Arc<Self> {
        Arc::new(Self { port: None })
    }

    /// The port-override-aware lookup (per-session script VMs).
    pub fn for_port(port: impl Into<String>) -> Arc<Self> {
        Arc::new(Self { port: Some(port.into()) })
    }
}

/// The pure read behind [`QuickSendFileLookup`]: entry `id` (llcom semantics:
/// 1-based) over the pages flattened in order; `None` out of range.
pub fn quick_send_entry(store: &QuickSendStore, id: i64) -> Option<(bool, String)> {
    if id < 1 {
        return None;
    }
    let mut index = id - 1;
    for page in &store.pages {
        if index < page.entries.len() as i64 {
            let entry = &page.entries[index as usize];
            return Some((entry.hex, entry.payload.clone()));
        }
        index -= page.entries.len() as i64;
    }
    None
}

impl rcom::core::lua::QuickSendLookup for QuickSendFileLookup {
    fn entry(&self, id: i64) -> Option<(bool, String)> {
        let store = match &self.port {
            Some(port) => QuickSendStore::load_for(port),
            None => QuickSendStore::load(),
        };
        quick_send_entry(&store, id)
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

    /// The per-port override stem sanitizes OS port names (slashes, colons)
    /// into file-safe text, and an override file round-trips (ticket #35's
    /// per-session quick-send sets).
    #[test]
    fn per_port_override_names_sanitize_and_round_trip() {
        let stem = QuickSendStore::sanitize_port_name("/dev/tty.usbserial-1420");
        assert_eq!(stem, "_dev_tty.usbserial-1420");
        assert!(!stem.contains('/'));
        assert_eq!(QuickSendStore::sanitize_port_name("COM3"), "COM3");
        assert_eq!(QuickSendStore::sanitize_port_name(r"\\.\COM10"), "__._COM10");

        let mut store = QuickSendStore::default();
        store.pages[0].entries = vec![QuickSendEntry::text("port-only", "x\r\n")];
        let path = std::env::temp_dir().join(format!("rcom-quicksend-override-{stem}-{}.toml", std::process::id()));
        store.save_to(Some(&path)).expect("save");
        let loaded = QuickSendStore::load_from(Some(&path)).expect("load");
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.pages[0].entries, vec![QuickSendEntry::text("port-only", "x\r\n")]);
        assert_eq!(loaded.pages.len(), PAGE_COUNT);
    }

    /// The `apiQuickSendList` read (parity sweep #26): 1-based ids over the
    /// flattened pages, hex flags carried, out-of-range (and `id < 1`) `None` —
    /// the binding's `""` → head.lua's `nil`.
    #[test]
    fn quick_send_entry_flattens_pages_one_based() {
        let mut store = QuickSendStore::default();
        store.pages[0].entries = vec![QuickSendEntry::text("a", "one"), QuickSendEntry::hex("b", "0D 0A")];
        store.pages[1].entries = vec![QuickSendEntry::text("c", "three")];

        assert_eq!(quick_send_entry(&store, 1), Some((false, "one".into())));
        assert_eq!(quick_send_entry(&store, 2), Some((true, "0D 0A".into())));
        assert_eq!(quick_send_entry(&store, 3), Some((false, "three".into())));
        // Page boundaries flatten: page 2's entry follows page 1's.
        assert_eq!(quick_send_entry(&store, 4), None);
        assert_eq!(quick_send_entry(&store, 0), None);
        assert_eq!(quick_send_entry(&store, -3), None);
    }
}
