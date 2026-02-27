#![cfg(target_os = "macos")]

pub mod power_management;
pub mod process_lock;
pub mod duration_parser;

use clap::Parser;
use nix::{sys::event, unistd};
use signal_hook::{consts::SIGINT, iterator::Signals};
use std::os::unix::process::CommandExt;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;

struct ActiveAssertions {
    _assertions: Vec<power_management::PowerAssertion>,
    _sleep_guard: Option<process_lock::ProcessLock>,
}

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

fn main() {
    let mut args = Args::parse();
    if !(args.display
        || args.disk
        || args.system
        || args.system_on_ac
        || args.entirely
        || args.user_active)
    {
        // Default to system sleep if no other options are specified
        args.system = true;
    }

    if !cfg!(target_os = "macos") {
        panic!("This program only works on macOS.");
    }

    if args.verbose {
        println!("DEBUG {:#?}", &args);
    }

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

    let mut sleep_str = format!("Preventing sleep types: [{}] ", sleep_types.join(", "));

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

    if let Some(command) = args.command {
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
    } else if args.timeout.is_some() || args.waitfor.is_some() {
        // If timeout or waitfor is used, wait appropriately

        let mut duration = chrono::Duration::try_seconds(0).unwrap();
        let mut end_time = chrono::Local::now();

        let timeout = args.timeout.is_some();
        let waitfor = args.waitfor.is_some();
        if timeout {
            // Timeout selected
            // Print how long we're waiting for
            match duration_parser::parse_duration(&args.timeout.expect("Timeout should be present")) {
                Ok(d) => duration = d,
                Err(e) => {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            }
            end_time += duration;
            let seconds = duration.num_seconds() % 60;
            let minutes = duration.num_minutes() % 60;
            let hours = duration.num_hours() % 24;
            let days = duration.num_days();

            sleep_str += &format!(
                "for {}{}{}{}",
                if days > 0 {
                    format!("{} day{} ", days, if days != 1 { "s" } else { "" })
                } else {
                    String::from("")
                },
                if hours > 0 {
                    format!("{} hour{} ", hours, if hours != 1 { "s" } else { "" })
                } else {
                    String::from("")
                },
                if minutes > 0 {
                    format!("{} minute{} ", minutes, if minutes != 1 { "s" } else { "" })
                } else {
                    String::from("")
                },
                if seconds % 60 > 0 || seconds == 0 {
                    format!(
                        "{} second{}",
                        seconds,
                        if seconds % 60 != 1 { "s" } else { "" }
                    )
                } else {
                    String::new()
                }
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
            // Print when we're resuming
            println!(
                "Resuming {}.",
                if duration.num_seconds() > (60 * 60 * 24) {
                    end_time.format(LONG_FMT)
                } else {
                    end_time.format(SHORT_FMT)
                }
            );
            thread::sleep(duration.to_std().expect("Duration should be valid"));
        }

        if waitfor {
            let pid = args.waitfor.expect("PID should be present");

            // wait without polling using kevent
            let kq = event::Kqueue::new().expect("Failed to create Kqueue");
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

            kq.kevent(&[kev], &mut eventlist, None)
                .expect("Failed to register Kqueue event");
            if args.verbose {
                println!("{:#?}", kev)
            };

            if eventlist[0].flags().contains(event::EvFlags::EV_ERROR) {
                if eventlist[0].data() == nix::Error::ESRCH as isize {
                    println!("PID {} not found", pid);
                } else {
                    eprintln!(
                        "kevent error waiting for PID {}: {}",
                        pid,
                        nix::Error::from_raw(eventlist[0].data() as i32)
                    );
                }
                process::exit(1);
            }

            exit_code = eventlist[0].data() as i32;

            print!("PID {pid} finished ");
            let now = chrono::Local::now();
            print!("{} ", now.format(SHORT_FMT));
            println!("with exit code {}", exit_code);
        }
    } else {
        // If no timer arguments are provided, disable sleep until Ctrl+C is pressed
        sleep_str += "until Ctrl+C pressed.";
        println!("{}", sleep_str);
        thread::park();
    }
    if let Ok(mut guard) = assertions.lock() {
        let _ = guard.take();
    }
    process::exit(exit_code);
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_set_assertions_display() {
        let args = super::Args {
            verbose: false,
            dry_run: false,
            drop_root: false,
            display: true,
            disk: false,
            system: false,
            system_on_ac: false,
            entirely: false,
            user_active: false,
            timeout: None,
            waitfor: None,
            command: None,
        };

        let assertions = super::set_assertions(&args, true);
        assert_eq!(assertions._assertions.len(), 1);
        assert!(assertions._sleep_guard.is_none());
    }

    #[test]
    fn test_set_assertions_multiple_flags() {
        let args = super::Args {
            verbose: false,
            dry_run: false,
            drop_root: false,
            display: true,
            disk: true,
            system: true,
            system_on_ac: false,
            entirely: false,
            user_active: false,
            timeout: None,
            waitfor: None,
            command: None,
        };

        let assertions = super::set_assertions(&args, true);
        // Should have 3 assertions: display, disk, system
        assert_eq!(assertions._assertions.len(), 3);
        assert!(assertions._sleep_guard.is_none());
    }

    #[test]
    fn test_set_assertions_system_on_ac() {
        let args = super::Args {
            verbose: false,
            dry_run: false,
            drop_root: false,
            display: false,
            disk: false,
            system: false,
            system_on_ac: true,
            entirely: false,
            user_active: false,
            timeout: None,
            waitfor: None,
            command: None,
        };

        let assertions = super::set_assertions(&args, true);
        assert_eq!(assertions._assertions.len(), 1);
        assert!(assertions._sleep_guard.is_none());
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("Not supported on this OS");
}
