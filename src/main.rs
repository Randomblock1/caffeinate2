#[cfg(target_os = "macos")]
use caffeinate2::{duration_parser, power_management, process_lock};
#[cfg(any(test, target_os = "macos"))]
use clap::Parser;
#[cfg(target_os = "macos")]
use nix::{sys::event, unistd};
#[cfg(target_os = "macos")]
use signal_hook::{consts::SIGINT, iterator::Signals};
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "macos")]
use std::process;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::thread;

#[cfg(target_os = "macos")]
struct ActiveAssertions {
    _assertions: Vec<power_management::PowerAssertion>,
    _sleep_guard: Option<process_lock::ProcessLock>,
}

#[cfg(target_os = "macos")]
fn set_assertions(args: &Args, state: bool) -> ActiveAssertions {
    if args.dry_run {
        return ActiveAssertions {
            _assertions: Vec::new(),
            _sleep_guard: None,
        };
    }

    let sleep_guard = if args.entirely {
        match process_lock::ProcessLock::new(args.verbose) {
            Ok(guard) => Some(guard),
            Err(e) => {
                eprintln!(
                    "Error: Failed to acquire process lock or disable sleep: {}",
                    e
                );
                process::exit(1);
            }
        }
    } else {
        None
    };

    let mut assertions = Vec::new();

    let mut add_assertion =
        |result: Result<power_management::PowerAssertion, u32>, name: &str| match result {
            Ok(assertion) => assertions.push(assertion),
            Err(code) => {
                eprintln!(
                    "Error: Failed to create {} assertion (code: {:X})",
                    name, code
                );
                process::exit(1);
            }
        };

    let assertions_config = [
        (
            args.display,
            power_management::AssertionType::PreventUserIdleDisplaySleep,
            "display sleep",
        ),
        (
            args.disk,
            power_management::AssertionType::PreventDiskIdle,
            "disk idle",
        ),
        (
            args.system,
            power_management::AssertionType::PreventUserIdleSystemSleep,
            "system sleep",
        ),
        (
            args.system_on_ac,
            power_management::AssertionType::PreventSystemSleep,
            "system sleep on AC",
        ),
    ];

    for (enabled, assertion_type, name) in assertions_config {
        if enabled {
            add_assertion(
                power_management::create_assertion(assertion_type, state, args.verbose),
                name,
            );
        }
    }

    if args.user_active {
        add_assertion(
            power_management::declare_user_activity(true, args.verbose),
            "user activity",
        );
    }

    if args.verbose {
        println!("Assertions created");
    }

    ActiveAssertions {
        _assertions: assertions,
        _sleep_guard: sleep_guard,
    }
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Verbose mode
    #[arg(short, long)]
    verbose: bool,

    /// Dry run. Don't actually prevent sleep.
    /// Useful for testing.
    #[arg(long)]
    dry_run: bool,

    /// Drop root privileges in command.
    /// You need root to disable sleep entirely,
    /// but some programs don't want to run as root.
    #[arg(long)]
    drop_root: bool,

    /// Disable display sleep
    #[arg(short, long)]
    display: bool,

    /// Disable disk idle sleep
    #[arg(short = 'm', long)]
    disk: bool,

    /// Disable idle system sleep. [DEFAULT]
    #[arg(short = 'i', long)]
    system: bool,

    /// Disable system sleep while not on battery
    #[arg(short, long)]
    system_on_ac: bool,

    /// Disable system sleep entirely (ignores lid closing)
    #[arg(short, long)]
    entirely: bool,

    /// Declare the user is active.
    /// If the display is off, this option turns it on and prevents it from going into idle sleep.
    #[arg(short, long)]
    user_active: bool,

    /// Wait for X seconds.
    /// Also supports time units (like "1 day 2 hours 3mins 4s").
    #[arg(short, long, name = "DURATION")]
    timeout: Option<String>,

    /// Wait for program with PID X to complete and pass its exit code.
    #[arg(short, long, name = "PID")]
    waitfor: Option<i32>,

    /// Wait for given command to complete (takes priority above timeout and pid)
    #[arg()]
    command: Option<Vec<String>>,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
enum WaitMode {
    Command,
    Timeout,
    Pid,
    TimeoutOrPid,
    UntilInterrupt,
}

#[cfg(any(test, target_os = "macos"))]
fn apply_default_assertion(args: &mut Args) {
    if !(args.display
        || args.disk
        || args.system
        || args.system_on_ac
        || args.entirely
        || args.user_active)
    {
        args.system = true;
    }
}

#[cfg(any(test, target_os = "macos"))]
fn selected_sleep_types(args: &Args) -> Vec<&'static str> {
    let mut sleep_types = Vec::new();

    if args.display {
        sleep_types.push("Display");
    }
    if args.disk {
        sleep_types.push("Disk");
    }
    if args.system {
        sleep_types.push("System");
    }
    if args.system_on_ac {
        sleep_types.push("System (if on AC)");
    }
    if args.entirely {
        sleep_types.push("Entirely");
    }
    if args.user_active {
        sleep_types.push("User active");
    }

    sleep_types
}

#[cfg(any(test, target_os = "macos"))]
fn wait_mode(args: &Args) -> WaitMode {
    if args.command.is_some() {
        WaitMode::Command
    } else {
        match (args.timeout.is_some(), args.waitfor.is_some()) {
            (true, true) => WaitMode::TimeoutOrPid,
            (true, false) => WaitMode::Timeout,
            (false, true) => WaitMode::Pid,
            (false, false) => WaitMode::UntilInterrupt,
        }
    }
}

#[cfg(any(test, target_os = "macos"))]
fn format_timeout_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds() % 60;
    let minutes = duration.num_minutes() % 60;
    let hours = duration.num_hours() % 24;
    let days = duration.num_days();
    let mut parts = Vec::new();

    if days > 0 {
        parts.push(format!("{} day{}", days, if days != 1 { "s" } else { "" }));
    }
    if hours > 0 {
        parts.push(format!(
            "{} hour{}",
            hours,
            if hours != 1 { "s" } else { "" }
        ));
    }
    if minutes > 0 {
        parts.push(format!(
            "{} minute{}",
            minutes,
            if minutes != 1 { "s" } else { "" }
        ));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!(
            "{} second{}",
            seconds,
            if seconds != 1 { "s" } else { "" }
        ));
    }

    parts.join(" ")
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
enum WaitForPidResult {
    Exited(i32),
    TimedOut,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
enum WaitForPidError {
    NotFound,
    Kevent(nix::Error),
}

#[cfg(target_os = "macos")]
fn timespec_from_duration(duration: std::time::Duration) -> libc::timespec {
    let max_seconds = <libc::time_t>::MAX as u64;
    libc::timespec {
        tv_sec: duration.as_secs().min(max_seconds) as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

#[cfg(target_os = "macos")]
fn wait_for_pid(
    pid: i32,
    timeout: Option<std::time::Duration>,
    verbose: bool,
) -> Result<WaitForPidResult, WaitForPidError> {
    let kq = event::Kqueue::new().map_err(WaitForPidError::Kevent)?;
    let kev = event::KEvent::new(
        pid as usize,
        event::EventFilter::EVFILT_PROC,
        event::EvFlags::EV_ADD
            | event::EvFlags::EV_ENABLE
            | event::EvFlags::EV_ONESHOT
            | event::EvFlags::EV_ERROR,
        event::FilterFlag::NOTE_EXITSTATUS,
        0,
        0,
    );

    let mut eventlist = [kev];
    let timeout = timeout.map(timespec_from_duration);
    let event_count = kq
        .kevent(&[kev], &mut eventlist, timeout)
        .map_err(WaitForPidError::Kevent)?;

    if event_count == 0 {
        return Ok(WaitForPidResult::TimedOut);
    }

    let event = eventlist[0];
    if verbose {
        println!("{:#?}", event);
    }

    if event.flags().contains(event::EvFlags::EV_ERROR) {
        if event.data() == nix::Error::ESRCH as isize {
            Err(WaitForPidError::NotFound)
        } else {
            Err(WaitForPidError::Kevent(nix::Error::from_raw(
                event.data() as i32
            )))
        }
    } else {
        Ok(WaitForPidResult::Exited(event.data() as i32))
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let mut args = Args::parse();
    apply_default_assertion(&mut args);

    if args.verbose {
        println!("DEBUG {:#?}", &args);
    }

    let mut sleep_str = format!(
        "Preventing sleep types: [{}] ",
        selected_sleep_types(&args).join(", ")
    );

    let assertions = Arc::new(Mutex::new(Some(set_assertions(&args, true))));
    let assertions_clone = assertions.clone();

    let mut exit_code = 0;

    let mut signals = Signals::new([SIGINT]).expect("Failed to create signal iterator");
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            println!("\nStopping...");
            if let Ok(mut guard) = assertions_clone.lock() {
                let _ = guard.take();
            }
            process::exit(exit_code);
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

            exit_code = child
                .wait()
                .expect("Command wasn't running")
                .code()
                .unwrap_or(0);
        }
        WaitMode::Timeout | WaitMode::Pid | WaitMode::TimeoutOrPid => {
            // If timeout or waitfor is used, wait appropriately

            let mut duration = chrono::Duration::try_seconds(0).unwrap();
            let mut end_time = chrono::Local::now();

            let timeout = args.timeout.is_some();
            let waitfor = args.waitfor.is_some();
            if timeout {
                // Timeout selected
                // Print how long we're waiting for
                match duration_parser::parse_duration(
                    &args.timeout.expect("Timeout should be present"),
                ) {
                    Ok(d) => duration = d,
                    Err(e) => {
                        eprintln!("{}", e);
                        process::exit(1);
                    }
                }
                end_time += duration;
                sleep_str += &format!("for {}", format_timeout_duration(duration));
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
                // Print when we're resuming
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

                let timeout = if timeout {
                    Some(duration.to_std().expect("Duration should be valid"))
                } else {
                    None
                };

                match wait_for_pid(pid, timeout, args.verbose) {
                    Ok(WaitForPidResult::Exited(pid_exit_code)) => {
                        exit_code = pid_exit_code;

                        print!("PID {pid} finished ");
                        let now = chrono::Local::now();
                        print!("{} ", now.format(SHORT_FMT));
                        println!("with exit code {}", exit_code);
                    }
                    Ok(WaitForPidResult::TimedOut) => {}
                    Err(WaitForPidError::NotFound) => {
                        println!("PID {} not found", pid);
                        process::exit(1);
                    }
                    Err(WaitForPidError::Kevent(e)) => {
                        eprintln!("kevent error waiting for PID {}: {}", pid, e);
                        process::exit(1);
                    }
                }
            }
        }
        WaitMode::UntilInterrupt => {
            // If no timer arguments are provided, disable sleep until Ctrl+C is pressed
            sleep_str += "until Ctrl+C pressed.";
            println!("{}", sleep_str);
            thread::park();
        }
    }
    if let Ok(mut guard) = assertions.lock() {
        let _ = guard.take();
    }
    process::exit(exit_code);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("caffeinate2 only supports macOS.");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse_args(args: &[&str]) -> Args {
        Args::try_parse_from(args).unwrap()
    }

    #[test]
    fn defaults_to_system_assertion_when_no_assertion_flags_are_set() {
        let mut args = parse_args(&["caffeinate2"]);

        apply_default_assertion(&mut args);

        assert!(args.system);
        assert_eq!(selected_sleep_types(&args), vec!["System"]);
    }

    #[test]
    fn explicit_assertion_flags_do_not_add_default_system_assertion() {
        let mut args = parse_args(&["caffeinate2", "--display", "--user-active"]);

        apply_default_assertion(&mut args);

        assert!(args.display);
        assert!(args.user_active);
        assert!(!args.system);
        assert_eq!(selected_sleep_types(&args), vec!["Display", "User active"]);
    }

    #[test]
    fn wait_mode_matches_cli_priority() {
        assert_eq!(
            wait_mode(&parse_args(&["caffeinate2"])),
            WaitMode::UntilInterrupt
        );
        assert_eq!(
            wait_mode(&parse_args(&["caffeinate2", "--timeout", "10m"])),
            WaitMode::Timeout
        );
        assert_eq!(
            wait_mode(&parse_args(&["caffeinate2", "--waitfor", "123"])),
            WaitMode::Pid
        );
        assert_eq!(
            wait_mode(&parse_args(&[
                "caffeinate2",
                "--timeout",
                "10m",
                "--waitfor",
                "123"
            ])),
            WaitMode::TimeoutOrPid
        );
        assert_eq!(
            wait_mode(&parse_args(&[
                "caffeinate2",
                "--timeout",
                "10m",
                "echo",
                "ok"
            ])),
            WaitMode::Command
        );
    }

    #[test]
    fn timeout_duration_format_omits_zero_components() {
        assert_eq!(
            format_timeout_duration(chrono::Duration::try_seconds(0).unwrap()),
            "0 seconds"
        );
        assert_eq!(
            format_timeout_duration(chrono::Duration::try_seconds(60).unwrap()),
            "1 minute"
        );
        assert_eq!(
            format_timeout_duration(chrono::Duration::try_seconds(3661).unwrap()),
            "1 hour 1 minute 1 second"
        );
        assert_eq!(
            format_timeout_duration(chrono::Duration::try_seconds(90_061).unwrap()),
            "1 day 1 hour 1 minute 1 second"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    #[test]
    fn test_set_assertions_dry_run() {
        let args = super::Args {
            verbose: false,
            dry_run: true,
            drop_root: false,
            display: true,
            disk: true,
            system: true,
            system_on_ac: true,
            entirely: true,
            user_active: true,
            timeout: None,
            waitfor: None,
            command: None,
        };

        let assertions = super::set_assertions(&args, true);
        assert!(assertions._assertions.is_empty());
        assert!(assertions._sleep_guard.is_none());
    }
}
