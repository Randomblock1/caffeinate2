#[cfg(target_os = "macos")]
mod cli;
#[cfg(target_os = "macos")]
mod wait;

#[cfg(target_os = "macos")]
use caffeinate2::{duration_parser, install, sleep_mode};
#[cfg(target_os = "macos")]
use clap::Parser;
#[cfg(target_os = "macos")]
use cli::{Args, MaintenanceCommand};
#[cfg(target_os = "macos")]
use nix::unistd;
#[cfg(target_os = "macos")]
use signal_hook::{consts::SIGINT, iterator::Signals};
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "macos")]
use std::process;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicI32, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::thread;
#[cfg(target_os = "macos")]
use wait::{wait_for_pid, wait_mode, WaitForPidError, WaitForPidResult, WaitMode};

#[cfg(target_os = "macos")]
fn run_maintenance(command: MaintenanceCommand) {
    match command {
        MaintenanceCommand::InstallHelper => {
            if let Err(e) = install::install_helper_privileged() {
                eprintln!("Error: {e}");
                process::exit(1);
            }
            println!("Installed caffeinate2 helper.");
        }
        MaintenanceCommand::UninstallHelper => {
            if !nix::unistd::Uid::effective().is_root() {
                eprintln!("Error: --uninstall-helper must run as root (try sudo).");
                process::exit(1);
            }
            if let Err(e) = install::uninstall_helper() {
                eprintln!("Error: {e}");
                process::exit(1);
            }
            println!("Uninstalled caffeinate2 helper.");
        }
        MaintenanceCommand::InstallHelperInternal => {
            if !nix::unistd::Uid::effective().is_root() {
                eprintln!("Error: --install-helper-internal must run as root.");
                process::exit(1);
            }
            let source = match install::resolve_helper_source() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Error: {e}");
                    process::exit(1);
                }
            };
            if let Err(e) = install::install_helper(&source) {
                eprintln!("Error: {e}");
                process::exit(1);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let args = Args::parse();
    if let Some(command) = args.maintenance_command() {
        run_maintenance(command);
        return;
    }

    // Catch verb-style invocations from the old subcommand syntax so they
    // error out instead of running e.g. `/bin/sh -c install-helper` while
    // preventing sleep.
    if let Some(first) = args.command.as_ref().and_then(|c| c.first())
        && matches!(
            first.as_str(),
            "install-helper" | "uninstall-helper" | "install-helper-internal"
        )
    {
        eprintln!("Error: '{first}' is not a command to run. Did you mean: caffeinate2 --{first}?");
        process::exit(2);
    }

    let mut sleep_modes = args.sleep_modes();
    sleep_modes.apply_defaults();

    if args.verbose {
        println!("DEBUG {args:#?}");
    }

    let mut sleep_str = format!(
        "Preventing sleep types: [{}] ",
        sleep_modes.selected_labels().join(", ")
    );

    // Parse the timeout before enabling any sleep prevention: a bad -t value
    // must not toggle the system sleep setting and then exit without
    // releasing it (process::exit skips destructors, and the entirely-mode
    // hold has system-wide effects beyond this process's lifetime).
    let parsed_timeout = match args
        .timeout
        .as_deref()
        .map(duration_parser::parse_duration)
        .transpose()
    {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    };

    let active = match sleep_modes.enable_all(args.verbose, args.dry_run) {
        Ok(active) => active,
        Err(e) => {
            eprintln!("Error: {e}");
            if sleep_modes.contains(sleep_mode::SleepMode::Entirely) {
                eprintln!(
                    "Hint: install the privileged helper with: sudo caffeinate2 --install-helper"
                );
            }
            process::exit(1);
        }
    };

    let active = Arc::new(Mutex::new(Some(active)));
    let active_signal = active.clone();
    let exit_code = Arc::new(AtomicI32::new(0));
    let exit_code_signal = Arc::clone(&exit_code);

    let mut signals = Signals::new([SIGINT]).expect("Failed to create signal iterator");
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            println!("\nStopping...");
            if let Ok(mut guard) = active_signal.lock() {
                let _ = guard.take();
            }
            process::exit(exit_code_signal.load(Ordering::Relaxed));
        }
    });

    match wait_mode(&args) {
        WaitMode::Command => {
            let command = args.command.expect("Command should be present");
            sleep_str += "until command finishes.";
            println!("{sleep_str}");

            let uid;
            let gid;

            if args.drop_root {
                let uid_str =
                    std::env::var("SUDO_UID").unwrap_or_else(|_| unistd::getuid().to_string());
                let gid_str =
                    std::env::var("SUDO_GID").unwrap_or_else(|_| unistd::getgid().to_string());

                uid = uid_str.parse::<u32>().expect("Invalid UID");
                gid = gid_str.parse::<u32>().expect("Invalid GID");
            } else {
                uid = unistd::getuid().into();
                gid = unistd::getgid().into();
            }

            if args.verbose {
                println!("uid: {uid}, gid: {gid}");
            }

            let mut child = process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command.join(" "))
                .stdout(process::Stdio::inherit())
                .stderr(process::Stdio::inherit())
                .uid(uid)
                .gid(gid)
                .spawn()
                .expect("Failed to execute command");

            let code = child
                .wait()
                .expect("Command wasn't running")
                .code()
                .unwrap_or(0);
            exit_code.store(code, Ordering::Relaxed);
        }
        WaitMode::Timeout | WaitMode::Pid | WaitMode::TimeoutOrPid => {
            let mut duration = chrono::Duration::try_seconds(0).unwrap();
            let mut end_time = chrono::Local::now();

            let timeout = args.timeout.is_some();
            let waitfor = args.waitfor.is_some();
            if timeout {
                duration = parsed_timeout.expect("Timeout should be present");
                end_time += duration;
                sleep_str += &format!(
                    "for {}",
                    duration_parser::format_duration_human(duration)
                );
            }

            print!("{sleep_str}");

            if timeout && waitfor {
                print!(" or ");
            }
            if waitfor {
                print!(
                    "until PID {} finishes",
                    args.waitfor.expect("PID should be present")
                );
            }
            println!(".");

            const SHORT_FMT: &str = "at %-I:%M:%S %p";
            const LONG_FMT: &str = "on %B %-d at %-I:%M:%S %p";

            if timeout {
                println!(
                    "Resuming {}.",
                    if duration.num_seconds() > (60 * 60 * 24) {
                        end_time.format(LONG_FMT)
                    } else {
                        end_time.format(SHORT_FMT)
                    }
                );
                if !waitfor {
                    thread::sleep(duration.to_std().expect("Duration should be valid"));
                }
            }

            if waitfor {
                let pid = args.waitfor.expect("PID should be present");

                let timeout_duration = if timeout {
                    Some(duration.to_std().expect("Duration should be valid"))
                } else {
                    None
                };

                match wait_for_pid(pid, timeout_duration, args.verbose) {
                    Ok(WaitForPidResult::Exited(pid_exit_code)) => {
                        exit_code.store(pid_exit_code, Ordering::Relaxed);

                        print!("PID {pid} finished ");
                        let now = chrono::Local::now();
                        print!("{} ", now.format(SHORT_FMT));
                        println!(
                            "with exit code {}",
                            exit_code.load(Ordering::Relaxed)
                        );
                    }
                    Ok(WaitForPidResult::TimedOut) => {}
                    Err(WaitForPidError::NotFound) => {
                        println!("PID {pid} not found");
                        // Release holds before exiting: process::exit skips
                        // destructors, and an entirely-mode hold would leave
                        // system sleep disabled (until the helper reaps it,
                        // or indefinitely with the root CLI fallback).
                        if let Ok(mut guard) = active.lock() {
                            let _ = guard.take();
                        }
                        process::exit(1);
                    }
                    Err(WaitForPidError::Kevent(e)) => {
                        eprintln!("kevent error waiting for PID {pid}: {e}");
                        if let Ok(mut guard) = active.lock() {
                            let _ = guard.take();
                        }
                        process::exit(1);
                    }
                }
            }
        }
        WaitMode::UntilInterrupt => {
            sleep_str += "until Ctrl+C pressed.";
            println!("{sleep_str}");
            // park() may return spuriously; loop so only the SIGINT handler
            // (which exits the process) can end the session.
            loop {
                thread::park();
            }
        }
    }

    if let Ok(mut guard) = active.lock() {
        let _ = guard.take();
    }
    process::exit(exit_code.load(Ordering::Relaxed));
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("caffeinate2 only supports macOS.");
    std::process::exit(1);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::cli::parse_args;
    use sleep_mode::{SleepMode, SleepModeSet};

    #[test]
    fn defaults_to_system_assertion_when_no_assertion_flags_are_set() {
        let mut sleep_modes = parse_args(&["caffeinate2"]).sleep_modes();
        sleep_modes.apply_defaults();
        assert!(sleep_modes.contains(SleepMode::System));
        assert_eq!(sleep_modes.selected_labels(), vec!["System"]);
    }

    #[test]
    fn explicit_assertion_flags_do_not_add_default_system_assertion() {
        let sleep_modes = parse_args(&["caffeinate2", "--display", "--user-active"]).sleep_modes();
        assert!(sleep_modes.contains(SleepMode::Display));
        assert!(sleep_modes.contains(SleepMode::UserActive));
        assert!(!sleep_modes.contains(SleepMode::System));
        assert_eq!(
            sleep_modes.selected_labels(),
            vec!["Display", "User active"]
        );
    }

    #[test]
    fn dry_run_creates_no_assertions() {
        let mut sleep_modes = SleepModeSet::default();
        for mode in SleepMode::all() {
            sleep_modes.insert(mode);
        }
        let active = sleep_modes.enable_all(false, true).unwrap();
        assert!(active.is_empty());
    }
}
