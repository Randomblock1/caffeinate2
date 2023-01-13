#![cfg(target_os = "macos")]

use clap::Parser;
use signal_hook::{consts::SIGINT, iterator::Signals};
use std::io;
use std::process;
use std::thread;

#[link(name = "pmstub")]
extern "C" {
    pub fn setSleepDisabled(sleepDisabled: bool) -> core::ffi::c_uint;
    pub fn getSleepDisabled() -> bool;
}

fn disable_system_sleep(sleep_disabled: bool) {
    let result;
    if sleep_disabled {
        result = unsafe { setSleepDisabled(true) };
    } else {
        result = unsafe { setSleepDisabled(false) };
    }

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

// Clap args
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Disable display sleep
    #[arg(short, long)]
    display: bool,

    /// Disable disk idle sleep
    #[arg(short = 'm', long)]
    disk: bool,

    /// Disable idle system sleep
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
    let args = Args::parse();

    #[cfg(debug_assertions)]
    println!("DEBUG {:#?}", args);

    if !cfg!(target_os = "macos") {
        eprintln!("This program only works on macOS");
        process::exit(1);
    }

    let mut signals = Signals::new([SIGINT])?;

    thread::spawn(move || {
        for _ in signals.forever() {
            disable_system_sleep(false);
            process::exit(0);
        }
    });

    if args.command.is_some() {
        // If command is passed, it takes priority over everything else
        let command = args.command.unwrap();
        // Otherwise, disable sleep while running the given command
        disable_system_sleep(true);
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

        disable_system_sleep(false);
        process::exit(child.wait()?.code().unwrap_or(1));
    } else if args.timeout.is_some() || args.waitfor.is_some() {
        // If timeout or waitfor is used, wait appropriately
        // The original caffeinate treats arg position as priority
        let args_vec = std::env::args().collect::<Vec<_>>();
        let timeout_index = args_vec.iter().position(|x| x == "--timeout" || x == "-t");
        let waitfor_index = args_vec.iter().position(|x| x == "--waitfor" || x == "-w");
        if timeout_index < waitfor_index {
            let secs = args.timeout.unwrap();
            let duration = std::time::Duration::from_secs(secs);
            println!("Preventing sleep for {secs} seconds.");
            disable_system_sleep(true);
            thread::sleep(duration);
            disable_system_sleep(false);
            process::exit(0);
        } else {
            let pid = args.waitfor.unwrap();
            println!("Sleeping until PID {pid} finishes.");
            // TODO
        }
    } else {
        // If no arguments are provided, disable sleep until Ctrl+C is pressed
        disable_system_sleep(true);
        println!("Preventing sleep until Ctrl+C pressed.");
        thread::park();
    }
    Ok(())
}
