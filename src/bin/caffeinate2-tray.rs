#[cfg(all(target_os = "macos", feature = "tray"))]
mod tray_cli;

#[cfg(all(target_os = "macos", feature = "tray"))]
use anyhow::Context;
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::entirely::install;
#[cfg(all(target_os = "macos", feature = "tray"))]
use caffeinate2::util::logging;
#[cfg(all(target_os = "macos", feature = "tray"))]
use clap::Parser;
#[cfg(all(target_os = "macos", feature = "tray"))]
use tray_cli::{Args, MaintenanceCommand};

#[cfg(all(target_os = "macos", feature = "tray"))]
fn run_maintenance(command: MaintenanceCommand) -> anyhow::Result<()> {
    match command {
        MaintenanceCommand::InstallLaunchAgent => {
            let tray_path =
                std::env::current_exe().context("could not resolve tray binary path")?;
            install::install_tray_launch_agent(&tray_path)?;
            let plist_path = install::tray_launch_agent_path()?;
            println!("Installed tray LaunchAgent at {}.", plist_path.display());
            println!("caffeinate2-tray will start at next login.");
            Ok(())
        }
        MaintenanceCommand::UninstallLaunchAgent => {
            install::uninstall_tray_launch_agent()?;
            println!("Removed tray LaunchAgent.");
            Ok(())
        }
        MaintenanceCommand::LaunchAgentStatus => {
            let plist_path = install::tray_launch_agent_path()?;
            if install::tray_launch_agent_installed() {
                println!("LaunchAgent: installed");
            } else {
                println!("LaunchAgent: not installed");
            }
            println!("Path: {}", plist_path.display());
            Ok(())
        }
    }
}

#[cfg(all(target_os = "macos", feature = "tray"))]
fn main() {
    // The tray CLI has no verbose flag, so tracing uses the default filter
    // (RUST_LOG still overrides it).
    logging::init_cli_tracing(false);
    let args = Args::parse();
    if let Some(command) = args.maintenance_command() {
        if let Err(error) = run_maintenance(command) {
            tracing::error!("Error: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    if args.detach {
        tray_cli::detach::spawn_detached_or_exit();
    }

    if let Err(e) = caffeinate2::tray::run() {
        tracing::error!("caffeinate2-tray error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(all(target_os = "macos", feature = "tray")))]
fn main() {
    eprintln!("caffeinate2-tray requires macOS and the tray feature.");
    std::process::exit(1);
}
