#![cfg(target_os = "macos")]

mod power_management;

use clap::Parser;
use signal_hook::{consts::SIGINT, iterator::Signals};
use std::io;
use std::os::unix::process::CommandExt;
use std::process;
use std::sync::mpsc::channel;
use std::thread;
use time::macros::format_description;

fn set_assertions(iokit: &power_management::IOKit, args: &Args, state: bool) -> Vec<u32> {
    if args.dry_run {
        // Don't actually sleep
        return Vec::new();
    }

    if args.entirely {
        // Prevents the system from sleeping entirely.
        iokit.set_sleep_disabled(true).unwrap_or_else(|_| {
            eprintln!("Error: Insufficient privileges to disable sleep. Try running with sudo.");
            process::exit(1);
        });
    }

    let mut assertions = Vec::new();
    if args.display {
        // Prevents the display from dimming automatically.
        assertions.push(iokit.create_assertion("PreventUserIdleDisplaySleep", state));
    }
    if args.disk {
        // Prevents the disk from stopping when idle.
        assertions.push(iokit.create_assertion("PreventDiskIdle", state));
    }
    if args.system {
        // Prevents the system from sleeping automatically.
        assertions.push(iokit.create_assertion("PreventUserIdleSystemSleep", state));
    }
    if args.system_on_ac {
        // Prevents the system from sleeping when on AC power.
        assertions.push(iokit.create_assertion("PreventSystemSleep", state));
    }

    if args.user_active {
        // Declares the user is active.
        assertions.push(iokit.declare_user_activity(true));
    }

    if args.verbose {
        println!("Assertions: {:?}", assertions);
    }

    assertions
}

fn release_assertions(iokit: &power_management::IOKit, assertions: &Vec<u32>) {
    for assertion in assertions {
        iokit.release_assertion(*assertion);
    }
    if power_management::IOKit::get_sleep_disabled(iokit) {
        iokit.set_sleep_disabled(false).unwrap_or_else(|_| {
            eprintln!("Error: Insufficient privileges to disable sleep. Try running with sudo.");
            process::exit(1);
        });
    }
}

/// Clap args
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Verbose mode
    #[arg(short, long)]
    verbose: bool,

    /// Dry run. Don't actually sleep.
    /// Useful for testing.
    #[arg(long)]
    dry_run: bool,

    /// Drop root privileges in command.
    /// Some programs don't want to work as root,
    /// but you need root to disable sleep entirely.
    #[arg(long)]
    drop_root: bool,

    /// Disable display sleep
    #[arg(short, long)]
    display: bool,

    /// Disable disk idle sleep
    #[arg(short = 'm', long)]
    disk: bool,

    /// Disable idle system sleep. Default if no other options are specified.
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

    /// Wait for program with PID X to complete.
    #[arg(short, long, name = "PID")]
    waitfor: Option<i32>,

    /// Wait for given command to complete (takes priority above timeout and pid)
    #[arg()]
    command: Option<Vec<String>>,
}

fn parse_duration(duration: String) -> u64 {
    // Use regex to split the duration into a bunch of number and unit pairs
    let mut total_seconds = 0;
    let re = regex::Regex::new(r"(\d+)\s*(s|m|h|d)").unwrap();

    for captures in re.captures_iter(&duration) {
        let number = captures[1]
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("invalid timeout"));
        let unit = &captures[2];

        match unit {
            "s" => total_seconds += number,
            "m" => total_seconds += number * 60,
            "h" => total_seconds += number * 3600,
            "d" => total_seconds += number * 86400,
            _ => panic!("invalid duration"),
        }
    }

    // If no units were specified, assume seconds
    if total_seconds == 0 {
        total_seconds = duration.parse().unwrap_or_else(|_| {
            eprintln!("Error: Timeout isn't a valid duration or number!");
            process::exit(1)
        });
    }

    total_seconds
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

    // Print types of sleep prevented
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

    let iokit = power_management::IOKit::new();
    let assertions = set_assertions(&iokit, &args, true);

    let mut exit_code = 0;

    let mut signals = Signals::new([SIGINT]).unwrap();
    let assertions_clone = assertions.clone();
    thread::spawn(move || {
        for _ in signals.forever() {
            release_assertions(&power_management::IOKit::new(), &assertions_clone);
            process::exit(exit_code);
        }
    });

    if args.command.is_some() {
        // If command is passed, it takes priority over everything else
        let command = args.command.unwrap();
        // Disable sleep while running the given command
        sleep_str += "until command finishes.";
        println!("{sleep_str}");

        let uid;
        let gid;

        if args.drop_root {
            let uid_str =
                std::env::var("SUDO_UID").unwrap_or_else(|_| nix::unistd::getuid().to_string());
            let gid_str =
                std::env::var("SUDO_GID").unwrap_or_else(|_| nix::unistd::getgid().to_string());

            uid = uid_str.parse::<u32>().unwrap();
            gid = gid_str.parse::<u32>().unwrap();
        } else {
            uid = nix::unistd::getuid().into();
            gid = nix::unistd::getgid().into();
        }

        if args.verbose {
            println!("uid: {uid}, gid: {gid}");
        }

        let mut child = process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command.join(" "))
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped())
            .uid(uid)
            .gid(gid)
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stdout_reader = io::BufReader::new(stdout);
        let stderr_reader = io::BufReader::new(stderr);

        for line in io::BufRead::lines(stdout_reader) {
            println!("{}", line.unwrap());
        }

        for line in io::BufRead::lines(stderr_reader) {
            eprintln!("{}", line.unwrap());
        }

        exit_code = child.wait().unwrap().code().unwrap_or(0);
    } else if args.timeout.is_some() || args.waitfor.is_some() {
        // If timeout or waitfor is used, wait appropriately
        // TODO Major Refactor Needed: They can be used at the same time, but the code is a mess

        let (sender, receiver) = channel();

        let mut duration = std::time::Duration::from_secs(0);
        let mut end_time = time::OffsetDateTime::now_local().unwrap();

        let timeout = args.timeout.is_some();
        let waitfor = args.waitfor.is_some();
        if timeout {
            // Timeout selected
            // Print how long we're waiting for
            duration = std::time::Duration::from_secs(parse_duration(args.timeout.unwrap()));
            end_time = time::OffsetDateTime::now_local().unwrap() + duration;
            let seconds = duration.as_secs() % 60;
            let minutes = (duration.as_secs() % 3600) / 60;
            let hours = (duration.as_secs() % 86400) / 3600;
            let days = duration.as_secs() / 86400;

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
                    String::from("")
                }
            );
        }

        print!("{sleep_str}");

        if timeout && waitfor {
            print!(" or ");
        }
        if waitfor {
            print!("until PID {} finishes", args.waitfor.unwrap());
        }
        println!(".");

        if timeout {
            let short_fmt = format_description!("at [hour repr:12]:[minute]:[second] [period]");
            let long_fmt = format_description!(
                "on [month repr:long] [day] at [hour repr:12]:[minute]:[second] [period]"
            );

            // Print when we're resuming
            println!(
                "Resuming {}.",
                if duration.as_secs() > (60 * 60 * 24) {
                    end_time.format(&long_fmt).unwrap()
                } else {
                    end_time.format(&short_fmt).unwrap()
                }
            );

            // Spawn a new thread to handle the timeout
            let timeout_sender = sender.clone();
            thread::spawn(move || {
                thread::sleep(duration);
                timeout_sender.send(0).unwrap();
            });
        }

        if waitfor {
            // Wait for PID selected
            let pid = args.waitfor.unwrap();

            let mut child = process::Command::new("lsof")
                .arg("-p")
                .arg(pid.to_string())
                .arg("+r")
                .arg("1")
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .spawn()
                .unwrap();

            // Spawn a new thread to handle the timeout
            let waitpid_sender = sender.clone();
            thread::spawn(move || {
                let status = child.wait().unwrap();
                let exit_code;

                if status.code() == Some(1) {
                    eprintln!("PID {} does not exist.", pid);
                    exit_code = 1;
                } else {
                    print!("PID {pid} finished ");
                    exit_code = 0;
                    let now = time::OffsetDateTime::now_local().unwrap();
                    let short_fmt =
                        format_description!("at [hour repr:12]:[minute]:[second] [period]");
                    println!("{}", now.format(&short_fmt).unwrap());
                }
                waitpid_sender.send(exit_code).unwrap();
            });
        }

        // Wait for either the timeout or the process to finish
        exit_code = receiver.recv().unwrap();
    } else {
        // If no timer arguments are provided, disable sleep until Ctrl+C is pressed
        sleep_str += "until Ctrl+C pressed.";
        println!("{}", sleep_str);
        thread::park();
    }
    release_assertions(&iokit, &assertions);
    process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_duration() {
        let duration = "1d2h3m4s".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result, 93784);

        let duration = "1day 2hrs3m".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result, 93780);

        let duration = "3 minutes 17 hours 2 seconds".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result, 61382);

        let duration = "45323".to_string();
        let result = super::parse_duration(duration);
        assert_eq!(result, 45323);
    }
}
