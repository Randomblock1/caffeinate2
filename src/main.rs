#![cfg(target_os = "macos")]

pub mod power_management;
pub mod process_lock;

use clap::Parser;
use nix::{sys::event, unistd};
use signal_hook::{consts::SIGINT, iterator::Signals};
use std::os::unix::process::CommandExt;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug)]
struct ActiveAssertions {
    _assertions: Vec<power_management::PowerAssertion>,
    _sleep_guard: Option<process_lock::ProcessLock>,
}

trait PowerManager {
    fn create_assertion(
        &self,
        assertion_type: power_management::AssertionType,
        state: bool,
        verbose: bool,
    ) -> Result<power_management::PowerAssertion, u32>;

    fn declare_user_activity(
        &self,
        state: bool,
        verbose: bool,
    ) -> Result<power_management::PowerAssertion, u32>;

    fn create_process_lock(
        &self,
        verbose: bool,
    ) -> Result<process_lock::ProcessLock, Box<dyn std::error::Error>>;
}

struct RealPowerManager;

impl PowerManager for RealPowerManager {
    fn create_assertion(
        &self,
        assertion_type: power_management::AssertionType,
        state: bool,
        verbose: bool,
    ) -> Result<power_management::PowerAssertion, u32> {
        power_management::create_assertion(assertion_type, state, verbose)
    }

    fn declare_user_activity(
        &self,
        state: bool,
        verbose: bool,
    ) -> Result<power_management::PowerAssertion, u32> {
        power_management::declare_user_activity(state, verbose)
    }

    fn create_process_lock(
        &self,
        verbose: bool,
    ) -> Result<process_lock::ProcessLock, Box<dyn std::error::Error>> {
        process_lock::ProcessLock::new(verbose)
    }
}

fn set_assertions(
    args: &Args,
    state: bool,
    pm: &impl PowerManager,
) -> Result<ActiveAssertions, Box<dyn std::error::Error>> {
    if args.dry_run {
        return Ok(ActiveAssertions {
            _assertions: Vec::new(),
            _sleep_guard: None,
        });
    }

    let sleep_guard = if args.entirely {
        match pm.create_process_lock(args.verbose) {
            Ok(guard) => Some(guard),
            Err(e) => {
                return Err(format!(
                    "Error: Failed to acquire process lock or disable sleep: {}",
                    e
                )
                .into());
            }
        }
    } else {
        None
    };

    let mut assertions = Vec::new();

    let mut add_assertion = |result: Result<power_management::PowerAssertion, u32>,
                             name: &str|
     -> Result<(), Box<dyn std::error::Error>> {
        match result {
            Ok(assertion) => {
                assertions.push(assertion);
                Ok(())
            }
            Err(code) => Err(format!(
                "Error: Failed to create {} assertion (code: {:X})",
                name, code
            )
            .into()),
        }
    };

    if args.display {
        add_assertion(
            pm.create_assertion(
                power_management::AssertionType::PreventUserIdleDisplaySleep,
                state,
                args.verbose,
            ),
            "display sleep",
        )?;
    }
    if args.disk {
        add_assertion(
            pm.create_assertion(
                power_management::AssertionType::PreventDiskIdle,
                state,
                args.verbose,
            ),
            "disk idle",
        )?;
    }
    if args.system {
        add_assertion(
            pm.create_assertion(
                power_management::AssertionType::PreventUserIdleSystemSleep,
                state,
                args.verbose,
            ),
            "system sleep",
        )?;
    }
    if args.system_on_ac {
        add_assertion(
            pm.create_assertion(
                power_management::AssertionType::PreventSystemSleep,
                state,
                args.verbose,
            ),
            "system sleep on AC",
        )?;
    }

    if args.user_active {
        add_assertion(
            pm.declare_user_activity(true, args.verbose),
            "user activity",
        )?;
    }

    if args.verbose {
        println!("Assertions created");
    }

    Ok(ActiveAssertions {
        _assertions: assertions,
        _sleep_guard: sleep_guard,
    })
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

fn parse_duration(duration: String) -> chrono::Duration {
    let duration = duration.trim();

    match humantime::parse_duration(duration) {
        Ok(std_duration) => chrono::Duration::from_std(std_duration).unwrap_or_else(|_| {
            eprintln!("Error: Timeout is too large!");
            process::exit(1)
        }),
        Err(_) => {
            let seconds = duration.parse::<u64>().unwrap_or_else(|_| {
                eprintln!("Error: Timeout isn't a valid duration or number!");
                process::exit(1)
            });
            chrono::Duration::try_seconds(seconds.try_into().unwrap_or_else(|_| {
                eprintln!("Error: Timeout is too large!");
                process::exit(1)
            }))
            .unwrap_or_else(|| {
                eprintln!("Error: Timeout is too large!");
                process::exit(1)
            })
        }
    }
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

    let mut sleep_str = "Preventing sleep types: ".to_string();

    sleep_str += "[ ";

    if args.display {
        sleep_str += "Display ";
    }
    if args.disk {
        sleep_str += "Disk ";
    }
    if args.system {
        sleep_str += "System ";
    }
    if args.system_on_ac {
        sleep_str += "System (if on AC) ";
    }
    if args.entirely {
        sleep_str += "Entirely ";
    }
    if args.user_active {
        sleep_str += "User active ";
    }
    sleep_str += "] ";

    let pm = RealPowerManager;
    let assertions = match set_assertions(&args, true, &pm) {
        Ok(a) => Arc::new(Mutex::new(Some(a))),
        Err(e) => {
            eprintln!("{}", e);
            process::exit(1);
        }
    };
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
            duration = parse_duration(args.timeout.expect("Timeout should be present"));
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
    use crate::power_management::{AssertionType, PowerAssertion};
    use crate::process_lock::ProcessLock;
    use crate::PowerManager;
    use std::cell::RefCell;

    struct MockPowerManager {
        should_fail_assertion: bool,
        should_fail_lock: bool,
        assertions_created: RefCell<Vec<String>>,
    }

    impl MockPowerManager {
        fn new() -> Self {
            Self {
                should_fail_assertion: false,
                should_fail_lock: false,
                assertions_created: RefCell::new(Vec::new()),
            }
        }
    }

    impl PowerManager for MockPowerManager {
        fn create_assertion(
            &self,
            assertion_type: AssertionType,
            _state: bool,
            verbose: bool,
        ) -> Result<PowerAssertion, u32> {
            if self.should_fail_assertion {
                Err(1)
            } else {
                self.assertions_created
                    .borrow_mut()
                    .push(assertion_type.as_str().to_string());
                Ok(PowerAssertion::new_test(1, verbose))
            }
        }

        fn declare_user_activity(
            &self,
            _state: bool,
            verbose: bool,
        ) -> Result<PowerAssertion, u32> {
            if self.should_fail_assertion {
                Err(1)
            } else {
                self.assertions_created
                    .borrow_mut()
                    .push("UserActivity".to_string());
                Ok(PowerAssertion::new_test(2, verbose))
            }
        }

        fn create_process_lock(
            &self,
            verbose: bool,
        ) -> Result<ProcessLock, Box<dyn std::error::Error>> {
            if self.should_fail_lock {
                Err("Lock failure".into())
            } else {
                Ok(ProcessLock::new_test(verbose))
            }
        }
    }

    #[test]
    fn test_parse_duration() {
        let duration = "1d 2h 3m 4s".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 93784);

        let duration = "1day 2h 3m".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 93780);

        let duration = "3min 17h 2s".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 61382);

        let duration = "45323".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 45323);
    }

    #[test]
    fn test_parse_duration_edge_cases() {
        // Test 0
        let duration = "0s".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 0);

        // Test large number
        let duration = "1000000s".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 1000000);

        // Test just number
        let duration = "60".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result.num_seconds(), 60);
    }

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

        let pm = MockPowerManager::new();
        let assertions = super::set_assertions(&args, true, &pm).unwrap();
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

        let pm = MockPowerManager::new();
        let assertions = super::set_assertions(&args, true, &pm).unwrap();
        assert_eq!(assertions._assertions.len(), 1);
        assert!(assertions._sleep_guard.is_none());
        assert_eq!(pm.assertions_created.borrow()[0], "PreventUserIdleDisplaySleep");
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

        let pm = MockPowerManager::new();
        let assertions = super::set_assertions(&args, true, &pm).unwrap();
        // Should have 3 assertions: display, disk, system
        assert_eq!(assertions._assertions.len(), 3);
        assert!(assertions._sleep_guard.is_none());

        let created = pm.assertions_created.borrow();
        assert!(created.contains(&"PreventUserIdleDisplaySleep".to_string()));
        assert!(created.contains(&"PreventDiskIdle".to_string()));
        assert!(created.contains(&"PreventUserIdleSystemSleep".to_string()));
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

        let pm = MockPowerManager::new();
        let assertions = super::set_assertions(&args, true, &pm).unwrap();
        assert_eq!(assertions._assertions.len(), 1);
        assert!(assertions._sleep_guard.is_none());
        assert_eq!(pm.assertions_created.borrow()[0], "PreventSystemSleep");
    }

    #[test]
    fn test_set_assertions_lock_failure() {
        let args = super::Args {
            verbose: false,
            dry_run: false,
            drop_root: false,
            display: false,
            disk: false,
            system: false,
            system_on_ac: false,
            entirely: true,
            user_active: false,
            timeout: None,
            waitfor: None,
            command: None,
        };

        let mut pm = MockPowerManager::new();
        pm.should_fail_lock = true;

        let result = super::set_assertions(&args, true, &pm);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to acquire process lock"));
    }

    #[test]
    fn test_set_assertions_assertion_failure() {
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

        let mut pm = MockPowerManager::new();
        pm.should_fail_assertion = true;

        let result = super::set_assertions(&args, true, &pm);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to create display sleep assertion"));
    }
}
