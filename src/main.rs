#![cfg(target_os = "macos")]

pub mod power_management;

use clap::Parser;
use power_management::*;
use signal_hook::{consts::SIGINT, iterator::Signals};
use std::io;
use std::process;
use std::thread;

#[link(name = "pmstub")]
extern "C" {
    pub fn getSleepDisabled() -> bool;
}

fn disable_system_sleep(sleep_disabled: bool) {
    let result = disable_sleep(sleep_disabled);

    // See IOKit/IOReturn.h for error codes.
    if result == 0xE00002C1 {
        eprintln!(
            "Error: Could not modify system sleep: Permission denied. Try running with sudo."
        );
        process::exit(1);
    } else if result != 0 {
        eprintln!(
            "Error: Could not modify system sleep: IOReturn code {:X}",
            result
        );
        process::exit(1);
    }
}

fn set_assertions(args: &Args, state: bool) -> Vec<u32> {
    let mut assertions = Vec::new();
    if args.display {
        // Prevents the display from dimming automatically.
        assertions.push(create_assertion("PreventUserIdleDisplaySleep", state));
    }
    if args.disk {
        // Prevents the disk from stopping when idle.
        assertions.push(create_assertion("PreventDiskIdle", state));
    }
    if args.system {
        // Prevents the system from sleeping automatically.
        assertions.push(create_assertion("PreventUserIdleSystemSleep", state));
    }
    if args.system_on_ac {
        // Prevents the system from sleeping when on AC power.
        assertions.push(create_assertion("PreventSystemSleep", state));
    }
    if args.entirely {
        // Prevents the system from sleeping entirely.
        disable_system_sleep(true);
    }
    if args.user_active {
        // Declares the user is active.
        assertions.push(declare_user_activity());
    }

    #[cfg(debug_assertions)]
    println!("Assertions: {:?}", assertions);

    assertions
}

fn release_assertions(assertions: Vec<u32>) {
    for assertion in assertions {
        release_assertion(assertion);
    }
    if unsafe { getSleepDisabled() } {
        disable_system_sleep(false);
    }
}

/// Clap args
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
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
    /// If the display is off, this option turns the display on and prevents the display from going into idle sleep.
    /// If a timeout is not specified with '-t' option, then this assertion is taken with a default of 5 second timeout.
    #[arg(short, long)]
    user_active: bool,

    /// Wait for X seconds.
    #[arg(short, long, name = "SECONDS")]
    timeout: Option<u64>,

    /// Wait for program with PID X to complete.
    #[arg(short, long, name = "PID")]
    waitfor: Option<i32>,

    /// Wait for given command to complete (takes priority above timeout and pid)
    #[arg()]
    command: Option<Vec<String>>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();
    if !args.display
        && !args.disk
        && !args.system
        && !args.system_on_ac
        && !args.entirely
        && !args.user_active
    {
        // Default to system sleep if no other options are specified
        args.system = true;
    }

    let assertions = set_assertions(&args, true);

    #[cfg(debug_assertions)]
    println!("DEBUG {:#?}", &args);

    if !cfg!(target_os = "macos") {
        eprintln!("This program only works on macOS");
        process::exit(1);
    }

    let mut signals = Signals::new([SIGINT])?;

    let assertions_clone = assertions.clone();
    thread::spawn(move || {
        for _ in signals.forever() {
            release_assertions(assertions_clone);
            process::exit(0);
        }
    });

    if args.command.is_some() {
        // If command is passed, it takes priority over everything else
        let command = args.command.unwrap();
        // Disable sleep while running the given command
        println!("Preventing sleep until command finishes.");

        let mut child = process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command.join(" "))
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stdout_reader = io::BufReader::new(stdout);
        let stderr_reader = io::BufReader::new(stderr);

        for line in io::BufRead::lines(stdout_reader) {
            println!("{}", line?);
        }

        for line in io::BufRead::lines(stderr_reader) {
            eprintln!("{}", line?);
        }

        release_assertions(assertions);
        process::exit(child.wait()?.code().unwrap_or(1));
    } else if args.timeout.is_some() || args.waitfor.is_some() {
        // If timeout or waitfor is used, wait appropriately
        // The original caffeinate treats arg position as priority
        let args_vec = std::env::args().collect::<Vec<_>>();
        let timeout_index = args_vec
            .iter()
            .position(|x| x == "--timeout" || x == "-t")
            .unwrap_or(std::usize::MAX);
        let waitfor_index = args_vec
            .iter()
            .position(|x| x == "--waitfor" || x == "-w")
            .unwrap_or(std::usize::MAX);
        if timeout_index < waitfor_index {
            let secs = args.timeout.unwrap();
            let duration = std::time::Duration::from_secs(secs);
            println!("Preventing sleep for {secs} seconds.");
            thread::sleep(duration);
            release_assertions(assertions);
            process::exit(0);
        } else {
            let pid = args.waitfor.unwrap();
            println!("Sleeping until PID {pid} finishes.");
            // TODO
        }
    } else {
        // If no arguments are provided, disable sleep until Ctrl+C is pressed
        set_assertions(&args, true);
        println!("Preventing sleep until Ctrl+C pressed.");
        thread::park();
    }
    Ok(())
}
