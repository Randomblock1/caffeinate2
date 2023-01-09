#![cfg(target_os = "macos")]

extern crate plist;
extern crate signal_hook;

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
        unsafe {
            result = setSleepDisabled(true);
        }
    } else {
        unsafe {
            result = setSleepDisabled(false);
        }
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let args: Vec<String> = std::env::args().collect();

    if args.len() == 1 {
        // If no arguments are provided, disable sleep until Ctrl+C is pressed
        disable_system_sleep(true);
        println!("Preventing sleep until Ctrl+C pressed.");
        thread::park();
    } else if args.len() == 3 && args[1] == "-t" {
        // If the -t flag is provided, sleep for the given number of seconds
        let secs: u64 = match args[2].parse() {
            Ok(num) => num,
            Err(_) => {
                eprintln!("Error: invalid number provided for the -t flag");
                process::exit(1);
            }
        };

        let duration = std::time::Duration::from_secs(secs);
        println!("Preventing sleep for {secs} seconds.");
        disable_system_sleep(true);
        thread::sleep(duration);
        disable_system_sleep(false);
        process::exit(0);
    } else {
        // Otherwise, disable sleep while running the given command
        disable_system_sleep(true);
        println!("Preventing sleep until command finishes.");

        let mut child = process::Command::new("/bin/sh")
            .arg("-c")
            .args(&args[1..])
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
    }
    Ok(())
}
