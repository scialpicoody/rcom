//! The backend factory (design-ui §5.7): the one place where "real" and
//! "mock" meet. `SerialBackend` implementations sit behind it — the real
//! `serialport` driver and the scenario-playing [`MockBackend`](super::mock)
//! — and the selection happens at **runtime, from `RCOM_MOCK`**, with the mock
//! compiled in always (no cargo feature: a feature would fork `src/app` with
//! `#[cfg]` and break the zero-UI-changes promise).
//!
//! ```text
//! RCOM_MOCK = off | all | echo | noisy | framed | unplug
//!   off (default in release builds)  → real backend only
//!   all (default in debug builds)    → MOCK0 echo, MOCK1 noisy, MOCK2 framed, MOCK3 unplug
//!   <scenario>                       → mock backend with just that scenario as MOCK0
//! ```
//!
//! Everything downstream — sessions, bus, log store, the UI — sees one
//! `SerialBackend`; the swap is invisible by construction.

use std::sync::Arc;

use super::mock::MockScenario;
use super::serial::SerialPortBackend;
use super::{PortInfo, SerialBackend};
use crate::core::error::Result;

/// What the mock seam should present (design-ui §5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockMode {
    /// Real backend only (no mock ports listed).
    Off,
    /// The four scenarios as `MOCK0`–`MOCK3`.
    All,
    /// One scenario as `MOCK0`.
    Single(MockScenario),
}

impl MockMode {
    /// The `RCOM_MOCK` value behind this mode (`None`-safe display for traces).
    pub fn env_value(self) -> &'static str {
        match self {
            MockMode::Off => "off",
            MockMode::All => "all",
            MockMode::Single(scenario) => scenario.id(),
        }
    }
}

/// Reads `RCOM_MOCK`; `None` when unset (or unparseable — traced, then the
/// caller's default applies).
pub fn mock_mode_from_env() -> Option<MockMode> {
    let value = std::env::var("RCOM_MOCK").ok()?;
    match value.trim() {
        "" | "off" | "0" | "false" => Some(MockMode::Off),
        "all" | "1" | "true" => Some(MockMode::All),
        other => match MockScenario::from_id(other) {
            Some(scenario) => Some(MockMode::Single(scenario)),
            None => {
                tracing::warn!("RCOM_MOCK={other:?} is not off|all|echo|noisy|framed|unplug — ignored");
                None
            }
        },
    }
}

/// The default when `RCOM_MOCK` is unset (design-ui §5.7): all scenarios in
/// debug builds, the real backend in release builds.
pub fn mock_mode_default_for_profile() -> MockMode {
    if cfg!(debug_assertions) {
        MockMode::All
    } else {
        MockMode::Off
    }
}

/// The backend behind the engine: real and mock, one `SerialBackend` surface.
pub struct BackendFactory {
    real: SerialPortBackend,
    mock: super::mock::MockBackend,
    mode: MockMode,
}

impl BackendFactory {
    /// Builds the factory from `RCOM_MOCK` (falling back to the profile
    /// default — see [`mock_mode_default_for_profile`]).
    pub fn from_env() -> Self {
        let mode = mock_mode_from_env().unwrap_or_else(mock_mode_default_for_profile);
        Self::with_mode(mode)
    }

    /// Builds the factory with an explicit mode — the path the app's driver
    /// uses when legacy demo env vars pin the seam (an `RCOM_AUTODEMO` run
    /// needs its mock port even in release builds).
    pub fn with_mode(mode: MockMode) -> Self {
        tracing::info!(mock = mode.env_value(), "backend factory selected");
        Self {
            real: SerialPortBackend::new(),
            mock: super::mock::MockBackend::for_mode(mode),
            mode,
        }
    }

    /// The selected mode.
    pub fn mock_mode(&self) -> MockMode {
        self.mode
    }

    /// The playback control of a mock port by name (backend-level, for the
    /// demo harness paths); `None` for real ports or with the mock off.
    pub fn mock_control(&self, name: &str) -> Option<super::mock::MockControl> {
        self.mock.control_for(name)
    }
}

impl SerialBackend for BackendFactory {
    fn enumerate(&self) -> Result<Vec<PortInfo>> {
        match self.mode {
            // Mock modes present only the mock ports (§5.7: "<scenario> → mock
            // backend with just that scenario"): the demo environment is
            // reproducible, no real port leaks into an AC run.
            MockMode::All | MockMode::Single(_) => self.mock.enumerate(),
            MockMode::Off => match self.real.enumerate() {
                Ok(ports) => Ok(ports),
                Err(error) => {
                    tracing::warn!("real port enumeration failed: {error}");
                    Ok(Vec::new())
                }
            },
        }
    }

    fn open(&self, name: &str, config: &super::PortConfig) -> Result<super::OpenPort> {
        match self.mode {
            MockMode::All | MockMode::Single(_) => self.mock.open(name, config),
            MockMode::Off => self.real.open(name, config),
        }
    }
}

/// Convenience alias matching the manager's constructor.
pub fn boxed_factory() -> Arc<dyn SerialBackend> {
    Arc::new(BackendFactory::from_env())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::port::PortConfig;
    use crate::core::port::mock::MOCK_PORT_NAME;

    /// The env parser: off/all aliases, scenario ids, invalid values.
    #[test]
    fn env_parsing_matches_the_documented_vocabulary() {
        // (No env-var writes in parallel tests — the parser itself is pure
        // enough to exercise through a helper; here we pin the defaults.)
        assert_eq!(mock_mode_default_for_profile(), {
            if cfg!(debug_assertions) {
                MockMode::All
            } else {
                MockMode::Off
            }
        });
        assert_eq!(MockScenario::from_id("echo"), Some(MockScenario::Echo));
        assert_eq!(MockScenario::from_id("nope"), None);
        assert_eq!(MockMode::Single(MockScenario::Echo).env_value(), "echo");
        assert_eq!(MockMode::All.env_value(), "all");
        assert_eq!(MockMode::Off.env_value(), "off");
    }

    /// The factory's mode split: a mock mode lists only mocks; `off` lists no
    /// mock at all and routes `MOCK0` opens to the real driver (which fails —
    /// there is no such port — proving the routing, not the hardware).
    #[test]
    fn factory_selects_by_mode() {
        let mock_factory = BackendFactory::with_mode(MockMode::Single(MockScenario::Echo));
        let ports = mock_factory.enumerate().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, MOCK_PORT_NAME);
        assert!(ports[0].is_mock());
        // The mock port opens through the factory surface.
        assert!(mock_factory.open(MOCK_PORT_NAME, &PortConfig::new(115_200)).is_ok());

        let off_factory = BackendFactory::with_mode(MockMode::Off);
        let ports = off_factory.enumerate().unwrap();
        assert!(
            ports.iter().all(|info| !info.is_mock()),
            "off must not list mock ports"
        );
        assert!(
            off_factory.open(MOCK_PORT_NAME, &PortConfig::new(115_200)).is_err(),
            "with the mock off, MOCK0 must not open"
        );

        let all_factory = BackendFactory::with_mode(MockMode::All);
        assert_eq!(all_factory.enumerate().unwrap().len(), 4);
    }

    /// The mock mode is reported back (the app's demo paths branch on it).
    #[test]
    fn factory_reports_its_mode() {
        assert_eq!(BackendFactory::with_mode(MockMode::All).mock_mode(), MockMode::All);
        assert_eq!(
            BackendFactory::with_mode(MockMode::Single(MockScenario::Unplug)).mock_mode(),
            MockMode::Single(MockScenario::Unplug)
        );
    }
}
