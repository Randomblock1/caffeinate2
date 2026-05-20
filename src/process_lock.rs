use crate::{
    lockfile::{self, ProcessChecker, ProcessId, ProcessStartTime},
    power_management,
};
use libc::{PROC_PIDTBSDINFO, proc_bsdinfo, proc_pidinfo};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::path::PathBuf;

fn get_process_start_time(pid: i32) -> Option<ProcessStartTime> {
    unsafe {
        let mut info = std::mem::zeroed::<proc_bsdinfo>();
        let size = std::mem::size_of::<proc_bsdinfo>() as i32;
        let ret = proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut _,
            size,
        );
        if ret == size {
            Some(ProcessStartTime {
                seconds: info.pbi_start_tvsec,
                microseconds: info.pbi_start_tvusec,
            })
        } else {
            None
        }
    }
}

type SleepDisabler = Box<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;

pub struct ProcessLock {
    verbose: bool,
    lock_file_path: PathBuf,
    sleep_disabler: SleepDisabler,
    process_checker: Box<ProcessChecker>,
    process_id: ProcessId,
}

impl ProcessLock {
    pub fn new(verbose: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let lock_path = if nix::unistd::getuid().is_root() {
            PathBuf::from("/var/run/caffeinate2.lock")
        } else {
            PathBuf::from(format!("/tmp/caffeinate2_{}.lock", nix::unistd::getuid()))
        };

        Self::with_options(
            verbose,
            lock_path,
            Box::new(power_management::set_sleep_disabled),
            Box::new(default_process_checker),
        )
    }

    fn with_options(
        verbose: bool,
        lock_file_path: PathBuf,
        sleep_disabler: SleepDisabler,
        process_checker: Box<ProcessChecker>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let process_id = current_process_id()?;
        let should_disable = lockfile::update_lockfile(
            true,
            verbose,
            &lock_file_path,
            process_checker.as_ref(),
            &process_id,
        )?;

        if should_disable {
            if verbose {
                println!("First instance detected. Disabling system sleep globally.");
            }
            if let Err(code) = sleep_disabler(true, verbose) {
                let _ = lockfile::update_lockfile(
                    false,
                    verbose,
                    &lock_file_path,
                    process_checker.as_ref(),
                    &process_id,
                );
                return Err(std::io::Error::other(format!(
                    "Failed to disable sleep (IOKit error: {:X})",
                    code
                ))
                .into());
            }
        } else if verbose {
            println!("Other instances running. Sleep already disabled.");
        }

        Ok(Self {
            verbose,
            lock_file_path,
            sleep_disabler,
            process_checker,
            process_id,
        })
    }
}

fn current_process_id() -> Result<ProcessId, std::io::Error> {
    let pid = std::process::id() as i32;
    let start_time = get_process_start_time(pid)
        .ok_or_else(|| std::io::Error::other("Failed to determine current process start time"))?;
    Ok(ProcessId { pid, start_time })
}

fn default_process_checker(pid: i32, start_time: ProcessStartTime) -> bool {
    let is_alive = match kill(Pid::from_raw(pid), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true, // Assume alive on permission errors
    };

    if !is_alive {
        return false;
    }

    // Verify start time to prevent PID reuse issues
    match get_process_start_time(pid) {
        Some(actual_start_time) => actual_start_time == start_time,
        None => true,
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        match lockfile::update_lockfile(
            false,
            self.verbose,
            &self.lock_file_path,
            self.process_checker.as_ref(),
            &self.process_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_lock_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_wrapper_test_{}_{}.lock",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        path
    }

    #[test]
    fn test_process_checker_rejects_current_pid_with_wrong_start_time() {
        let current_pid = std::process::id() as i32;
        let current_start_time = get_process_start_time(current_pid).unwrap();
        let wrong_start_time = ProcessStartTime {
            seconds: current_start_time.seconds.saturating_add(1),
            microseconds: current_start_time.microseconds,
        };

        assert!(default_process_checker(current_pid, current_start_time));
        assert!(!default_process_checker(current_pid, wrong_start_time));
    }

    #[test]
    fn test_first_instance() {
        let lock_path = temp_lock_path();
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(|_pid: i32, _start_time: ProcessStartTime| false);

        let lock =
            ProcessLock::with_options(true, lock_path.clone(), sleep_disabler, process_checker)
                .unwrap();

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0]);

        drop(calls);
        drop(lock);

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[1]);

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn test_sleep_disable_failure_rolls_back_lockfile_entry() {
        let lock_path = temp_lock_path();

        let sleep_disabler = Box::new(|_state: bool, _verbose: bool| Err(0xE000_02C1));

        let process_checker = Box::new(|_pid: i32, _start_time: ProcessStartTime| false);

        let result =
            ProcessLock::with_options(false, lock_path.clone(), sleep_disabler, process_checker);

        assert!(result.is_err());
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.trim().is_empty());

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
