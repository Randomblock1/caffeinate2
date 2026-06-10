use crate::sleep_mode::{SleepMode, SleepModeSet};
use clap::{Parser, Subcommand};

#[derive(Subcommand, Debug)]
pub enum MaintenanceCommand {
    /// Install the privileged helper for entirely mode (requires root).
    InstallHelper,
    /// Remove the privileged helper (requires root).
    UninstallHelper,
    /// Internal entry point used after administrator authorization.
    #[command(hide = true, name = "install-helper-internal")]
    InstallHelperInternal,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None, args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<MaintenanceCommand>,
    #[command(flatten)]
    pub args: Args,
}

#[derive(Parser, Debug)]
pub struct Args {
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
    #[arg()]
    pub command: Option<Vec<String>>,
}

impl Args {
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
    Cli::try_parse_from(args).unwrap().args
}
