use crate::helper_ipc::HelperHoldGuard;
use crate::power_management::{self, AssertionType, PowerAssertion};
use serde::{Deserialize, Serialize};

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

    pub fn enable(self, verbose: bool) -> Result<ActiveSleepHold, EnableError> {
        match self {
            SleepMode::UserActive => power_management::declare_user_activity(true, verbose)
                .map(ActiveSleepHold::Assertion)
                .map_err(EnableError::Iokit),
            SleepMode::Entirely => {
                if !crate::helper_ipc::HelperClient::new().is_available() {
                    return Err(EnableError::HelperUnavailable);
                }
                HelperHoldGuard::acquire()
                    .map(ActiveSleepHold::Entirely)
                    .map_err(EnableError::Ipc)
            }
            mode => {
                let assertion_type = mode.assertion_type().expect("non-entirely modes have assertions");
                power_management::create_assertion(assertion_type, true, verbose)
                    .map(ActiveSleepHold::Assertion)
                    .map_err(EnableError::Iokit)
            }
        }
    }
}

/// Active sleep prevention for tray (single mode) or CLI building blocks.
pub enum ActiveSleepHold {
    Assertion(PowerAssertion),
    Entirely(HelperHoldGuard),
}

/// CLI flags for one or more simultaneous sleep modes.
#[derive(Debug, Clone, Default)]
pub struct SleepModeSet {
    pub display: bool,
    pub disk: bool,
    pub system: bool,
    pub system_on_ac: bool,
    pub entirely: bool,
    pub user_active: bool,
}

impl SleepModeSet {
    pub fn apply_defaults(&mut self) {
        if !(self.display
            || self.disk
            || self.system
            || self.system_on_ac
            || self.entirely
            || self.user_active)
        {
            self.system = true;
        }
    }

    pub fn selected_labels(&self) -> Vec<&'static str> {
        self.iter_enabled().map(SleepMode::label).collect()
    }

    pub fn iter_enabled(&self) -> impl Iterator<Item = SleepMode> + '_ {
        SleepMode::all()
            .into_iter()
            .filter(|mode| self.is_enabled(*mode))
    }

    pub fn is_enabled(&self, mode: SleepMode) -> bool {
        match mode {
            SleepMode::Display => self.display,
            SleepMode::Disk => self.disk,
            SleepMode::System => self.system,
            SleepMode::SystemOnAc => self.system_on_ac,
            SleepMode::Entirely => self.entirely,
            SleepMode::UserActive => self.user_active,
        }
    }

    pub fn enable_all(
        &self,
        verbose: bool,
        dry_run: bool,
    ) -> Result<ActiveSleepModes, Box<dyn std::error::Error>> {
        if dry_run {
            return Ok(ActiveSleepModes {
                assertions: Vec::new(),
                entirely: None,
            });
        }

        let entirely = if self.entirely {
            Some(crate::process_lock::acquire_entirely(verbose)?)
        } else {
            None
        };

        let mut assertions = Vec::new();
        for mode in self.iter_enabled() {
            match mode {
                SleepMode::Entirely => {}
                SleepMode::UserActive => {
                    assertions.push(power_management::declare_user_activity(true, verbose).map_err(
                        |code| -> Box<dyn std::error::Error> {
                            std::io::Error::other(format!(
                                "Failed to create user activity assertion (code: {code:X})"
                            ))
                            .into()
                        },
                    )?);
                }
                mode => {
                    let assertion_type = mode.assertion_type().unwrap();
                    assertions.push(
                        power_management::create_assertion(assertion_type, true, verbose).map_err(
                            |code| -> Box<dyn std::error::Error> {
                                std::io::Error::other(format!(
                                    "Failed to create {} assertion (code: {code:X})",
                                    mode.label()
                                ))
                                .into()
                            },
                        )?,
                    );
                }
            }
        }

        if verbose {
            println!("Assertions created");
        }

        Ok(ActiveSleepModes {
            assertions,
            entirely,
        })
    }
}

pub struct ActiveSleepModes {
    pub assertions: Vec<PowerAssertion>,
    pub entirely: Option<crate::process_lock::EntirelyGuard>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_system_when_no_flags_set() {
        let mut set = SleepModeSet::default();
        set.apply_defaults();
        assert!(set.system);
        assert_eq!(set.selected_labels(), vec!["System"]);
    }

    #[test]
    fn explicit_flags_skip_default_system() {
        let set = SleepModeSet {
            display: true,
            user_active: true,
            ..Default::default()
        };
        assert_eq!(set.selected_labels(), vec!["Display", "User active"]);
    }
}
