pub mod detach;

use clap::Parser;

/// One-shot LaunchAgent maintenance action selected via an exclusive flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceCommand {
    InstallLaunchAgent,
    UninstallLaunchAgent,
    LaunchAgentStatus,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Install the tray LaunchAgent for start-at-login
    /// (writes ~/Library/LaunchAgents/com.randomblock1.caffeinate2-tray.plist).
    /// Cannot be combined with other options.
    #[arg(long, exclusive = true)]
    pub install_launch_agent: bool,

    /// Remove the tray LaunchAgent.
    /// Cannot be combined with other options.
    #[arg(long, exclusive = true)]
    pub uninstall_launch_agent: bool,

    /// Show whether the tray LaunchAgent is installed and print its path.
    /// Cannot be combined with other options.
    #[arg(long, exclusive = true)]
    pub launch_agent_status: bool,

    /// Detach from the terminal and run in the background.
    /// Returns control to the shell immediately; the tray keeps running
    /// after the terminal closes. stderr is captured, best-effort, to
    /// ~/Library/Logs/caffeinate2-tray.log (appended, never rotated), and is
    /// discarded only if that file cannot be opened.
    #[arg(short = 'd', long)]
    pub detach: bool,
}

impl Args {
    pub const fn maintenance_command(&self) -> Option<MaintenanceCommand> {
        if self.install_launch_agent {
            Some(MaintenanceCommand::InstallLaunchAgent)
        } else if self.uninstall_launch_agent {
            Some(MaintenanceCommand::UninstallLaunchAgent)
        } else if self.launch_agent_status {
            Some(MaintenanceCommand::LaunchAgentStatus)
        } else {
            None
        }
    }
}

#[cfg(test)]
pub fn parse_args(args: &[&str]) -> Args {
    Args::try_parse_from(args).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_flags_are_exclusive() {
        let args = parse_args(&["caffeinate2-tray", "--install-launch-agent"]);
        assert_eq!(
            args.maintenance_command(),
            Some(MaintenanceCommand::InstallLaunchAgent)
        );

        let args = parse_args(&["caffeinate2-tray", "--uninstall-launch-agent"]);
        assert_eq!(
            args.maintenance_command(),
            Some(MaintenanceCommand::UninstallLaunchAgent)
        );

        let args = parse_args(&["caffeinate2-tray", "--launch-agent-status"]);
        assert_eq!(
            args.maintenance_command(),
            Some(MaintenanceCommand::LaunchAgentStatus)
        );
    }

    #[test]
    fn default_runs_tray_without_maintenance_command() {
        let args = parse_args(&["caffeinate2-tray"]);
        assert_eq!(args.maintenance_command(), None);
        assert!(!args.detach);
    }

    #[test]
    fn detach_parses_without_maintenance_command() {
        let args = parse_args(&["caffeinate2-tray", "-d"]);
        assert!(args.detach);
        assert_eq!(args.maintenance_command(), None);

        let args = parse_args(&["caffeinate2-tray", "--detach"]);
        assert!(args.detach);
        assert_eq!(args.maintenance_command(), None);
    }

    #[test]
    fn detach_conflicts_with_maintenance_flags() {
        assert!(
            Args::try_parse_from(["caffeinate2-tray", "-d", "--install-launch-agent"]).is_err()
        );
        assert!(
            Args::try_parse_from(["caffeinate2-tray", "--install-launch-agent", "-d"]).is_err()
        );
        assert!(
            Args::try_parse_from(["caffeinate2-tray", "-d", "--uninstall-launch-agent"]).is_err()
        );
        assert!(Args::try_parse_from(["caffeinate2-tray", "-d", "--launch-agent-status"]).is_err());
    }
}
