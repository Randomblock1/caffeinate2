use signal_hook::{consts::SIGINT, iterator::Signals};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use std::{process, thread};

/// Detects if a sleep event occurred by measuring the elapsed time of a sleep operation.
///
/// # Arguments
///
/// * `expected_duration` - The duration we intend to sleep for.
/// * `threshold` - The duration threshold above which we consider a system sleep event occurred.
/// * `measure_sleep` - A function that performs the sleep and returns the actual elapsed duration.
///
/// # Returns
///
/// * `Option<Duration>` - The excess duration (actual - expected) if a sleep event was detected,
///                        or `None` if no sleep event was detected or an error occurred.
fn detect_sleep_event<F>(
    expected_duration: Duration,
    threshold: Duration,
    mut measure_sleep: F,
) -> Option<Duration>
where
    F: FnMut(Duration) -> Result<Duration, ()>,
{
    match measure_sleep(expected_duration) {
        Ok(elapsed) => {
            if elapsed > threshold {
                // Calculate the excess sleep time.
                // We return elapsed - expected_duration as the "sleep duration".
                Some(elapsed.saturating_sub(expected_duration))
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

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

        let sleep_result = detect_sleep_event(SLEEP_DURATION, SLEEP_THRESHOLD, |duration| {
             sleep(duration);
             now.elapsed().map_err(|_| ())
        });

        if let Some(excess_duration) = sleep_result {
            let elapsed_secs = excess_duration.as_secs();
            sleep_arr.lock().unwrap().push(elapsed_secs);
            let now = chrono::Local::now();
            println!(
                "Sleep detected! Slept for {} seconds, woke at {}",
                elapsed_secs,
                now.format("%Y-%m-%d %-I:%M:%S %p")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_detect_sleep_event_no_sleep() {
        let expected = Duration::from_secs(5);
        let threshold = Duration::from_secs(10);

        // Mock: slept exactly as expected
        let result = detect_sleep_event(expected, threshold, |_| Ok(expected));
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_sleep_event_sleep_detected() {
        let expected = Duration::from_secs(5);
        let threshold = Duration::from_secs(10);
        let elapsed = Duration::from_secs(15);

        // Mock: slept longer than threshold
        let result = detect_sleep_event(expected, threshold, |_| Ok(elapsed));
        assert_eq!(result, Some(elapsed - expected));
        assert_eq!(result.unwrap().as_secs(), 10);
    }

    #[test]
    fn test_detect_sleep_event_threshold_boundary() {
        let expected = Duration::from_secs(5);
        let threshold = Duration::from_secs(10);

        // Mock: slept exactly threshold amount
        let result = detect_sleep_event(expected, threshold, |_| Ok(threshold));
        assert_eq!(result, None);

        // Mock: slept threshold + epsilon
        let elapsed = threshold + Duration::from_nanos(1);
        let result = detect_sleep_event(expected, threshold, |_| Ok(elapsed));
        assert!(result.is_some());
    }

    #[test]
    fn test_detect_sleep_event_clock_error() {
        let expected = Duration::from_secs(5);
        let threshold = Duration::from_secs(10);

        // Mock: clock went backwards or error occurred
        let result = detect_sleep_event(expected, threshold, |_| Err(()));
        assert_eq!(result, None);
    }
}
