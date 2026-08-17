#[cfg(target_os = "macos")]
mod cli;

#[cfg(target_os = "macos")]
use caffeinate2::entirely::{authz, helper_ipc, install};
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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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

/// Shared state coordinating the command spawner with the signal thread, so a
/// signal can never orphan a command mid-launch. The handshake: the spawner
/// holds `gate` across store(SPAWN_IN_FLIGHT) → spawn() → store(real pid or
/// 0), and refuses to spawn at all once `shutting_down` is set; the signal
/// thread sets that flag first and then acquires the gate (bounded try_lock),
/// so the pid it reads is settled — never mid-spawn. Residual window,
/// accepted: if spawn() itself hangs past the signal thread's 100 ms bound,
/// the pid still reads as SPAWN_IN_FLIGHT and no SIGTERM is forwarded.
#[cfg(target_os = "macos")]
struct SpawnSync {
    child_pid: AtomicI32,
    gate: Mutex<()>,
    shutting_down: AtomicBool,
}

#[cfg(target_os = "macos")]
impl SpawnSync {
    /// `child_pid` value marking a `spawn()` in flight.
    const SPAWN_IN_FLIGHT: i32 = -1;

    fn new() -> Self {
        Self {
            child_pid: AtomicI32::new(0),
            gate: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
        }
    }
}

/// Credentials to apply to a spawned command. `groups` is `Some` only when
/// `--drop-root` is in effect: the child's supplementary group list must be
/// reset to the target user's own groups so it doesn't inherit root's (e.g.
/// `wheel`, `admin`). `None` means leave the inherited group list alone.
/// `login` (also `--drop-root`-only) is the target user's passwd identity,
/// used to reset the child's login environment.
#[cfg(target_os = "macos")]
struct CommandCredentials {
    uid: u32,
    gid: u32,
    groups: Option<Vec<u32>>,
    login: Option<authz::User>,
}

/// Resolve the credentials for the wrapped command. Runs in `main` before any
/// sleep prevention is enabled, so every error path here can plain-exit.
#[cfg(target_os = "macos")]
fn command_credentials(args: &Args) -> CommandCredentials {
    if args.drop_root {
        // Nothing to drop when not root: `setgroups` is root-only on macOS
        // (setuid/setgid to one's own ids would succeed), so the pre_exec
        // hook made every non-root --drop-root run die at its first call with
        // EPERM before the command started — and running without sudo is the
        // natural invocation now that the helper removes the need for it.
        // Warn and run with the credentials already in force.
        if !unistd::geteuid().is_root() {
            eprintln!("Warning: --drop-root: not running as root, nothing to drop");
            return CommandCredentials {
                uid: unistd::getuid().into(),
                gid: unistd::getgid().into(),
                groups: None,
                login: None,
            };
        }
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
            process::exit(1);
        }
        let uid_str = sudo_uid
            .clone()
            .unwrap_or_else(|| unistd::getuid().to_string());
        let gid_str = sudo_gid
            .clone()
            .unwrap_or_else(|| unistd::getgid().to_string());
        let Ok(uid) = uid_str.parse::<u32>() else {
            eprintln!("Error: invalid SUDO_UID: {uid_str}");
            process::exit(1);
        };
        let Ok(gid) = gid_str.parse::<u32>() else {
            eprintln!("Error: invalid SUDO_GID: {gid_str}");
            process::exit(1);
        };
        // With neither SUDO_UID nor SUDO_GID set we fell back to the current
        // ids; if those are root, --drop-root would silently run the command as
        // root anyway (invoked as root directly, not via sudo). Refuse rather
        // than defeat the flag the user explicitly asked for.
        if sudo_uid.is_none() && sudo_gid.is_none() && uid == 0 {
            eprintln!(
                "Error: --drop-root requires running via sudo; SUDO_UID/SUDO_GID are unset and the process is root, so privileges cannot be dropped"
            );
            process::exit(1);
        }
        // Resolve via SUDO_USER when present; otherwise fall back to just the
        // primary group so the child still sheds root's supplementary groups.
        let groups = std::env::var("SUDO_USER")
            .ok()
            .and_then(|user| authz::group_ids_for_user(&user, gid))
            .unwrap_or_else(|| vec![gid]);
        let groups = cap_groups_for_setgroups(groups, gid);
        // The child's environment must match the identity it runs as: sudo
        // resets USER, LOGNAME, and SHELL to root's, and HOME too under
        // `sudo -H`/`sudo -i` or a sudoers without HOME in env_keep — so
        // anything reading per-user config (git, ssh, cargo, ...) silently
        // misbehaves without this. SUDO_* and PATH are deliberately left
        // alone, matching sudo itself.
        let login = authz::user_for_uid(uid);
        if login.is_none() {
            eprintln!(
                "Warning: no passwd entry for uid {uid}; the command keeps the current HOME/USER/LOGNAME/SHELL"
            );
        }
        CommandCredentials {
            uid,
            gid,
            groups: Some(groups),
            login,
        }
    } else {
        CommandCredentials {
            uid: unistd::getuid().into(),
            gid: unistd::getgid().into(),
            groups: None,
            login: None,
        }
    }
}

#[cfg(target_os = "macos")]
fn run_command_mode(
    args: &Args,
    sleep_str: &str,
    credentials: CommandCredentials,
    active: &Arc<Mutex<Option<sleep_mode::ActiveSession>>>,
    sync: &SpawnSync,
    exit_code: &Arc<AtomicI32>,
) {
    // `WaitMode::Command` is produced only for a non-empty trailing command
    // (see `wait_mode`), so presence and non-emptiness both hold here.
    let command = args.command.as_ref().expect("Command should be present");
    println!("{sleep_str}until command finishes.");

    let CommandCredentials {
        uid,
        gid,
        groups,
        login,
    } = credentials;
    tracing::debug!("uid: {uid}, gid: {gid}");

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

    // --drop-root: point HOME/USER/LOGNAME/SHELL at the target user (see
    // command_credentials; only these four — PATH, cwd, and umask are left
    // alone). Empty fields mean the passwd record omitted them; an empty HOME
    // would break more than an inherited one, so it is skipped, while an
    // empty shell gets the POSIX default.
    if let Some(user) = login {
        if !user.home.is_empty() {
            child_command.env("HOME", &user.home);
        }
        child_command.env("USER", &user.name);
        child_command.env("LOGNAME", &user.name);
        let shell = if user.shell.is_empty() {
            "/bin/sh"
        } else {
            user.shell.as_str()
        };
        child_command.env("SHELL", shell);
    }

    let spawned = {
        let _gate = sync
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The signal thread sets the flag before taking this gate: seen here,
        // the process is already tearing down its hold, and a command started
        // now would be orphaned the instant it exits. Park instead — the
        // signal thread owns the exit.
        if sync.shutting_down.load(Ordering::Relaxed) {
            drop(_gate);
            loop {
                thread::park();
            }
        }
        sync.child_pid
            .store(SpawnSync::SPAWN_IN_FLIGHT, Ordering::Relaxed);
        let spawned = child_command.spawn();
        sync.child_pid.store(
            spawned.as_ref().map_or(0, |child| child.id().cast_signed()),
            Ordering::Relaxed,
        );
        spawned
    };
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Error: failed to execute command: {e}");
            // 127 is the shell's "command not found"; everything else (a
            // pre_exec privilege failure, a permission error) is "found but
            // cannot execute" (126) — don't mislabel those as a missing
            // program.
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            release_active_and_exit(active, code);
        }
    };

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
    sync.child_pid.store(0, Ordering::Relaxed);
    // Match the -w decoding: report signal deaths as 128 + signal number
    // instead of masking them as success.
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(0);
    exit_code.store(code, Ordering::Relaxed);
}

/// Cap a supplementary group list for `setgroups`, keeping the primary gid
/// first.
///
/// The kernel rejects `setgroups` with more than `NGROUPS_MAX` (16) entries
/// outright (EINVAL) — and `getgrouplist` follows directory-services
/// membership, so a stock admin account resolves to 16 groups before the
/// `caffeinate2` grant group pushes it to 17. Truncating matches what
/// `initgroups` does for every real login session; membership beyond the
/// kernel list is still honored through the directory service, so the dropped
/// tail only affects the rare syscall reading the process group list directly.
/// The primary gid leads because it must survive the cut: it determines the
/// group ownership of files the child creates.
#[cfg(target_os = "macos")]
fn cap_groups_for_setgroups(groups: Vec<u32>, primary_gid: u32) -> Vec<u32> {
    let mut capped = Vec::with_capacity(groups.len() + 1);
    capped.push(primary_gid);
    capped.extend(groups.into_iter().filter(|&gid| gid != primary_gid));
    capped.truncate(ngroups_max());
    capped
}

/// The kernel's `NGROUPS_MAX` (16 on macOS), from sysconf; libc exports no
/// constant for it on Apple targets.
#[cfg(target_os = "macos")]
fn ngroups_max() -> usize {
    // SAFETY: sysconf with a valid name constant has no other preconditions.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_NGROUPS_MAX) }).unwrap_or(16)
}

/// Timeout to hand [`wait_for_pid`] when `-w` is combined with `-t`.
///
/// `None` means "wait indefinitely for the PID" (`-w` with no `-t`). `Some(dur)`
/// bounds the wait. A non-positive `-t` is rejected before this point, so a
/// bounded wait always carries a positive duration.
#[cfg(target_os = "macos")]
fn waitfor_timeout(
    timeout_present: bool,
    duration: jiff::SignedDuration,
) -> Option<jiff::SignedDuration> {
    timeout_present.then_some(duration)
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
        // main() rejects non-positive timeouts and timeouts that overflow the
        // timestamp range before any sleep prevention is enabled, so this add
        // cannot fail in practice; the fallback leaves `end_time` at "now" —
        // a zero wait — rather than unwinding with an active hold. (Not a
        // saturating wait: jiff refuses, it doesn't clamp.)
        if let Ok(next) = end_time.checked_add(duration) {
            end_time = next;
        }
        let _ = write!(
            sleep_str,
            "for {}",
            duration_parser::format_duration_human(duration)
        );
    }

    print!("{sleep_str}");

    // With `-t` set the "for <duration>" prefix is always present (a
    // non-positive timeout is rejected in main()), so the " or " separator is
    // needed exactly when a timeout and a PID are both awaited.
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

    if timeout && !waitfor {
        println!(
            "Resuming {}.",
            if duration.as_secs() >= (60 * 60 * 24) {
                end_time.strftime(LONG_TIME_FMT)
            } else {
                end_time.strftime(SHORT_TIME_FMT)
            }
        );
        // The printed resume time is wall-clock, but nanosleep (thread::sleep)
        // does not advance while the system sleeps — one long sleep would
        // overshoot the promise by however long the machine slept mid-window.
        // Re-arm against the absolute end time in bounded chunks instead: a
        // chunk suspended by system sleep costs at most one chunk of overshoot
        // before the loop re-reads the clock. `TryFrom<SignedDuration>` fails
        // only for negative durations (the remaining time is checked positive
        // first), so the conversion is infallible here.
        const RESUME_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);
        loop {
            let remaining = jiff::Zoned::now().duration_until(&end_time);
            if remaining <= jiff::SignedDuration::ZERO {
                break;
            }
            let remaining = std::time::Duration::try_from(remaining).unwrap_or_default();
            thread::sleep(remaining.min(RESUME_RECHECK));
        }
    }

    if !waitfor {
        return;
    }

    let pid = args.waitfor.expect("PID should be present");
    // Infallible for the positive duration main() guaranteed; a failure would
    // degrade to a zero wait (an instant "Timeout reached"), not an unwind.
    //
    // Time-base caveat: this hands a *relative* timeout to kevent, which (like
    // nanosleep) does not advance across system sleep — so `-t X -w PID`
    // bounds awake time, while `-t X` alone honors the wall clock above.
    // Nothing printed for the combined mode promises a wall-clock deadline,
    // so the difference is deliberate rather than corrected here.
    let timeout_duration = waitfor_timeout(timeout, duration)
        .map(|d| std::time::Duration::try_from(d).unwrap_or_default());

    match wait_for_pid(pid, timeout_duration) {
        Ok(WaitForPidResult::Exited(pid_exit_code)) => {
            exit_code.store(pid_exit_code, Ordering::Relaxed);

            print!("PID {pid} finished ");
            let now = jiff::Zoned::now();
            print!("{} ", now.strftime(SHORT_TIME_FMT));
            println!("with exit code {}", exit_code.load(Ordering::Relaxed));
        }
        Ok(WaitForPidResult::ExitedStatusUnknown) => {
            // Exit code stays 0: the status of another user's process cannot
            // be read (see `wait_for_pid`), and inventing a failure code for
            // an exit we know nothing about would be worse.
            print!("PID {pid} finished ");
            let now = jiff::Zoned::now();
            print!("{} ", now.strftime(SHORT_TIME_FMT));
            println!("(exit code unavailable for another user's process).");
        }
        Ok(WaitForPidResult::TimedOut) => {
            if timeout {
                let now = jiff::Zoned::now();
                println!("Timeout reached {}.", now.strftime(SHORT_TIME_FMT));
            }
        }
        Err(error) => {
            match error {
                // Unreachable from the CLI: clap's `parse_positive_pid`
                // rejects non-positive PIDs. `wait_for_pid` keeps the guard
                // for its direct callers.
                WaitForPidError::InvalidPid => {
                    eprintln!("Error: invalid PID {pid}; expected a positive process ID");
                }
                WaitForPidError::NotFound => eprintln!("Error: PID {pid} not found"),
                WaitForPidError::Kevent(e) => {
                    eprintln!("kevent error waiting for PID {pid}: {e}");
                }
            }
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
                Ok(status) => {
                    println!("Helper: running");
                    println!("Entirely-mode holders: {}", status.holders);
                    println!(
                        "System sleep disabled by helper: {}",
                        if status.sleep_disabled { "yes" } else { "no" }
                    );
                    // The installed helper is a copied snapshot that keeps
                    // serving its own version until reinstalled, so a version
                    // skew is the user's cue to realign the two. Point at
                    // whichever side is behind: reinstalling the helper when it
                    // is older, but updating this binary when the helper is
                    // newer (a reinstall would otherwise downgrade the helper).
                    if status.is_stale() {
                        let advice = if status.helper_is_newer() {
                            "update this caffeinate2 binary to match"
                        } else {
                            "update with: sudo caffeinate2 --install-helper"
                        };
                        println!(
                            "Helper version: {} (this binary is {}; {advice})",
                            status.version,
                            helper_ipc::HelperStatus::BINARY_VERSION
                        );
                    } else {
                        println!(
                            "Helper version: {}",
                            helper_ipc::HelperStatus::BINARY_VERSION
                        );
                    }
                }
                Err(e) if e.kind() == caffeinate2::entirely::error::HelperIpcErrorKind::Connect => {
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
    // Parse first so tracing verbosity comes from clap's own `-v`/`--verbose`
    // value: this handles bundled short flags (`-vt 3600`, `-vi`) and ignores a
    // `-v` that belongs to a wrapped command after `--`. clap parse errors print
    // to stderr and exit on their own (they don't use tracing). The exclusive
    // `--install-helper-internal` / `--status` paths still get tracing
    // initialized here, before `run_maintenance` does any real work.
    let args = Args::parse();
    caffeinate2::util::logging::init_cli_tracing(args.verbose);
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

    let mode = wait_mode(&args);
    if matches!(mode, WaitMode::Command) && (args.timeout.is_some() || args.waitfor.is_some()) {
        eprintln!("Warning: trailing command takes priority over --timeout and --waitfor");
    }

    let mut sleep_modes = args.sleep_modes();
    sleep_modes.apply_defaults();

    tracing::debug!("{args:#?}");

    let mut sleep_str = format!(
        "Preventing sleep types: [{}] ",
        sleep_modes.selected_labels().join(", ")
    );

    // Parse the timeout before enabling any sleep prevention: a bad -t value
    // must not toggle the system sleep setting and then exit without
    // releasing it (process::exit skips destructors, and the entirely-mode
    // hold has system-wide effects beyond this process's lifetime). The value
    // is validated in every mode, including Command mode where the trailing
    // command takes priority (as warned above) and the timeout is never
    // consumed: a `-t` that does not name a valid positive duration is an
    // error, not something to silently ignore.
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

    // A non-positive timeout is always an error, in every mode: `-t 0 -w PID`
    // and `-t 0 -- command` error like a bare `-t 0`.
    if parsed_timeout
        .as_ref()
        .is_some_and(|d| *d <= jiff::SignedDuration::ZERO)
    {
        eprintln!("Error: timeout must be positive");
        process::exit(1);
    }

    // A timeout can parse fine (the parser only bounds it to SignedDuration)
    // yet overflow `Zoned::now() + duration`, whose timestamp range is far
    // smaller — and that add would fail only after sleep prevention was
    // enabled. Reject it here while nothing has been toggled yet.
    if parsed_timeout
        .as_ref()
        .is_some_and(timeout_overflows_datetime)
    {
        eprintln!("Error: timeout is too far in the future");
        process::exit(1);
    }

    // Resolve --drop-root credentials under the same rule as the timeout
    // validation above: a tampered SUDO_UID/SUDO_GID must error out before any
    // sleep prevention is enabled, not after.
    let credentials = matches!(mode, WaitMode::Command).then(|| command_credentials(&args));

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
    let spawn_sync = Arc::new(SpawnSync::new());
    let spawn_sync_signal = Arc::clone(&spawn_sync);

    // Also catch SIGTERM/SIGHUP (kill, logout): exiting without releasing an
    // entirely-mode hold would leave system sleep disabled until the helper
    // reaps the dead process (or indefinitely with the root CLI fallback).
    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("Failed to create signal iterator");
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            println!("\nStopping...");
            // Refuse future spawns first, then serialize with any spawn in
            // flight (see SpawnSync): once the gate is acquired the pid is
            // settled — real, or 0 — never mid-spawn. The try_lock bound
            // keeps shutdown prompt if spawn() itself hangs; in that one case
            // the pid may still read as SPAWN_IN_FLIGHT and no kill is sent.
            spawn_sync_signal
                .shutting_down
                .store(true, Ordering::Relaxed);
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            let pid = loop {
                if spawn_sync_signal.gate.try_lock().is_ok()
                    || std::time::Instant::now() >= deadline
                {
                    break spawn_sync_signal.child_pid.load(Ordering::Relaxed);
                }
                thread::sleep(std::time::Duration::from_millis(5));
            };
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

    match mode {
        WaitMode::Command => {
            let credentials = credentials.expect("resolved above for Command mode");
            run_command_mode(
                &args,
                &sleep_str,
                credentials,
                &active,
                &spawn_sync,
                &exit_code,
            );
        }
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

/// True when adding `duration` to the current local time would leave jiff's
/// representable `Zoned` range (which ends at year 9999 — far below what
/// `parse_duration` accepts). Checked before enabling sleep prevention so the
/// later display math can never fail while a hold is active.
#[cfg(target_os = "macos")]
fn timeout_overflows_datetime(duration: &jiff::SignedDuration) -> bool {
    jiff::Zoned::now().checked_add(*duration).is_err()
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
    fn huge_parseable_timeout_is_rejected_before_enabling() {
        // ~3.2 million years: parses (well under SignedDuration's cap) but
        // overflows Zoned::now() + duration. Must be caught by the pre-enable
        // validation instead of failing mid-session.
        let huge = duration_parser::parse_duration("100000000000000").unwrap();
        assert!(crate::timeout_overflows_datetime(&huge));
        let sane = duration_parser::parse_duration("1h").unwrap();
        assert!(!crate::timeout_overflows_datetime(&sane));
    }

    #[test]
    fn timeout_parses_raw_number_as_seconds() {
        assert_eq!(
            duration_parser::parse_duration("3600").unwrap().as_secs(),
            3600
        );
        assert_eq!(
            duration_parser::parse_duration("45323").unwrap().as_secs(),
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
                .as_secs(),
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
                .as_secs(),
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
    fn setgroups_cap_keeps_primary_first_and_at_most_ngroups_max() {
        use crate::cap_groups_for_setgroups;
        let max = crate::ngroups_max();
        assert_eq!(max, 16, "macOS pins NGROUPS_MAX at 16");

        // More groups than the kernel accepts, primary buried in the middle:
        // the cap leads with the primary, dedupes it, and cuts at the limit.
        let many: Vec<u32> = (1..=24).collect();
        let capped = cap_groups_for_setgroups(many.clone(), 12);
        assert_eq!(capped.len(), max);
        assert_eq!(capped[0], 12);
        assert_eq!(capped.iter().filter(|&&gid| gid == 12).count(), 1);
        assert!(capped.iter().all(|gid| many.contains(gid)));

        // A primary missing from the resolved list is still prepended.
        let capped = cap_groups_for_setgroups(vec![7, 8], 20);
        assert_eq!(capped, vec![20, 7, 8]);

        // A short list is passed through (reordered to primary-first only).
        let capped = cap_groups_for_setgroups(vec![5, 20, 7], 20);
        assert_eq!(capped, vec![20, 5, 7]);
    }

    #[test]
    fn waitfor_timeout_bounds_only_when_timeout_present() {
        use crate::waitfor_timeout;
        let thirty = jiff::SignedDuration::from_secs(30);
        // `-w PID` with no `-t`: wait indefinitely for the PID.
        assert_eq!(waitfor_timeout(false, thirty), None);
        // `-t 30s -w PID`: bounded wait. A non-positive `-t` is rejected before
        // this point, so waitfor_timeout only ever bounds with a positive value.
        assert_eq!(waitfor_timeout(true, thirty), Some(thirty));
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
        // Classification only: main validates `-t` in every mode, so the zero
        // timeout is rejected before this Command classification is acted on.
        let args = parse_args(&["caffeinate2", "-t", "0", "--", "script"]);
        assert_eq!(wait_mode(&args), WaitMode::Command);
    }

    #[test]
    fn invalid_timeout_with_trailing_command_is_command_mode() {
        use crate::cli::wait::wait_mode;
        // Classification only: main validates `-t` in every mode, so the
        // malformed timeout is rejected before the command would run.
        let args = parse_args(&["caffeinate2", "-t", "notaduration", "--", "echo", "hi"]);
        assert_eq!(wait_mode(&args), WaitMode::Command);
    }

    #[test]
    fn reject_non_positive_waitfor_at_parse() {
        assert!(Args::try_parse_from(["caffeinate2", "-w", "0"]).is_err());
        assert!(Args::try_parse_from(["caffeinate2", "-w", "-1"]).is_err());
    }
}
