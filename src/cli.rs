use crate::sleep_mode::{SleepMode, SleepModeSet};
use clap::Parser;

/// One-shot maintenance action selected via an exclusive flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceCommand {
    InstallHelper,
    UninstallHelper,
    InstallHelperInternal,
    Status,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Install the privileged helper for entirely mode
    /// (prompts for administrator authorization; or run with sudo).
    /// Cannot be combined with other options.
    #[arg(long, exclusive = true)]
    pub install_helper: bool,

    /// Remove the privileged helper (requires root).
    /// Cannot be combined with other options.
    #[arg(long, exclusive = true)]
    pub uninstall_helper: bool,

    /// Internal entry point used after administrator authorization.
    #[arg(long, exclusive = true, hide = true)]
    pub install_helper_internal: bool,

    /// Show entirely-mode helper status (holders and sleep state).
    /// Cannot be combined with other options.
    #[arg(long, exclusive = true)]
    pub status: bool,

    /// Verbose mode
    #[arg(short, long)]
    pub verbose: bool,

    /// Dry run. Don't actually prevent sleep.
    /// Useful for testing.
    #[arg(long)]
    pub dry_run: bool,

    /// Drop root privileges in command.
    /// You need root to disable sleep entirely,
    /// but some programs don't want to run as root.
    #[arg(long)]
    pub drop_root: bool,

    /// Run COMMAND through /bin/sh -c instead of executing it directly.
    #[arg(long)]
    pub shell: bool,

    /// Disable display sleep
    #[arg(short, long)]
    pub display: bool,

    /// Disable disk idle sleep
    #[arg(short = 'm', long)]
    pub disk: bool,

    /// Disable idle system sleep. [DEFAULT]
    #[arg(short = 'i', long)]
    pub system: bool,

    /// Disable system sleep while not on battery
    #[arg(short, long)]
    pub system_on_ac: bool,

    /// Disable system sleep entirely (ignores lid closing)
    #[arg(short, long)]
    pub entirely: bool,

    /// Declare the user is active.
    /// If the display is off, this option turns it on and prevents it from going into idle sleep.
    #[arg(short, long)]
    pub user_active: bool,

    /// Wait for X seconds.
    /// Also supports time units (like "1 day 2 hours 3mins 4s").
    #[arg(short, long, name = "DURATION")]
    pub timeout: Option<String>,

    /// Wait for program with PID X to complete and pass its exit code.
    #[arg(short, long, name = "PID")]
    pub waitfor: Option<i32>,

    /// Wait for given command to complete (takes priority above timeout and pid)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Option<Vec<String>>,
}

impl Args {
    pub fn maintenance_command(&self) -> Option<MaintenanceCommand> {
        // The flags are `exclusive`, so clap guarantees at most one is set.
        if self.install_helper {
            Some(MaintenanceCommand::InstallHelper)
        } else if self.uninstall_helper {
            Some(MaintenanceCommand::UninstallHelper)
        } else if self.install_helper_internal {
            Some(MaintenanceCommand::InstallHelperInternal)
        } else if self.status {
            Some(MaintenanceCommand::Status)
        } else {
            None
        }
    }

    pub fn sleep_modes(&self) -> SleepModeSet {
        let mut set = SleepModeSet::default();
        if self.display {
            set.insert(SleepMode::Display);
        }
        if self.disk {
            set.insert(SleepMode::Disk);
        }
        if self.system {
            set.insert(SleepMode::System);
        }
        if self.system_on_ac {
            set.insert(SleepMode::SystemOnAc);
        }
        if self.entirely {
            set.insert(SleepMode::Entirely);
        }
        if self.user_active {
            set.insert(SleepMode::UserActive);
        }
        set
    }
}

#[cfg(test)]
pub(crate) fn parse_args(args: &[&str]) -> Args {
    Args::try_parse_from(args).unwrap()
}
