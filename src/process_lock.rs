use crate::power_management;
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const LOCK_FILE_PATH: &str = "/tmp/caffeinate2.lock";

type SleepDisabler = Box<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;
type ProcessChecker = Box<dyn Fn(i32) -> bool + Send + Sync>;

pub struct ProcessLock {
    verbose: bool,
    lock_file_path: PathBuf,
    sleep_disabler: SleepDisabler,
    process_checker: ProcessChecker,
}

impl ProcessLock {
    pub fn new(verbose: bool) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_options(
            verbose,
            PathBuf::from(LOCK_FILE_PATH),
            Box::new(power_management::set_sleep_disabled),
            Box::new(default_process_checker),
        )
    }

    fn with_options(
        verbose: bool,
        lock_file_path: PathBuf,
        sleep_disabler: SleepDisabler,
        process_checker: ProcessChecker,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let should_disable = update_lockfile(true, verbose, &lock_file_path, &process_checker)?;

        if should_disable {
            if verbose {
                println!("First instance detected. Disabling system sleep globally.");
            }
            sleep_disabler(true, verbose).map_err(|code| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to disable sleep (IOKit error: {:X})", code),
                )
            })?;
        } else if verbose {
            println!("Other instances running. Sleep already disabled.");
        }

        Ok(Self {
            verbose,
            lock_file_path,
            sleep_disabler,
            process_checker,
        })
    }
}

fn default_process_checker(pid: i32) -> bool {
    if pid == std::process::id() as i32 {
        return true;
    }
    match kill(Pid::from_raw(pid), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true, // Assume alive on other errors (e.g. permission) to be safe
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        match update_lockfile(
            false,
            self.verbose,
            &self.lock_file_path,
            &self.process_checker,
        ) {
            Ok(should_enable) => {
                if should_enable {
                    if self.verbose {
                        println!("Last instance exiting. Re-enabling system sleep globally.");
                    }
                    if let Err(code) = (self.sleep_disabler)(false, self.verbose) {
                        eprintln!("Error: Failed to re-enable sleep (IOKit error: {:X})", code);
                    }
                } else if self.verbose {
                    println!("Other instances still running. Keeping sleep disabled.");
                }
            }
            Err(e) => {
                eprintln!("Error updating lockfile during exit: {}", e);
            }
        }
    }
}

/// Returns true if the state should change
fn update_lockfile(
    add: bool,
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
) -> Result<bool, std::io::Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o666)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(path)?;

    let mut file = match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(f) => f,
        Err((_, e)) => return Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    };

    let mut content = String::new();
    file.read_to_string(&mut content)?;

    let current_pid = std::process::id() as i32;
    let mut pids: HashSet<i32> = content
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect();

    // Filter out dead processes
    pids.retain(|&pid| {
        if pid == current_pid {
            return true;
        }
        if process_checker(pid) {
            true
        } else {
            if verbose {
                println!("Removing stale PID {} from lockfile", pid);
            }
            false
        }
    });

    let active_count_before = pids.len();

    if add {
        pids.insert(current_pid);
    } else {
        pids.remove(&current_pid);
    }

    let active_count_after = pids.len();

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    for pid in &pids {
        writeln!(file, "{}", pid)?;
    }

    let should_toggle = if add {
        active_count_before == 0
    } else {
        active_count_after == 0
    };

    Ok(should_toggle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::{Arc, Mutex};

    // Since we don't have rand crate in Cargo.toml and don't want to add it just for this if not needed,
    // let's use a simple counter or SystemTime for unique names.
    fn temp_lock_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let start = SystemTime::now();
        let since_the_epoch = start.duration_since(UNIX_EPOCH).unwrap();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_test_{}.lock",
            since_the_epoch.as_nanos()
        ));
        path
    }

    #[test]
    fn test_first_instance() {
        let lock_path = temp_lock_path();
        // Ensure file doesn't exist (though temp dir should be cleanish or unique name)
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(|_pid: i32| false); // No other processes running

        let lock = ProcessLock::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        )
        .unwrap();

        // Check if file created and contains PID
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&std::process::id().to_string()));

        // Check if sleep disabled (true)
        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], true);

        // Cleanup done by Drop (or manually if we want to check state before drop)
        drop(calls); // Release lock on calls
        drop(lock); // Drop the lock

        // Check if sleep re-enabled (false)
        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], false);

        // Cleanup file
        if lock_path.exists() {
             std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn test_subsequent_instance() {
        let lock_path = temp_lock_path();

        // Pre-populate with another PID
        let other_pid = 99999;
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{}", other_pid).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        // process checker says other_pid is ALIVE
        let process_checker = Box::new(move |pid: i32| pid == other_pid);

        let lock = ProcessLock::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        )
        .unwrap();

        // Check file contains both PIDs
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&other_pid.to_string()));
        assert!(content.contains(&std::process::id().to_string()));

        // Check sleep disabler NOT called (since other instance is running)
        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 0);

        drop(calls);
        drop(lock);

        // Check sleep disabler NOT called on drop either (other instance still running)
        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 0);

        if lock_path.exists() {
             std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn test_stale_pid_cleanup() {
        let lock_path = temp_lock_path();

        // Pre-populate with a dead PID
        let dead_pid = 88888;
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{}", dead_pid).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        // process checker says dead_pid is DEAD
        let process_checker = Box::new(|_pid: i32| false);

        let lock = ProcessLock::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        )
        .unwrap();

        // Check file contains ONLY current PID (dead one removed)
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(!content.contains(&dead_pid.to_string()));
        assert!(content.contains(&std::process::id().to_string()));

        // Since we are now the "first" valid instance, sleep should be disabled
        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], true);

        drop(calls);
        drop(lock);

        if lock_path.exists() {
             std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
