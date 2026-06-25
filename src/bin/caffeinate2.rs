#[cfg(target_os = "macos")]
mod cli;

#[cfg(target_os = "macos")]
use caffeinate2::entirely::{helper_ipc, install};
#[cfg(target_os = "macos")]
use caffeinate2::sleep::sleep_mode;
#[cfg(target_os = "macos")]
use caffeinate2::util::duration_parser;
#[cfg(target_os = "macos")]
use clap::Parser;
#[cfg(target_os = "macos")]
use cli::wait::{
    WaitForPidError, WaitForPidResult, WaitMode, misquoted_duration_error, wait_for_pid, wait_mode,
};
#[cfg(target_os = "macos")]
use cli::{Args, MaintenanceCommand};
#[cfg(target_os = "macos")]
use nix::sys::signal::{Signal, kill};
#[cfg(target_os = "macos")]
use nix::unistd;
#[cfg(target_os = "macos")]
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};
#[cfg(target_os = "macos")]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(target_os = "macos")]
use std::process;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicI32, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::thread;

#[cfg(target_os = "macos")]
const SHORT_TIME_FMT: &str = "at %-I:%M:%S %p";
#[cfg(target_os = "macos")]
const LONG_TIME_FMT: &str = "on %B %-d at %-I:%M:%S %p";

#[cfg(target_os = "macos")]
fn release_active_and_exit(active: &Arc<Mutex<Option<sleep_mode::ActiveSession>>>, code: i32) -> ! {
    let mut guard = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = guard.take();
    process::exit(code);
}

/// Credentials to apply to a spawned command. `groups` is `Some` only when
/// `--drop-root` is in effect: the child's supplementary group list must be
/// reset to the target user's own groups so it doesn't inherit root's (e.g.
/// `wheel`, `admin`). `None` means leave the inherited group list alone.
#[cfg(target_os = "macos")]
struct CommandCredentials {
    uid: u32,
    gid: u32,
    groups: Option<Vec<u32>>,
}

/// Resolve the target user's full supplementary group list (including the
/// primary `gid`). Returns `None` if the lookup fails, so the caller can fall
/// back to a conservative single-group list rather than leaking root's groups.
#[cfg(target_os = "macos")]
fn supplementary_groups_for(user: &str, gid: u32) -> Option<Vec<u32>> {
    let cname = std::ffi::CString::new(user).ok()?;
    // macOS `getgrouplist` takes/returns `int` groups (not `gid_t`); convert to
    // `gid_t` (u32) for `setgroups`. NGROUPS_MAX is 16, but query with a larger
    // buffer and retry once if the kernel reports it needs more.
    let mut ngroups: libc::c_int = 64;
    let mut buf: Vec<libc::c_int> = vec![0; ngroups as usize];
    let mut rc = unsafe {
        libc::getgrouplist(
            cname.as_ptr(),
            gid as libc::c_int,
            buf.as_mut_ptr(),
            &mut ngroups,
        )
    };
    if rc < 0 {
        // ngroups now holds the required size.
        let needed = usize::try_from(ngroups).ok()?.max(1);
        buf = vec![0; needed];
        rc = unsafe {
            libc::getgrouplist(
                cname.as_ptr(),
                gid as libc::c_int,
                buf.as_mut_ptr(),
                &mut ngroups,
            )
        };
        if rc < 0 {
            return None;
        }
    }
    let count = usize::try_from(ngroups).ok()?.min(buf.len());
    buf.truncate(count);
    Some(buf.into_iter().map(|g| g as u32).collect())
}

#[cfg(target_os = "macos")]
fn command_credentials(
    args: &Args,
    active: &Arc<Mutex<Option<sleep_mode::ActiveSession>>>,
) -> CommandCredentials {
    if args.drop_root {
        let sudo_uid = std::env::var("SUDO_UID").ok();
        let sudo_gid = std::env::var("SUDO_GID").ok();
        // sudo always exports SUDO_UID and SUDO_GID together. Exactly one being
        // set is a tampered or partial environment: falling back to the current
        // (root) id for the missing half would drop privileges only halfway,
        // running the child as root's uid or gid. Refuse rather than half-drop.
        if sudo_uid.is_some() != sudo_gid.is_some() {
            eprintln!(
                "Error: --drop-root requires SUDO_UID and SUDO_GID to be set together; only one is present"
            );
            release_active_and_exit(active, 1);
        }
        let uid_str = sudo_uid
            .clone()
            .unwrap_or_else(|| unistd::getuid().to_string());
        let gid_str = sudo_gid
            .clone()
            .unwrap_or_else(|| unistd::getgid().to_string());
        let Ok(uid) = uid_str.parse::<u32>() else {
            eprintln!("Error: invalid SUDO_UID: {uid_str}");
            release_active_and_exit(active, 1);
        };
        let Ok(gid) = gid_str.parse::<u32>() else {
            eprintln!("Error: invalid SUDO_GID: {gid_str}");
            release_active_and_exit(active, 1);
        };
        // With neither SUDO_UID nor SUDO_GID set we fell back to the current
        // ids; if those are root, --drop-root would silently run the command as
        // root anyway (invoked as root directly, not via sudo). Refuse rather
        // than defeat the flag the user explicitly asked for.
        if sudo_uid.is_none() && sudo_gid.is_none() && uid == 0 {
            eprintln!(
                "Error: --drop-root requires running via sudo; SUDO_UID/SUDO_GID are unset and the process is root, so privileges cannot be dropped"
            );
            release_active_and_exit(active, 1);
        }
        // Resolve via SUDO_USER when present; otherwise fall back to just the
        // primary group so the child still sheds root's supplementary groups.
        let groups = std::env::var("SUDO_USER")
            .ok()
            .and_then(|user| supplementary_groups_for(&user, gid))
            .unwrap_or_else(|| vec![gid]);
        CommandCredentials {
            uid,
            gid,
            groups: Some(groups),
        }
    } else {
        CommandCredentials {
            uid: unistd::getuid().into(),
            gid: unistd::getgid().into(),
            groups: None,
        }
    }
}

#[cfg(target_os = "macos")]
fn run_command_mode(
    args: &Args,
    sleep_str: &str,
    active: &Arc<Mutex<Option<sleep_mode::ActiveSession>>>,
    child_pid: &Arc<AtomicI32>,
    exit_code: &Arc<AtomicI32>,
) {
    let command = args.command.as_ref().expect("Command should be present");
    if command.is_empty() {
        eprintln!("Error: empty command");
        release_active_and_exit(active, 2);
    }
    println!("{sleep_str}until command finishes.");

    let CommandCredentials { uid, gid, groups } = command_credentials(args, active);
    if args.verbose {
        println!("uid: {uid}, gid: {gid}");
    }

    let mut child_command = if args.shell {
        let mut child_command = process::Command::new("/bin/sh");
        child_command.arg("-c").arg(command.join(" "));
        child_command
    } else {
        let mut child_command = process::Command::new(&command[0]);
        child_command.args(&command[1..]);
        child_command
    };
    child_command
        .stdout(process::Stdio::inherit())
        .stderr(process::Stdio::inherit());

    match groups {
        // --drop-root: reset the supplementary group list, then gid, then uid,
        // in that order. std applies `.uid()`/`.gid()` *before* any `pre_exec`
        // hook, so doing it through `.uid()/.gid()` alone would leave root's
        // supplementary groups on the child; and `setgroups`/`setgid` must run
        // while still privileged (before `setuid`). Do all three in `pre_exec`
        // (the group list is resolved in the parent, since `getgrouplist` is not
        // async-signal-safe).
        Some(groups) => unsafe {
            child_command.pre_exec(move || {
                let ngroups = libc::c_int::try_from(groups.len()).unwrap_or(libc::c_int::MAX);
                if libc::setgroups(ngroups, groups.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        },
        None => {
            child_command.uid(uid).gid(gid);
        }
    }

    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Error: failed to execute command: {e}");
            release_active_and_exit(active, 127);
        }
    };
    child_pid.store(child.id().cast_signed(), Ordering::Relaxed);

    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("Error: command wait failed: {e}");
            release_active_and_exit(active, 1);
        }
    };
    // Clear the forwarded-signal target the instant wait() returns. There is a
    // tiny residual window between the kernel reaping the child (making its PID
    // eligible for reuse) and this store: a signal arriving in that window would
    // forward SIGTERM to whatever process now holds the recycled PID. We accept
    // it — the window is microscopic, the signal handler only forwards a
    // terminating signal the user is already sending, and it is the same
    // PID-reuse limitation that `-w` carries (see `wait_for_pid`).
    child_pid.store(0, Ordering::Relaxed);
    // Match the -w decoding: report signal deaths as 128 + signal number
    // instead of masking them as success.
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(0);
    exit_code.store(code, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn run_timed_wait_mode(
    args: &Args,
    parsed_timeout: Option<&jiff::SignedDuration>,
    sleep_str: &mut String,
    active: &Arc<Mutex<Option<sleep_mode::ActiveSession>>>,
    exit_code: &Arc<AtomicI32>,
) {
    use std::fmt::Write as _;

    let mut duration = jiff::SignedDuration::ZERO;
    let mut end_time = jiff::Zoned::now();

    let timeout = args.timeout.is_some();
    let waitfor = args.waitfor.is_some();
    if timeout {
        duration = parsed_timeout.copied().expect("Timeout should be present");
        if duration <= jiff::SignedDuration::ZERO && !waitfor {
            eprintln!("Error: timeout must be positive");
            release_active_and_exit(active, 1);
        }
        if duration > jiff::SignedDuration::ZERO {
            end_time += duration;
            let _ = write!(
                sleep_str,
                "for {}",
                duration_parser::format_duration_human(duration)
            );
        }
    }

    print!("{sleep_str}");

    // Only print the " or " separator when a "for <duration>" prefix was
    // actually emitted above (it is skipped for a zero timeout). Otherwise
    // `-t 0 -w PID` would print a dangling " or until PID ... finishes".
    let printed_timeout = timeout && duration > jiff::SignedDuration::ZERO;
    if printed_timeout && waitfor {
        print!(" or ");
    }
    if waitfor {
        print!(
            "until PID {} finishes",
            args.waitfor.expect("PID should be present")
        );
    }
    println!(".");

    if timeout {
        if !waitfor && duration > jiff::SignedDuration::ZERO {
            println!(
                "Resuming {}.",
                if duration.as_secs() > (60 * 60 * 24) {
                    end_time.strftime(LONG_TIME_FMT)
                } else {
                    end_time.strftime(SHORT_TIME_FMT)
                }
            );
        }
        if !waitfor && duration > jiff::SignedDuration::ZERO {
            let std_duration = match std::time::Duration::try_from(duration) {
                Ok(d) => d,
                Err(_) => {
                    eprintln!("Error: timeout is too large");
                    release_active_and_exit(active, 1);
                }
            };
            thread::sleep(std_duration);
        }
    }

    if !waitfor {
        return;
    }

    let pid = args.waitfor.expect("PID should be present");
    let timeout_duration = if timeout && duration > jiff::SignedDuration::ZERO {
        match std::time::Duration::try_from(duration) {
            Ok(d) => Some(d),
            Err(_) => {
                eprintln!("Error: timeout is too large");
                release_active_and_exit(active, 1);
            }
        }
    } else {
        None
    };

    match wait_for_pid(pid, timeout_duration, args.verbose) {
        Ok(WaitForPidResult::Exited(pid_exit_code)) => {
            exit_code.store(pid_exit_code, Ordering::Relaxed);

            print!("PID {pid} finished ");
            let now = jiff::Zoned::now();
            print!("{} ", now.strftime(SHORT_TIME_FMT));
            println!("with exit code {}", exit_code.load(Ordering::Relaxed));
        }
        Ok(WaitForPidResult::TimedOut) => {
            if timeout && duration > jiff::SignedDuration::ZERO {
                let now = jiff::Zoned::now();
                println!("Timeout reached {}.", now.strftime(SHORT_TIME_FMT));
            }
        }
        Err(WaitForPidError::InvalidPid) => {
            eprintln!("Error: invalid PID {pid}; expected a positive process ID");
            release_active_and_exit(active, 1);
        }
        Err(WaitForPidError::NotFound) => {
            eprintln!("Error: PID {pid} not found");
            release_active_and_exit(active, 1);
        }
        Err(WaitForPidError::Kevent(e)) => {
            eprintln!("kevent error waiting for PID {pid}: {e}");
            release_active_and_exit(active, 1);
        }
    }
}

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
        MaintenanceCommand::Status => {
            let client = helper_ipc::HelperClient::new();
            match client.status() {
                Ok((holders, sleep_disabled)) => {
                    println!("Helper: running");
                    println!("Entirely-mode holders: {holders}");
                    println!(
                        "System sleep disabled by helper: {}",
                        if sleep_disabled { "yes" } else { "no" }
                    );
                }
                Err(e) if helper_ipc::is_connect_error(&e.to_string()) => {
                    println!(
                        "Helper: not running (install with: sudo caffeinate2 --install-helper)"
                    );
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    process::exit(1);
                }
            }
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
    caffeinate2::util::logging::init_cli_tracing();
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
            "install-helper" | "uninstall-helper" | "install-helper-internal" | "status"
        )
    {
        eprintln!("Error: '{first}' is not a command to run. Did you mean: caffeinate2 --{first}?");
        process::exit(2);
    }

    // A misquoted multi-word duration (`-t 1 hour and 30 minutes`) lands the
    // extra words in `command`. Reject it up front — before any sleep hold is
    // taken — instead of silently spawning `hour` as a command and exiting 127.
    // clap consumes the `--` end-of-options marker, so detect it from the raw
    // argv to tell a deliberate `-t 3600 -- hour` command from a misquoted
    // `-t 1 hour` duration.
    let command_explicitly_separated = std::env::args_os().any(|arg| arg == "--");
    if let Some(message) = misquoted_duration_error(&args, command_explicitly_separated) {
        eprintln!("{message}");
        process::exit(2);
    }

    if args
        .command
        .as_ref()
        .is_some_and(|command| !command.is_empty())
        && (args.timeout.is_some() || args.waitfor.is_some())
    {
        eprintln!("Warning: trailing command takes priority over --timeout and --waitfor");
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

    if parsed_timeout
        .as_ref()
        .is_some_and(|d| *d <= jiff::SignedDuration::ZERO)
        && args.waitfor.is_none()
        && args
            .command
            .as_ref()
            .is_none_or(|command| command.is_empty())
    {
        eprintln!("Error: timeout must be positive");
        process::exit(1);
    }

    let active = match sleep_modes.enable_all(args.verbose, args.dry_run) {
        Ok(active) => active,
        Err(e) => {
            eprintln!("Error: {e}");
            // Only suggest installing the helper when it is unreachable.
            if sleep_modes.contains(sleep_mode::SleepMode::Entirely)
                && matches!(e, sleep_mode::EnableError::HelperUnavailable)
            {
                eprintln!(
                    "Hint: install the privileged helper with: sudo caffeinate2 --install-helper (or run caffeinate2 itself with sudo)"
                );
            }
            process::exit(1);
        }
    };

    let active = Arc::new(Mutex::new(Some(active)));
    let active_signal = active.clone();
    let exit_code = Arc::new(AtomicI32::new(0));
    let child_pid = Arc::new(AtomicI32::new(0));
    let child_pid_signal = Arc::clone(&child_pid);

    // Also catch SIGTERM/SIGHUP (kill, logout): exiting without releasing an
    // entirely-mode hold would leave system sleep disabled until the helper
    // reaps the dead process (or indefinitely with the root CLI fallback).
    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("Failed to create signal iterator");
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            println!("\nStopping...");
            let pid = child_pid_signal.load(Ordering::Relaxed);
            if pid > 0 {
                let _ = kill(unistd::Pid::from_raw(pid), Signal::SIGTERM);
            }
            let mut guard = active_signal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = guard.take();
            process::exit(128 + signal);
        }
    });

    match wait_mode(&args) {
        WaitMode::Command => run_command_mode(&args, &sleep_str, &active, &child_pid, &exit_code),
        WaitMode::Timeout | WaitMode::Pid | WaitMode::TimeoutOrPid => {
            run_timed_wait_mode(
                &args,
                parsed_timeout.as_ref(),
                &mut sleep_str,
                &active,
                &exit_code,
            );
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

    let mut guard = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = guard.take();
    process::exit(exit_code.load(Ordering::Relaxed));
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("caffeinate2 only supports macOS.");
    std::process::exit(1);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use crate::cli::{Args, parse_args, wait::WaitMode};
    use caffeinate2::sleep::sleep_mode::{SleepMode, SleepModeSet};
    use caffeinate2::util::duration_parser;
    use clap::Parser;

    #[test]
    fn timeout_parses_raw_number_as_seconds() {
        assert_eq!(
            duration_parser::parse_duration("3600")
                .unwrap()
                .num_seconds(),
            3600
        );
        assert_eq!(
            duration_parser::parse_duration("45323")
                .unwrap()
                .num_seconds(),
            45323
        );
    }

    #[test]
    fn timeout_parses_quoted_human_duration() {
        let args = parse_args(&["caffeinate2", "-t", "1 hour and 30 minutes"]);
        assert_eq!(args.timeout.as_deref(), Some("1 hour and 30 minutes"));
        assert_eq!(
            duration_parser::parse_duration(args.timeout.as_ref().unwrap())
                .unwrap()
                .num_seconds(),
            5400
        );
    }

    #[test]
    fn timeout_with_command_separator() {
        use crate::cli::wait::wait_mode;
        let args = parse_args(&["caffeinate2", "-t", "3600", "--", "hour"]);
        assert_eq!(args.timeout.as_deref(), Some("3600"));
        assert_eq!(args.command, Some(vec!["hour".to_string()]));
        assert_eq!(wait_mode(&args), WaitMode::Command);
        assert_eq!(
            duration_parser::parse_duration(args.timeout.as_ref().unwrap())
                .unwrap()
                .num_seconds(),
            3600
        );
    }

    #[test]
    fn trailing_tokens_after_bare_timeout_become_command() {
        use crate::cli::wait::wait_mode;
        let args = parse_args(&["caffeinate2", "-t", "3600", "hour"]);
        assert_eq!(args.timeout.as_deref(), Some("3600"));
        assert_eq!(args.command, Some(vec!["hour".to_string()]));
        assert_eq!(wait_mode(&args), WaitMode::Command);
    }

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
    fn shell_flag_is_opt_in() {
        let direct = parse_args(&["caffeinate2", "touch", "a b.txt"]);
        assert!(!direct.shell);
        assert_eq!(
            direct.command,
            Some(vec!["touch".to_string(), "a b.txt".to_string()])
        );

        let shell = parse_args(&["caffeinate2", "--shell", "echo", "ok", "&&", "true"]);
        assert!(shell.shell);
        assert_eq!(
            shell.command,
            Some(vec![
                "echo".to_string(),
                "ok".to_string(),
                "&&".to_string(),
                "true".to_string()
            ])
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

    #[test]
    fn zero_timeout_with_waitfor_uses_timeout_or_pid_mode() {
        use crate::cli::wait::wait_mode;
        let args = parse_args(&["caffeinate2", "-t", "0", "-w", "123"]);
        assert_eq!(args.waitfor, Some(123));
        assert_eq!(wait_mode(&args), WaitMode::TimeoutOrPid);
    }

    #[test]
    fn zero_timeout_with_trailing_command_is_command_mode() {
        use crate::cli::wait::wait_mode;
        let args = parse_args(&["caffeinate2", "-t", "0", "--", "script"]);
        assert_eq!(wait_mode(&args), WaitMode::Command);
    }

    #[test]
    fn reject_non_positive_waitfor_at_parse() {
        assert!(Args::try_parse_from(["caffeinate2", "-w", "0"]).is_err());
        assert!(Args::try_parse_from(["caffeinate2", "-w", "-1"]).is_err());
    }
}
