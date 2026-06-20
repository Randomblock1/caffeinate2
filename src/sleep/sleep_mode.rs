use crate::entirely::coordinator::{EntirelyCoordinator, EntirelyHoldGuard};
use crate::entirely::helper_ipc::{HelperClient, HelperHoldGuard, is_authorization_error, is_connect_error};
use crate::sleep::power_management::{self, AssertionType, PowerAssertion};
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
    /// The helper is running but denied the hold (peer is not root, an
    /// administrator, or a member of the grant group). The message contains
    /// the grant instructions; installing the helper again won't help.
    NotAuthorized(String),
    Iokit(u32),
    Ipc(String),
}

impl std::fmt::Display for EnableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HelperUnavailable => {
                f.write_str("entirely mode requires the privileged helper")
            }
            Self::NotAuthorized(message) | Self::Ipc(message) => f.write_str(message),
            Self::Iokit(code) => write!(f, "IOKit error: {code:X}"),
        }
    }
}

impl std::error::Error for EnableError {}

/// RAII hold for entirely mode (helper IPC or local lockfile).
pub enum EntirelyHold {
    Local(EntirelyHoldGuard),
    Helper(HelperHoldGuard),
}

/// Map a helper RPC error string onto the typed error the UIs dispatch on.
fn classify_helper_error(error: String) -> EnableError {
    if is_connect_error(&error) {
        EnableError::HelperUnavailable
    } else if is_authorization_error(&error) {
        EnableError::NotAuthorized(error)
    } else {
        EnableError::Ipc(error)
    }
}

fn acquire_entirely(verbose: bool, policy: EntirelyPolicy) -> Result<EntirelyHold, EnableError> {
    let client = HelperClient::new();

    match policy {
        EntirelyPolicy::HelperOrLocalFallback => match HelperHoldGuard::try_acquire(&client) {
            Ok(guard) => Ok(EntirelyHold::Helper(guard)),
            Err(error) if is_connect_error(&error) => {
                // The local fallback toggles the system SleepDisabled setting
                // directly, which only works as root. Fail fast with a clear
                // error instead of writing a lockfile entry and surfacing the
                // IOKit not-privileged code.
                if nix::unistd::Uid::effective().is_root() {
                    EntirelyCoordinator::cli_fallback(verbose)
                        .hold_current_process()
                        .map(EntirelyHold::Local)
                        .map_err(|e| EnableError::Ipc(e.to_string()))
                } else {
                    Err(EnableError::HelperUnavailable)
                }
            }
            Err(error) => Err(classify_helper_error(error)),
        },
        EntirelyPolicy::HelperRequired => HelperHoldGuard::try_acquire(&client)
            .map(EntirelyHold::Helper)
            .map_err(classify_helper_error),
    }
}

impl SleepMode {
    #[must_use] 
    pub const fn label(self) -> &'static str {
        match self {
            Self::Display => "Display",
            Self::Disk => "Disk",
            Self::System => "System",
            Self::SystemOnAc => "System (on AC)",
            Self::UserActive => "User active",
            Self::Entirely => "Entirely",
        }
    }

    #[must_use] 
    pub const fn all() -> [Self; 6] {
        [
            Self::Display,
            Self::Disk,
            Self::System,
            Self::SystemOnAc,
            Self::UserActive,
            Self::Entirely,
        ]
    }

    const fn assertion_type(self) -> Option<AssertionType> {
        match self {
            Self::Display => Some(AssertionType::PreventUserIdleDisplaySleep),
            Self::Disk => Some(AssertionType::PreventDiskIdle),
            Self::System => Some(AssertionType::PreventUserIdleSystemSleep),
            Self::SystemOnAc => Some(AssertionType::PreventSystemSleep),
            Self::UserActive | Self::Entirely => None,
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error if sleep prevention cannot be enabled for this mode.
    pub fn enable(
        self,
        verbose: bool,
        entirely_policy: EntirelyPolicy,
    ) -> Result<ActiveSleepHold, EnableError> {
        match self {
            Self::UserActive => power_management::declare_user_activity(verbose)
                .map(ActiveSleepHold::Assertion)
                .map_err(EnableError::Iokit),
            Self::Entirely => {
                acquire_entirely(verbose, entirely_policy).map(ActiveSleepHold::Entirely)
            }
            mode => {
                let Some(assertion_type) = mode.assertion_type() else {
                    return Err(EnableError::Ipc(format!(
                        "no IOKit assertion for {}",
                        mode.label()
                    )));
                };
                power_management::create_assertion(assertion_type, verbose)
                    .map(ActiveSleepHold::Assertion)
                    .map_err(EnableError::Iokit)
            }
        }
    }

    /// Enable sleep prevention for the tray, installing the privileged helper for entirely mode when needed.
///
/// # Errors
///
/// Returns an error if tray sleep prevention or helper installation fails.
    pub fn enable_for_tray(self) -> Result<ActiveSleepHold, EnableError> {
        match self.enable(false, TRAY_ENTIRELY_POLICY) {
            Err(EnableError::HelperUnavailable) if self == Self::Entirely => {
                crate::entirely::install::install_helper_privileged().map_err(EnableError::Ipc)?;
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
                    "helper is not running after install; try: sudo caffeinate2 --install-helper"
                        .to_string(),
                ))
            }
            other => other,
        }
    }
}

/// One active sleep-prevention hold (`IOKit` assertion or entirely-mode lock).
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
    #[must_use] 
    pub const fn is_empty(&self) -> bool {
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

    #[must_use] 
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

    ///
    /// # Errors
    ///
    /// Returns an error if any selected sleep mode cannot be enabled.
    pub fn enable_all(&self, verbose: bool, dry_run: bool) -> Result<ActiveSession, EnableError> {
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
    fn helper_errors_classify_by_prefix() {
        assert_eq!(
            classify_helper_error("connect failed: no socket".to_string()),
            EnableError::HelperUnavailable
        );
        assert_eq!(
            classify_helper_error("not authorized: nope".to_string()),
            EnableError::NotAuthorized("not authorized: nope".to_string())
        );
        assert_eq!(
            classify_helper_error("read failed: timeout".to_string()),
            EnableError::Ipc("read failed: timeout".to_string())
        );
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
