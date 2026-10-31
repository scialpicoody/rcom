//! The per-session view-model (design-ui §3.6, §5.6): one tab's presentation
//! state and log model, plain Rust over `Rc`/`RefCell` — the viewmodel state
//! the contract's boundary rule names (filter, view mode, follow, hide-sent,
//! timestamps, encoding… never core state). One tab per session; a tab's
//! lineage (`session_ids`) survives rebinds and auto-reconnects so the
//! scrollback continues across the reconnect (§6.5).

use super::logview::{LogLine, ViewMode};
use super::quicksend::QuickSendStore;
use rcom::core::bus::PortState;
use rcom::core::codec::TextEncoding;
use rcom::core::port::{PortInfo, PortKind};

/// How many rows the log view keeps before it drops the oldest (llcom-style
/// cap; the model holds the raw bytes the rows re-render from).
pub const ROW_CAP: usize = 5000;

/// One session tab's state. Lives app-side (`Rc<RefCell<…>>` on the UI
/// thread); the only core knowledge it holds is the session-id lineage, the
/// bound port's listing and the chip state — all read off bus events.
pub struct SessionVm {
    /// The tab's identity (stable across rebinds; the tab strip indexes by
    /// position, this id survives removals).
    pub tab_id: usize,
    /// The tab header's port name (the picker caption before a port binds).
    pub title: String,
    /// The bound port's listing (`kind` drives the mock drawer's visibility —
    /// AC9: the UI's mock affordances key off `PortInfo::kind`).
    pub port: Option<PortInfo>,
    /// Every session id this tab has carried (rebinds and auto-reconnects
    /// append; data routing matches any of them — one scrollback).
    pub session_ids: Vec<u64>,
    /// The chip state (contract §5.4: Closed grey · Open green · WatchDesired
    /// amber).
    pub state: PortState,
    /// -- presentation state ------------------------------------------------
    pub encoding: TextEncoding,
    pub view_mode: ViewMode,
    pub filter: String,
    pub hide_sent: bool,
    pub timestamps: bool,
    pub follow: bool,
    /// The log model (capped at [`ROW_CAP`]; raw bytes authoritative).
    pub lines: Vec<LogLine>,
    /// The session's pre-send hook is armed (drives the quick-send badge and
    /// new TX rows' marker expectations; the core is the rewriter).
    pub hook_armed: bool,
    /// A hook set before the tab had a session: flushed to the core on bind.
    pub pending_hook: Option<Option<String>>,
    /// The quick-send panel: this tab's view of the persisted profile (pages,
    /// page index, the editor's target).
    pub quick: QuickSendStore,
    pub quick_page: usize,
    pub quick_editing: Option<usize>,
    /// Busy rule (§4.1): an open is in flight — the Open/Close button greys
    /// until the open's `Result` (the Open/Error port event) lands.
    pub open_pending: bool,
    /// The mock drawer's expanded state (per tab).
    pub mock_drawer_open: bool,
    /// The mock playback pause mirror (the drawer button's label).
    pub mock_paused: bool,
    /// The mock playback speed mirror (the drawer combo's index).
    pub mock_speed_index: usize,
}

impl SessionVm {
    /// A fresh, unbound tab (the `+` flow before a port is picked).
    pub fn new(tab_id: usize) -> Self {
        Self {
            tab_id,
            title: String::new(),
            port: None,
            session_ids: Vec::new(),
            state: PortState::Closed,
            encoding: TextEncoding::Utf8,
            view_mode: ViewMode::Both,
            filter: String::new(),
            hide_sent: false,
            timestamps: true,
            follow: true,
            lines: Vec::new(),
            hook_armed: false,
            pending_hook: None,
            quick: QuickSendStore::load(),
            quick_page: 0,
            quick_editing: None,
            open_pending: false,
            mock_drawer_open: false,
            mock_paused: false,
            mock_speed_index: 2, // 1×
        }
    }

    /// Whether this tab owns session `id` (the data-routing match: a tab never
    /// processes another session's events, §3.6).
    pub fn owns_session(&self, id: u64) -> bool {
        self.session_ids.contains(&id)
    }

    /// The session the tab currently talks to (the newest of the lineage).
    pub fn current_session(&self) -> Option<u64> {
        self.session_ids.last().copied()
    }

    /// Whether the tab is bound to a mock port (the mock drawer's visibility
    /// — `PortInfo::kind == Mock(_)`, AC9).
    pub fn is_mock(&self) -> bool {
        self.port
            .as_ref()
            .is_some_and(|info| matches!(info.kind, PortKind::Mock(_)))
    }

    /// Whether the port is usable-open (chip green/amber: Close must stay
    /// available to cancel auto-reconnect).
    pub fn is_active_state(&self) -> bool {
        matches!(
            self.state,
            PortState::Open | PortState::Reopened | PortState::WatchDesired
        )
    }

    /// Appends a line to the model (capped); returns whether the line is
    /// visible under the tab's current presentation (the caller appends the
    /// rendered row to the live view only then).
    pub fn push_line(&mut self, line: LogLine) -> bool {
        let visible = line.visible(&self.filter, self.hide_sent, self.encoding);
        if self.lines.len() >= ROW_CAP {
            self.lines.remove(0);
        }
        self.lines.push(line);
        visible
    }

    /// Re-filters the whole model under the current presentation (the encoding
    /// switch, the filter box, hide-sent and view-mode flips all re-render
    /// history without new traffic — §3.3).
    pub fn visible_lines(&self) -> Vec<&LogLine> {
        self.lines
            .iter()
            .filter(|line| line.visible(&self.filter, self.hide_sent, self.encoding))
            .collect()
    }

    /// The `shown N / M rows` status (the filter's feedback).
    pub fn shown_status(&self) -> (usize, usize) {
        (self.visible_lines().len(), self.lines.len())
    }

    /// The tab caption: port name + the ● (open) / ○ (closed) state marker
    /// (§2.2: header text = port name; the marker mirrors the chip).
    pub fn tab_caption(&self) -> String {
        let marker = if self.is_active_state() { "●" } else { "○" };
        let name = if self.title.is_empty() { "—" } else { &self.title };
        format!("{name} {marker}")
    }

    /// The quick-send header's hook badge: `⌁ hook` (lit, amber) while the
    /// session's pre-send hook is armed, `hook off` (dim) otherwise (§3.5).
    pub fn hook_badge(&self) -> (&'static str, bool) {
        if self.hook_armed {
            ("⌁ hook", true)
        } else {
            ("hook off", false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_line(direction: rcom::core::bus::Direction, bytes: &[u8]) -> LogLine {
        LogLine::Data {
            stamp: "03:04:05.123".into(),
            direction,
            bytes: bytes.to_vec(),
            hooked: false,
        }
    }

    /// Per-session isolation (AC7's viewmodel half): two tabs never share
    /// lines, and each filters by its own presentation state.
    #[test]
    fn tabs_are_isolated_view_models() {
        let mut a = SessionVm::new(0);
        let mut b = SessionVm::new(1);
        a.session_ids.push(10);
        b.session_ids.push(11);

        assert!(a.owns_session(10));
        assert!(!a.owns_session(11));
        assert_eq!(a.current_session(), Some(10));

        a.push_line(data_line(rcom::core::bus::Direction::Rx, b"alpha"));
        b.push_line(data_line(rcom::core::bus::Direction::Tx, b"beta"));
        assert_eq!(a.lines.len(), 1);
        assert_eq!(b.lines.len(), 1);
        assert_eq!(a.visible_lines().len(), 1);
        assert_eq!(b.visible_lines().len(), 1);

        // b hides sent: its model keeps the row, its view drops it; a is
        // untouched.
        b.hide_sent = true;
        assert_eq!(b.visible_lines().len(), 0);
        assert_eq!(b.lines.len(), 1);
        assert_eq!(a.visible_lines().len(), 1);

        // Independent encodings: a reads GBK, b stays UTF-8.
        a.encoding = TextEncoding::Gbk;
        assert_ne!(a.encoding, b.encoding);
    }

    /// The row cap: the model rotates the oldest out (the view re-renders from
    /// the model, so the cap is the whole story).
    #[test]
    fn row_cap_rotates_the_oldest() {
        let mut vm = SessionVm::new(0);
        for i in 0..(ROW_CAP + 10) {
            vm.push_line(data_line(rcom::core::bus::Direction::Rx, format!("{i}").as_bytes()));
        }
        assert_eq!(vm.lines.len(), ROW_CAP);
        let first = match &vm.lines[0] {
            LogLine::Data { bytes, .. } => String::from_utf8_lossy(bytes).to_string(),
            _ => panic!("data line expected"),
        };
        assert_eq!(first, "10", "the first ten rotated out");
    }

    /// The filter applies to history (visible_lines) and live rows (push_line's
    /// return) alike.
    #[test]
    fn filter_applies_to_history_and_live_rows() {
        let mut vm = SessionVm::new(0);
        vm.push_line(data_line(rcom::core::bus::Direction::Rx, b"temperature=21"));
        vm.push_line(data_line(rcom::core::bus::Direction::Rx, b"status=ok"));
        vm.filter = "temp".into();
        assert_eq!(vm.shown_status(), (1, 2));
        assert!(
            !vm.push_line(data_line(rcom::core::bus::Direction::Rx, b"unrelated")),
            "a non-matching live row is not visible"
        );
        assert!(
            vm.push_line(data_line(rcom::core::bus::Direction::Rx, b"temperature=22")),
            "a matching live row is visible"
        );
        assert_eq!(vm.shown_status(), (2, 4));
    }

    /// The tab caption mirrors the chip (● open / ○ closed) and carries the
    /// port name.
    #[test]
    fn tab_caption_mirrors_the_chip() {
        let mut vm = SessionVm::new(0);
        assert_eq!(vm.tab_caption(), "— ○");
        vm.title = "MOCK0".into();
        assert_eq!(vm.tab_caption(), "MOCK0 ○");
        vm.state = PortState::Open;
        assert_eq!(vm.tab_caption(), "MOCK0 ●");
        vm.state = PortState::WatchDesired;
        assert_eq!(vm.tab_caption(), "MOCK0 ●", "watching is still an active port");
    }

    /// The hook badge flips with the armed state.
    #[test]
    fn hook_badge_flips_with_the_hook() {
        let mut vm = SessionVm::new(0);
        assert_eq!(vm.hook_badge(), ("hook off", false));
        vm.hook_armed = true;
        assert_eq!(vm.hook_badge(), ("⌁ hook", true));
        assert_eq!(super::super::quicksend::PAGE_COUNT, 10, "the pager's ten pages (§3.5)");
    }

    /// Mock visibility keys off the port kind (AC9): the tab shows the drawer
    /// only for `PortKind::Mock(_)` listings.
    #[test]
    fn mock_drawer_keys_off_the_port_kind() {
        use rcom::core::port::MockScenario;
        let mut vm = SessionVm::new(0);
        vm.port = Some(PortInfo {
            name: "COM3".into(),
            vid: None,
            pid: None,
            description: None,
            kind: PortKind::Real,
        });
        assert!(!vm.is_mock());
        vm.port = Some(PortInfo {
            name: "MOCK0".into(),
            vid: None,
            pid: None,
            description: None,
            kind: PortKind::Mock(MockScenario::Echo),
        });
        assert!(vm.is_mock());
    }
}
