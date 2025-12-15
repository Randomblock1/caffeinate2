use signal_hook::{consts::SIGINT, iterator::Signals};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use std::{process, thread};

fn main() {
    const SLEEP_TIME: u64 = 5;
    const SLEEP_DURATION: Duration = Duration::from_secs(SLEEP_TIME);
    const SLEEP_THRESHOLD: Duration = Duration::from_secs(SLEEP_TIME * 2);

    let sleep_arr = Arc::new(Mutex::new(Vec::new()));

    let mut signals = Signals::new([SIGINT]).expect("Failed to create signal iterator");
    let sleep_arr_clone = sleep_arr.clone();
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let len = sleep_arr_clone.lock().unwrap().len();
            if len != 0 {
                println!("\nSleep was detected {} times", len);
                println!(
                    "On average, slept for {} seconds",
                    sleep_arr_clone.lock().unwrap().iter().sum::<u64>() / len as u64
                );
            } else {
                println!("\nNo sleep was detected");
            }
            process::exit(0);
        }
    });

    // Detect sleep by measuring actual elapsed time vs expected sleep duration.
    // If the system sleeps, the elapsed time will be much longer than requested.
    loop {
        let now = SystemTime::now();

        sleep(SLEEP_DURATION);

        let elapsed = match now.elapsed() {
            Ok(e) => e,
            Err(_) => continue, // system clock adjusted backwards
        };

        if elapsed > SLEEP_THRESHOLD {
            let elapsed_secs = elapsed.as_secs();
            sleep_arr.lock().unwrap().push(elapsed_secs - SLEEP_TIME);
            let now = chrono::Local::now();
            println!(
                "Sleep detected! Slept for {} seconds, woke at {}",
                elapsed_secs - SLEEP_TIME,
                now.format("%Y-%m-%d %-I:%M:%S %p")
            );
        }
    }
}
