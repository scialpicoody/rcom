//! The settings page (issue #25): the app-global page of the bottom pane —
//! launch defaults (port/baud/encoding), the log directory override, language,
//! theme and motion.
//!
//! The XAML carries the skeleton (named controls, no visible strings); this
//! module resolves them, sets every label from the i18n dictionaries and wires
//! the events. Every change persists to `<config>/rcom/settings.toml`
//! immediately (the quick-send store's save-on-change rule) and applies live
//! where cheap:
//!
//! - **theme** — the window's requested variant re-resolves the theme's
//!   dictionaries live (`super::theme::apply_theme_variant`);
//! - **language** — the process locale switches and every page re-applies its
//!   strings (`super::controller::apply_all_strings`); history log rows keep
//!   the language they were rendered in (they are data, not chrome);
//! - **launch defaults / log dir** — persisted, applied at the next launch
//!   (the settings load runs before the window and the log store exist).
//!
//! The port selector is fed by the same `UiMsg::Ports` listing the toolbar
//! consumes (`apply_ports`), so the launch-default picker shows real names.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{CheckBox, ComboBox, TabItem, TextBox, TextBlock};

use rust_i18n::t;

use rcom::core::codec::TextEncoding;
use rcom::core::port::PortInfo;

use super::controller;
use super::settings::{LanguageMode, Settings, ThemeMode};
use super::theme;
use crate::MainWindow;

/// The baud rates the default-baud selector offers (the toolbar's list —
/// [`super::controller::BAUD_RATES`], single source).
use super::controller::BAUD_RATES;

/// The selector index of each language mode (System · English · 中文).
const LANGUAGE_INDEXES: [LanguageMode; 3] = [LanguageMode::System, LanguageMode::English, LanguageMode::Chinese];
/// The selector index of each theme mode (Dark · Light · Follow OS).
const THEME_INDEXES: [ThemeMode; 3] = [ThemeMode::Dark, ThemeMode::Light, ThemeMode::FollowOs];

fn language_at(index: i32) -> LanguageMode {
    LANGUAGE_INDEXES.get(index.max(0) as usize).copied().unwrap_or_default()
}

fn theme_at(index: i32) -> ThemeMode {
    THEME_INDEXES.get(index.max(0) as usize).copied().unwrap_or_default()
}

/// One page instance: the resolved controls, the window (the live theme
/// switch's target) and its save state.
struct SettingsPage {
    tab: Ref<TabItem>,
    port_caption: Ref<TextBlock>,
    port: Ref<ComboBox>,
    baud_caption: Ref<TextBlock>,
    baud: Ref<ComboBox>,
    encoding_caption: Ref<TextBlock>,
    encoding: Ref<ComboBox>,
    log_dir_caption: Ref<TextBlock>,
    log_dir: Ref<TextBox>,
    log_dir_note: Ref<TextBlock>,
    language_caption: Ref<TextBlock>,
    language: Ref<ComboBox>,
    theme_caption: Ref<TextBlock>,
    theme: Ref<ComboBox>,
    reduced_motion: Ref<CheckBox>,
    status: Ref<TextBlock>,
    path_note: Ref<TextBlock>,
    window: Ref<MainWindow>,
    /// The ports behind the default-port selector (the listing `apply_ports`
    /// received).
    ports: RefCell<Vec<PortInfo>>,
    /// The last persisted settings: the priming source for the selector
    /// rebuilds (the process-global is the *startup* snapshot — a live theme
    /// or language switch must not prime from stale values).
    saved: RefCell<Settings>,
    /// `true` while code primes the selectors — the change handlers skip so
    /// priming does not persist back or re-trigger the live applies.
    loading: Cell<bool>,
}

impl SettingsPage {
    /// The settings the selectors prime from (see `saved`).
    fn current(&self) -> Settings {
        self.saved.borrow().clone()
    }

    /// Applies the locale strings and primes the selectors from the persisted
    /// settings. Runs at attach and again after a language switch (then the
    /// persisted settings *are* the current controls — the switch saved first).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("settings_tab").to_string())));
        self.port_caption.set_text(Some(&t!("settings_port")));
        self.baud_caption.set_text(Some(&t!("settings_baud")));
        self.encoding_caption.set_text(Some(&t!("settings_encoding")));
        self.log_dir_caption.set_text(Some(&t!("settings_log_dir")));
        self.log_dir_note.set_text(Some(&t!("settings_log_dir_note")));
        self.language_caption.set_text(Some(&t!("settings_language")));
        self.theme_caption.set_text(Some(&t!("settings_theme")));
        self.reduced_motion
            .set_content(Some(Rc::new(t!("settings_reduced_motion").to_string())));
        self.status.set_text(Some(&t!("settings_status_idle")));
        self.path_note.set_text(Some(&t!(
            "settings_path_note",
            path = Settings::default_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<no config dir>".into())
        )));

        // Prime the selectors from the persisted settings (no save round-trip:
        // `loading` guards the change handlers).
        let saved = self.current();
        self.loading.set(true);
        self.fill_language_selector(&saved);
        self.fill_theme_selector(&saved);
        self.fill_baud_selector(&saved);
        self.fill_encoding_selector(&saved);
        self.log_dir.set_text(saved.log_dir.as_deref());
        self.reduced_motion.set_is_checked(Some(saved.reduced_motion));
        self.loading.set(false);
    }

    fn fill_language_selector(&self, saved: &Settings) {
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (the removal path resolves the
        // stale index against the already-empty list).
        self.language.set_selected_index(-1);
        let items = self.language.items();
        items.clear();
        for label in [
            t!("settings_language_system").to_string(),
            t!("settings_language_en").to_string(),
            t!("settings_language_zh").to_string(),
        ] {
            let value: BoxedValue = Rc::new(label);
            items.add(Some(value));
        }
        let index = LANGUAGE_INDEXES
            .iter()
            .position(|mode| *mode == saved.language)
            .unwrap_or(0);
        self.language.set_selected_index(index as i32);
    }

    fn fill_theme_selector(&self, saved: &Settings) {
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (the removal path resolves the
        // stale index against the already-empty list).
        self.theme.set_selected_index(-1);
        let items = self.theme.items();
        items.clear();
        for label in [
            t!("settings_theme_dark").to_string(),
            t!("settings_theme_light").to_string(),
            t!("settings_theme_follow_os").to_string(),
        ] {
            let value: BoxedValue = Rc::new(label);
            items.add(Some(value));
        }
        let index = THEME_INDEXES
            .iter()
            .position(|mode| *mode == saved.theme)
            .unwrap_or(0);
        self.theme.set_selected_index(index as i32);
    }

    fn fill_baud_selector(&self, saved: &Settings) {
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (the removal path resolves the
        // stale index against the already-empty list).
        self.baud.set_selected_index(-1);
        let items = self.baud.items();
        items.clear();
        for rate in BAUD_RATES {
            let value: BoxedValue = Rc::new(rate.to_string());
            items.add(Some(value));
        }
        let selected = saved.resolved_baud();
        let index = BAUD_RATES
            .iter()
            .position(|rate| *rate == selected)
            .unwrap_or_else(|| BAUD_RATES.iter().position(|rate| *rate == 115_200).unwrap_or(0));
        self.baud.set_selected_index(index as i32);
    }

    fn fill_encoding_selector(&self, saved: &Settings) {
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (the removal path resolves the
        // stale index against the already-empty list).
        self.encoding.set_selected_index(-1);
        let items = self.encoding.items();
        items.clear();
        for encoding in TextEncoding::all() {
            // Encoding names are proper nouns — not localized (the same rule
            // the toolbar's encoding selector follows).
            let value: BoxedValue = Rc::new(encoding.display_name().to_string());
            items.add(Some(value));
        }
        let selected = saved.resolved_encoding();
        let index = TextEncoding::all()
            .iter()
            .position(|encoding| *encoding == selected)
            .unwrap_or(0);
        self.encoding.set_selected_index(index as i32);
    }

    /// The port listing arrived (the same `UiMsg::Ports` the toolbar shows):
    /// refill the default-port selector, keeping the persisted preselect.
    fn fill_ports(&self, ports: &[PortInfo]) {
        *self.ports.borrow_mut() = ports.to_vec();
        // Deselect first: clearing a selector's items while it has a selection
        // panics in the pin's selection model (the removal path resolves the
        // stale index against the already-empty list). Under the loading guard
        // — the deselect must not persist an empty default port.
        self.loading.set(true);
        self.port.set_selected_index(-1);
        let items = self.port.items();
        items.clear();
        for info in ports {
            let name = match (&info.description, info.is_mock) {
                (Some(description), false) => format!("{}  ({description})", info.name),
                _ => info.name.clone(),
            };
            let value: BoxedValue = Rc::new(name);
            items.add(Some(value));
        }
        let index = self
            .current()
            .default_port
            .and_then(|name| ports.iter().position(|info| info.name == name).map(|index| index as i32));
        self.loading.set(true);
        self.port.set_selected_index(index.unwrap_or(-1));
        self.loading.set(false);
    }

    /// The settings the current controls describe.
    fn collect(&self) -> Settings {
        let port_index = self.port.selected_index();
        let baud_index = self.baud.selected_index();
        let encoding_index = self.encoding.selected_index();
        Settings {
            language: language_at(self.language.selected_index()),
            theme: theme_at(self.theme.selected_index()),
            reduced_motion: self.reduced_motion.is_checked().unwrap_or(false),
            default_port: self
                .ports
                .borrow()
                .get(port_index.max(0) as usize)
                .filter(|_| port_index >= 0)
                .map(|info| info.name.clone()),
            default_baud: BAUD_RATES.get(baud_index.max(0) as usize).copied(),
            default_encoding: TextEncoding::all()
                .get(encoding_index.max(0) as usize)
                .map(|encoding| encoding.id().to_string()),
            log_dir: self
                .log_dir
                .text()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty()),
        }
    }

    /// Persists the current controls and reports on the status line. Every
    /// change handler funnels here — the file is the single durable form, and
    /// the page's `saved` copy is what the selector rebuilds prime from.
    fn persist(&self) {
        let settings = self.collect();
        match settings.save() {
            Ok(()) => {
                *self.saved.borrow_mut() = settings;
                self.set_status(t!("settings_saved").to_string());
            }
            Err(error) => self.set_status(t!("settings_save_failed", error = &error).to_string()),
        }
    }

    fn set_status(&self, text: String) {
        self.status.set_text(Some(&text));
    }

    /// The language selector moved: persist, switch the process locale and
    /// re-apply every visible string (the live switch — no restart).
    fn apply_language(&self) {
        if self.loading.get() {
            return;
        }
        self.persist();
        let mode = language_at(self.language.selected_index());
        let locale = mode
            .locale_id()
            .map(str::to_string)
            .unwrap_or_else(super::i18n::system_locale);
        rust_i18n::set_locale(&locale);
        controller::apply_all_strings();
    }

    /// The theme selector moved: persist and switch the window's variant live.
    fn apply_theme(&self) {
        if self.loading.get() {
            return;
        }
        self.persist();
        theme::apply_theme_variant(&self.window, theme_at(self.theme.selected_index()));
    }

    /// The motion toggle moved: persist (the Reduced contract is enforced by
    /// every app-added `Transitions` setter — see `settings::MOTION_DOC`).
    fn apply_motion(&self) {
        if self.loading.get() {
            return;
        }
        self.persist();
    }

    /// A launch default moved (port/baud/encoding selectors, log dir box):
    /// persist only — these apply at the next launch.
    fn apply_launch_default(&self) {
        if self.loading.get() {
            return;
        }
        self.persist();
    }
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle to the page (the controller's insertion points and
    /// the event handlers run there; the pattern of the controller's PUMP).
    static PAGE: RefCell<Option<Rc<SettingsPage>>> = RefCell::new(None);
}

/// Resolves the named controls of the settings tab, sets the strings and wires
/// the events. One insertion point in the controller's `attach`; the window
/// handle is the live theme switch's target.
pub fn attach(element: &Ref<StyledElement>, window: &Ref<MainWindow>) {
    let page = Rc::new(SettingsPage {
        tab: NameScopeExtensions::get(element, "SettingsTab"),
        port_caption: NameScopeExtensions::get(element, "SettingsPortCaption"),
        port: NameScopeExtensions::get(element, "SettingsPortSelector"),
        baud_caption: NameScopeExtensions::get(element, "SettingsBaudCaption"),
        baud: NameScopeExtensions::get(element, "SettingsBaudSelector"),
        encoding_caption: NameScopeExtensions::get(element, "SettingsEncodingCaption"),
        encoding: NameScopeExtensions::get(element, "SettingsEncodingSelector"),
        log_dir_caption: NameScopeExtensions::get(element, "SettingsLogDirCaption"),
        log_dir: NameScopeExtensions::get(element, "SettingsLogDirBox"),
        log_dir_note: NameScopeExtensions::get(element, "SettingsLogDirNote"),
        language_caption: NameScopeExtensions::get(element, "SettingsLanguageCaption"),
        language: NameScopeExtensions::get(element, "SettingsLanguageSelector"),
        theme_caption: NameScopeExtensions::get(element, "SettingsThemeCaption"),
        theme: NameScopeExtensions::get(element, "SettingsThemeSelector"),
        reduced_motion: NameScopeExtensions::get(element, "SettingsReducedMotionCheck"),
        status: NameScopeExtensions::get(element, "SettingsStatus"),
        path_note: NameScopeExtensions::get(element, "SettingsPathNote"),
        window: window.clone(),
        ports: RefCell::new(Vec::new()),
        saved: RefCell::new(super::settings::global().cloned().unwrap_or_default()),
        loading: Cell::new(false),
    });
    page.apply_ui_strings();

    // Launch defaults: persist only (applied at the next launch).
    page.port.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_launch_default()
    });
    page.baud.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_launch_default()
    });
    page.encoding.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_launch_default()
    });
    page.log_dir.text_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_launch_default()
    });

    // The live applies.
    page.language.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_language()
    });
    page.theme.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_theme()
    });
    page.reduced_motion.is_checked_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_motion()
    });

    PAGE.with(|cell| *cell.borrow_mut() = Some(page));
}

/// The pump's entry for the port listing (`UiMsg::Ports`): the settings page's
/// default-port picker shows the same names as the toolbar.
pub fn apply_ports(ports: &[PortInfo]) {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.fill_ports(ports);
        }
    });
}

/// Re-applies the locale strings after a language switch (the settings page
/// itself is the trigger — `controller::apply_all_strings` fans out here).
pub fn apply_strings() {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.apply_ui_strings();
        }
    });
}

/// The `RCOM_SETTINGSDEMO` flip (run 1 of the two-run persist proof): drives
/// the *real* selector paths — setting the selections fires the same handlers
/// a click would, so the live theme switch, the live language re-apply and the
/// persistence all run through production code. Run 2 (see the controller's
/// `autodemo_settings_check`) finds the flipped values persisted and says OK.
pub fn autodemo_flip() {
    PAGE.with(|cell| {
        if let Some(page) = cell.borrow().as_ref() {
            page.loading.set(false);
            // Theme → Light (index 1): the selection-changed handler applies
            // the variant live and persists.
            page.theme.set_selected_index(1);
            // Language → 中文 (index 2): the handler switches the locale and
            // re-applies every page's strings.
            page.language.set_selected_index(2);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selector index maps round-trip through the mode tables — the page's
    /// collect/prime paths agree.
    #[test]
    fn selector_indexes_round_trip() {
        for (index, mode) in LANGUAGE_INDEXES.iter().enumerate() {
            assert_eq!(language_at(index as i32), *mode);
        }
        assert_eq!(language_at(-1), LanguageMode::System);
        assert_eq!(language_at(99), LanguageMode::System);

        for (index, mode) in THEME_INDEXES.iter().enumerate() {
            assert_eq!(theme_at(index as i32), *mode);
        }
        assert_eq!(theme_at(-1), ThemeMode::Dark);
        assert_eq!(theme_at(99), ThemeMode::Dark);
    }
}
