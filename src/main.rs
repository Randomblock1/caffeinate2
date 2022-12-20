extern crate plist;
extern crate signal_hook;

use signal_hook::{consts::SIGINT, iterator::Signals};
use std::process;
use std::io;

fn disable_sleep(new_value: bool) {
    let path = "/Library/Preferences/com.apple.PowerManagement.plist";

    // Open the file
    let mut value: plist::Value = match plist::from_file(path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {}", std::error::Error::source(&e).unwrap());
            process::exit(1);
        }
    };

    // Check if we can write to the file at the given path
    if let Err(e) = std::fs::OpenOptions::new().write(true).open(path) {
        eprintln!("Error: Cannot write to file: {e}");
        eprintln!("This program must be run with root permissions. Try using sudo.");
        process::exit(1);
    }

    // Get the "SystemPowerSettings" dictionary from the root dictionary
    let system_power_settings = value
        .as_dictionary_mut()
        .and_then(|dict| dict.get_mut("SystemPowerSettings"))
        .and_then(|dict| dict.as_dictionary_mut())
        .unwrap_or_else(|| {
            eprintln!("Error: Could not read file as dictionary");
            process::exit(1);
        });

    // Get the "SleepDisabled" key from the "SystemPowerSettings" dictionary
    system_power_settings
        .get("SleepDisabled")
        .and_then(|val| val.as_boolean())
        .unwrap_or_else(|| {
            eprintln!("Error: Could not get SleepDisabled from SystemPowerSettings dictionary");
            process::exit(1);
        });

    // Set the new SleepDisabled value in the "SystemPowerSettings" dictionary
    system_power_settings.insert(
        "SleepDisabled".to_string(),
        plist::Value::Boolean(new_value),
    );

    // Write the modified value back to the file
    if let Err(e) = plist::to_file_xml(path, &value) {
        eprintln!("Error writing to file: {}", e);
        process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut signals = Signals::new(&[SIGINT])?;

    std::thread::spawn(move || {
        for _ in signals.forever() {
            disable_sleep(false);
            process::exit(0);
        }
    });

    let args: Vec<String> = std::env::args().collect();
    println!("{:?}", args);

    if args.len() == 1 {
        // If no arguments are provided, disable sleep until Ctrl+C is pressed
        disable_sleep(true);
        println!("Preventing sleep until Ctrl+C pressed.");
        std::thread::park();
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
        disable_sleep(true);
        std::thread::sleep(duration);

        disable_sleep(false);
        process::exit(0);
    } else {
        // Otherwise, disable sleep while running the given command
        disable_sleep(true);
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

        process::exit(child.wait()?.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This program only works on macOS");
    process::exit(1);
}
