use crate::entirely::{EntirelyCoordinator, EntirelyHoldGuard};
use crate::helper_ipc::{HelperClient, HelperHoldGuard, is_connect_error};
use crate::power_management::{self, AssertionType, PowerAssertion};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SleepMode {
    Display,
    Disk,
    #[default]
    System,
    SystemOnAc,
    UserActive,
    Entirely,
}

/// How to acquire entirely-mode sleep prevention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntirelyPolicy {
    /// Prefer helper; fall back to in-process lockfile when the helper is unreachable (CLI).
    HelperOrLocalFallback,
    /// Helper required; fails with [`EnableError::HelperUnavailable`] when unreachable.
    HelperRequired,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnableError {
    HelperUnavailable,
    Iokit(u32),
    Ipc(String),
}

impl std::fmt::Display for EnableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnableError::HelperUnavailable => {
                f.write_str("entirely mode requires the privileged helper")
            }
            EnableError::Iokit(code) => write!(f, "IOKit error: {code:X}"),
            EnableError::Ipc(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EnableError {}

/// RAII hold for entirely mode (helper IPC or local lockfile).
pub enum EntirelyHold {
    Local(EntirelyHoldGuard),
    Helper(HelperHoldGuard),
}

fn acquire_entirely(verbose: bool, policy: EntirelyPolicy) -> Result<EntirelyHold, EnableError> {
    let client = HelperClient::new();

    match policy {
        EntirelyPolicy::HelperOrLocalFallback => match HelperHoldGuard::try_acquire(&client) {
            Ok(guard) => Ok(EntirelyHold::Helper(guard)),
            Err(error) if is_connect_error(&error) => EntirelyCoordinator::cli_fallback(verbose)
                .hold_current_process()
                .map(EntirelyHold::Local)
                .map_err(|e| EnableError::Ipc(e.to_string())),
            Err(error) => Err(EnableError::Ipc(error)),
        },
        EntirelyPolicy::HelperRequired => HelperHoldGuard::try_acquire(&client)
            .map(EntirelyHold::Helper)
            .map_err(|error| {
                if is_connect_error(&error) {
                    EnableError::HelperUnavailable
                } else {
                    EnableError::Ipc(error)
                }
            }),
    }
}

impl SleepMode {
    pub fn label(self) -> &'static str {
        match self {
            SleepMode::Display => "Display",
            SleepMode::Disk => "Disk",
            SleepMode::System => "System",
            SleepMode::SystemOnAc => "System (on AC)",
            SleepMode::UserActive => "User active",
            SleepMode::Entirely => "Entirely",
        }
    }

    pub fn all() -> [SleepMode; 6] {
        [
            SleepMode::Display,
            SleepMode::Disk,
            SleepMode::System,
            SleepMode::SystemOnAc,
            SleepMode::UserActive,
            SleepMode::Entirely,
        ]
    }

    fn assertion_type(self) -> Option<AssertionType> {
        match self {
            SleepMode::Display => Some(AssertionType::PreventUserIdleDisplaySleep),
            SleepMode::Disk => Some(AssertionType::PreventDiskIdle),
            SleepMode::System => Some(AssertionType::PreventUserIdleSystemSleep),
            SleepMode::SystemOnAc => Some(AssertionType::PreventSystemSleep),
            SleepMode::UserActive | SleepMode::Entirely => None,
        }
    }

    pub fn enable(
        self,
        verbose: bool,
        entirely_policy: EntirelyPolicy,
    ) -> Result<ActiveSleepHold, EnableError> {
        match self {
            SleepMode::UserActive => power_management::declare_user_activity(true, verbose)
                .map(ActiveSleepHold::Assertion)
                .map_err(EnableError::Iokit),
            SleepMode::Entirely => {
                acquire_entirely(verbose, entirely_policy).map(ActiveSleepHold::Entirely)
            }
            mode => {
                let Some(assertion_type) = mode.assertion_type() else {
                    return Err(EnableError::Ipc(format!(
                        "no IOKit assertion for {}",
                        mode.label()
                    )));
                };
                power_management::create_assertion(assertion_type, true, verbose)
                    .map(ActiveSleepHold::Assertion)
                    .map_err(EnableError::Iokit)
            }
        }
    }

    /// Enable sleep prevention for the tray, installing the privileged helper for entirely mode when needed.
    pub fn enable_for_tray(self) -> Result<ActiveSleepHold, EnableError> {
        match self.enable(false, TRAY_ENTIRELY_POLICY) {
            Err(EnableError::HelperUnavailable) if self == SleepMode::Entirely => {
                crate::install::install_helper_privileged().map_err(EnableError::Ipc)?;
                // launchd starts the helper asynchronously; give the socket a
                // few seconds to appear before declaring failure.
                for _ in 0..25 {
                    match self.enable(false, TRAY_ENTIRELY_POLICY) {
                        Err(EnableError::HelperUnavailable) => {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        other => return other,
                    }
                }
                Err(EnableError::Ipc(
                    "helper is not running after install; try: sudo caffeinate2 install-helper"
                        .to_string(),
                ))
            }
            other => other,
        }
    }
}

/// One active sleep-prevention hold (IOKit assertion or entirely-mode lock).
pub enum ActiveSleepHold {
    Assertion(PowerAssertion),
    Entirely(EntirelyHold),
}

/// All holds for a running CLI or tray session.
#[derive(Default)]
pub struct ActiveSession {
    pub holds: Vec<ActiveSleepHold>,
}

impl ActiveSession {
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty()
    }
}

/// Enabled sleep modes (CLI may enable several at once).
#[derive(Debug, Clone, Default)]
pub struct SleepModeSet(HashSet<SleepMode>);

impl SleepModeSet {
    pub fn insert(&mut self, mode: SleepMode) {
        self.0.insert(mode);
    }

    pub fn contains(&self, mode: SleepMode) -> bool {
        self.0.contains(&mode)
    }

    pub fn apply_defaults(&mut self) {
        if self.0.is_empty() {
            self.0.insert(SleepMode::System);
        }
    }

    pub fn selected_labels(&self) -> Vec<&'static str> {
        self.iter_enabled().map(SleepMode::label).collect()
    }

    pub fn iter_enabled(&self) -> impl Iterator<Item = SleepMode> + '_ {
        SleepMode::all()
            .into_iter()
            .filter(|mode| self.0.contains(mode))
    }

    pub fn enable_all(
        &self,
        verbose: bool,
        dry_run: bool,
    ) -> Result<ActiveSession, EnableError> {
        if dry_run {
            return Ok(ActiveSession::default());
        }

        let mut holds = Vec::new();
        for mode in self.iter_enabled() {
            holds.push(mode.enable(verbose, EntirelyPolicy::HelperOrLocalFallback)?);
        }

        if verbose && !holds.is_empty() {
            println!("Assertions created");
        }

        Ok(ActiveSession { holds })
    }
}

/// Entirely policy for the menu bar (helper required).
pub const TRAY_ENTIRELY_POLICY: EntirelyPolicy = EntirelyPolicy::HelperRequired;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_system_when_no_flags_set() {
        let mut set = SleepModeSet::default();
        set.apply_defaults();
        assert!(set.contains(SleepMode::System));
        assert_eq!(set.selected_labels(), vec!["System"]);
    }

    #[test]
    fn explicit_flags_skip_default_system() {
        let mut set = SleepModeSet::default();
        set.insert(SleepMode::Display);
        set.insert(SleepMode::UserActive);
        assert_eq!(set.selected_labels(), vec!["Display", "User active"]);
    }

    #[test]
    fn dry_run_enables_none() {
        let mut set = SleepModeSet::default();
        for mode in SleepMode::all() {
            set.insert(mode);
        }
        let active = set.enable_all(false, true).unwrap();
        assert!(active.is_empty());
    }
}
