use crate::entirely::coordinator::{EntirelyCoordinator, EntirelyHoldGuard};
use crate::entirely::error::HelperIpcError;
use crate::entirely::helper_ipc::{
    HelperClient, HelperHoldGuard, is_authorization_error, is_connect_error,
};
use crate::sleep::power_management::{self, AssertionType, PowerAssertion, UserActivityHold};
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

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnableError {
    #[error("entirely mode requires the privileged helper")]
    HelperUnavailable,
    /// The helper is running but denied the hold (peer is not root, an
    /// administrator, or a member of the grant group). The message contains
    /// the grant instructions; installing the helper again won't help.
    #[error("{0}")]
    NotAuthorized(String),
    #[error("IOKit error: {0:X}")]
    Iokit(u32),
    #[error("{0}")]
    Ipc(String),
}

/// Map a helper RPC error onto the typed error the UIs dispatch on.
fn classify_helper_error(error: HelperIpcError) -> EnableError {
    let message = error.to_string();
    if is_connect_error(&message) {
        EnableError::HelperUnavailable
    } else if is_authorization_error(&message) {
        EnableError::NotAuthorized(message)
    } else {
        EnableError::Ipc(message)
    }
}

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
            Err(error) if is_connect_error(&error.to_string()) => {
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
                .map(ActiveSleepHold::UserActivity)
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

    /// Enable sleep prevention for the tray, optionally installing the privileged
    /// helper for entirely mode when it is missing.
    ///
    /// This can block for several seconds (the administrator prompt plus the
    /// post-install socket wait), so the tray runs it on a background thread and
    /// commits the resulting hold back on the main thread — never call it
    /// directly from the UI thread (see `tray::state`).
    ///
    /// Pass `install_helper_if_missing: false` for upgrade-watcher retries: the
    /// watcher installs the helper when the user enables the setting, and
    /// re-prompting on every poll would spam administrator dialogs.
    ///
    /// # Errors
    ///
    /// Returns an error if tray sleep prevention or helper installation fails.
    pub fn enable_for_tray(
        self,
        install_helper_if_missing: bool,
    ) -> Result<ActiveSleepHold, EnableError> {
        match self.enable(false, TRAY_ENTIRELY_POLICY) {
            Err(EnableError::HelperUnavailable)
                if self == Self::Entirely && install_helper_if_missing =>
            {
                crate::entirely::install::install_helper_privileged()
                    .map_err(|e| EnableError::Ipc(e.to_string()))?;
                // launchd starts the helper asynchronously. Because this now
                // runs off the UI thread, we can afford a generous window
                // (~10s) for a slow launchd to bring the socket up before
                // declaring failure.
                for _ in 0..50 {
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

/// One active sleep-prevention hold (`IOKit` assertion, periodically-refreshed
/// user-activity assertion, or entirely-mode lock).
pub enum ActiveSleepHold {
    Assertion(PowerAssertion),
    UserActivity(UserActivityHold),
    Entirely(EntirelyHold),
}

impl ActiveSleepHold {
    /// Delays before each release attempt for a helper-backed entirely hold.
    /// Sized to ride out a helper crash + launchd respawn or a reinstall's
    /// bootout/bootstrap window, while staying well under a user's patience
    /// for "I turned it off".
    const RELEASE_RETRY_DELAYS: [std::time::Duration; 3] = [
        std::time::Duration::ZERO,
        std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(3),
    ];

    /// Release this hold, blocking until done. For a helper-backed entirely
    /// hold this sends the Release RPC and retries briefly when the helper is
    /// transiently unreachable: the drop path would discard the failure, and
    /// a discarded release leaves a live-pid holder entry that the helper's
    /// reaper never prunes — the machine then silently cannot sleep for the
    /// life of this process. Each attempt is bounded by the RPC timeouts
    /// (seconds, not milliseconds), so run this off the UI thread. When every
    /// retry fails, falls back to the plain drop, which frees the in-process
    /// count and lets the next full session cycle heal the helper-side entry.
    pub fn release_blocking(self) {
        let Self::Entirely(EntirelyHold::Helper(mut guard)) = self else {
            // Assertion, user-activity, and local-lockfile holds release
            // synchronously and reliably in their Drop impls.
            return;
        };
        for delay in Self::RELEASE_RETRY_DELAYS {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            match guard.release() {
                Ok(()) => return,
                Err(e) => tracing::warn!("entirely-mode release failed, will retry: {e}"),
            }
        }
        tracing::warn!(
            "entirely-mode release kept failing; discarding the guard (the helper reaps the entry when this process exits, or the next session cycle releases it)"
        );
    }
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

    /// Enable every selected sleep mode.
    ///
    /// `dry_run` skips *only* the actual sleep-prevention holds (no `IOKit`
    /// assertions, no entirely-mode lock): the caller still runs any configured
    /// wait/command/timeout so `--dry-run` exercises the full timing path
    /// without touching the system's power state.
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
            tracing::debug!("Assertions created");
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
            classify_helper_error(HelperIpcError::new("connect failed: no socket".to_string())),
            EnableError::HelperUnavailable
        );
        assert_eq!(
            classify_helper_error(HelperIpcError::connect(std::io::Error::other("no socket"))),
            EnableError::HelperUnavailable
        );
        assert_eq!(
            classify_helper_error(HelperIpcError::internal(std::io::Error::other(
                "Failed to determine process start time"
            ))),
            EnableError::Ipc("internal error: Failed to determine process start time".to_string())
        );
        assert_eq!(
            classify_helper_error(HelperIpcError::new("not authorized: nope".to_string())),
            EnableError::NotAuthorized("not authorized: nope".to_string())
        );
        assert_eq!(
            classify_helper_error(HelperIpcError::new("read failed: timeout".to_string())),
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
