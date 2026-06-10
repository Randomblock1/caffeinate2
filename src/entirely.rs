use crate::{
    lockfile::{self, ProcessChecker, ProcessId},
    power_management,
    process_util,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const HELPER_LOCK_PATH: &str = "/var/run/caffeinate2.lock";

pub type SleepDisabler = Arc<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;

pub fn helper_lock_path() -> PathBuf {
    PathBuf::from(HELPER_LOCK_PATH)
}

pub fn cli_fallback_lock_path() -> PathBuf {
    if nix::unistd::getuid().is_root() {
        helper_lock_path()
    } else {
        PathBuf::from(format!(
            "/tmp/caffeinate2_{}.lock",
            nix::unistd::getuid()
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntirelyStatus {
    pub holders: usize,
    pub sleep_disabled: bool,
}

struct EntirelyCoordinatorInner {
    verbose: bool,
    lock_file_path: PathBuf,
    sleep_disabler: SleepDisabler,
    process_checker: Arc<ProcessChecker>,
    sleep_disabled: AtomicBool,
}

#[derive(Clone)]
pub struct EntirelyCoordinator {
    inner: Arc<EntirelyCoordinatorInner>,
}

impl EntirelyCoordinator {
    pub fn with_options(
        verbose: bool,
        lock_file_path: PathBuf,
        sleep_disabler: SleepDisabler,
        process_checker: Arc<ProcessChecker>,
    ) -> Self {
        Self {
            inner: Arc::new(EntirelyCoordinatorInner {
                verbose,
                lock_file_path,
                sleep_disabler,
                process_checker,
                sleep_disabled: AtomicBool::new(false),
            }),
        }
    }

    pub fn helper_daemon(verbose: bool) -> Self {
        Self::with_options(
            verbose,
            helper_lock_path(),
            Arc::new(power_management::set_sleep_disabled),
            Arc::new(|pid, start| process_util::default_process_checker(pid, start)),
        )
    }

    pub fn cli_fallback(verbose: bool) -> Self {
        Self::with_options(
            verbose,
            cli_fallback_lock_path(),
            Arc::new(power_management::set_sleep_disabled),
            Arc::new(|pid, start| process_util::default_process_checker(pid, start)),
        )
    }

    pub fn hold(
        &self,
        process_id: ProcessId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let should_disable = lockfile::update_lockfile(
            true,
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
            &process_id,
        )?;

        if should_disable {
            if inner.verbose {
                eprintln!("First holder detected. Disabling system sleep globally.");
            }
            if let Err(code) = (inner.sleep_disabler)(true, inner.verbose) {
                let _ = lockfile::update_lockfile(
                    false,
                    inner.verbose,
                    &inner.lock_file_path,
                    inner.process_checker.as_ref(),
                    &process_id,
                );
                return Err(std::io::Error::other(format!(
                    "Failed to disable sleep (IOKit error: {code:X})"
                ))
                .into());
            }
            inner.sleep_disabled.store(true, Ordering::SeqCst);
        } else if inner.verbose {
            eprintln!("Other holders running. Sleep already disabled.");
        }

        Ok(())
    }

    pub fn release(
        &self,
        process_id: ProcessId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let should_enable = lockfile::update_lockfile(
            false,
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
            &process_id,
        )?;

        if should_enable {
            if inner.verbose {
                eprintln!("Last holder released. Re-enabling system sleep globally.");
            }
            if let Err(code) = (inner.sleep_disabler)(false, inner.verbose) {
                return Err(std::io::Error::other(format!(
                    "Failed to re-enable sleep (IOKit error: {code:X})"
                ))
                .into());
            }
            inner.sleep_disabled.store(false, Ordering::SeqCst);
        } else if inner.verbose {
            eprintln!("Other holders still running. Keeping sleep disabled.");
        }

        Ok(())
    }

    pub fn status(
        &self,
    ) -> Result<EntirelyStatus, Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let holders = lockfile::prune_lockfile(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
        )?;
        Ok(EntirelyStatus {
            holders,
            sleep_disabled: inner.sleep_disabled.load(Ordering::SeqCst),
        })
    }

    pub fn hold_current_process(
        &self,
    ) -> Result<EntirelyHoldGuard, Box<dyn std::error::Error + Send + Sync>> {
        let process_id = process_util::process_id_from_pid(std::process::id() as i32)?;
        self.hold(process_id)?;
        Ok(EntirelyHoldGuard {
            coordinator: self.clone(),
            process_id,
            active: true,
        })
    }
}

pub struct EntirelyHoldGuard {
    coordinator: EntirelyCoordinator,
    process_id: ProcessId,
    active: bool,
}

impl EntirelyHoldGuard {
    pub fn release(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.coordinator.release(self.process_id)
    }
}

impl Drop for EntirelyHoldGuard {
    fn drop(&mut self) {
        if self.active {
            if let Err(e) = self.release() {
                eprintln!("Error releasing entirely hold: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{ProcessChecker, ProcessStartTime};
    use crate::process_util;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_lock_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_entirely_test_{}_{}.lock",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        path
    }

    #[test]
    fn process_checker_rejects_current_pid_with_wrong_start_time() {
        let current_pid = std::process::id() as i32;
        let current_start_time = process_util::get_process_start_time(current_pid).unwrap();
        let wrong_start_time = ProcessStartTime {
            seconds: current_start_time.seconds.saturating_add(1),
            microseconds: current_start_time.microseconds,
        };

        assert!(process_util::default_process_checker(
            current_pid,
            current_start_time
        ));
        assert!(!process_util::default_process_checker(
            current_pid,
            wrong_start_time
        ));
    }

    #[test]
    fn first_holder_disables_sleep_once() {
        let lock_path = temp_lock_path();
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler: SleepDisabler = Arc::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker: Arc<ProcessChecker> =
            Arc::new(|_pid: i32, _start: ProcessStartTime| false);

        let coordinator = EntirelyCoordinator::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        let guard = coordinator.hold_current_process().unwrap();

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0]);

        drop(calls);
        drop(guard);

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[1]);

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn sleep_disable_failure_rolls_back_lockfile_entry() {
        let lock_path = temp_lock_path();

        let sleep_disabler: SleepDisabler =
            Arc::new(|_state: bool, _verbose: bool| Err(0xE000_02C1u32));

        let process_checker: Arc<ProcessChecker> =
            Arc::new(|_pid: i32, _start: ProcessStartTime| false);

        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        let result = coordinator.hold_current_process();

        assert!(result.is_err());
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.trim().is_empty());

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
