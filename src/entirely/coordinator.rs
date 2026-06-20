use crate::entirely::{
    lockfile::{self, ProcessChecker, ProcessId},
    process_util,
};
use crate::sleep::power_management;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const HELPER_LOCK_PATH: &str = "/var/run/caffeinate2.lock";

pub type SleepDisabler = Arc<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;

#[must_use]
pub fn helper_lock_path() -> PathBuf {
    PathBuf::from(HELPER_LOCK_PATH)
}

const CLI_FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

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
    /// Serializes hold/release/reconcile so concurrent connections can't
    /// interleave their lockfile reads with each other's sleep toggles.
    ops: Mutex<()>,
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
                ops: Mutex::new(()),
            }),
        }
    }

    pub fn helper_daemon(verbose: bool) -> Self {
        Self::with_options(
            verbose,
            helper_lock_path(),
            Arc::new(power_management::set_sleep_disabled),
            Arc::new(process_util::default_process_checker),
        )
    }

    pub fn cli_fallback(verbose: bool) -> Self {
        let coordinator = Self::with_options(
            verbose,
            helper_lock_path(),
            Arc::new(power_management::set_sleep_disabled),
            Arc::new(process_util::default_process_checker),
        );
        if let Err(e) = coordinator.reconcile_startup() {
            eprintln!("startup reconcile failed: {e}");
        }
        // The CLI fallback has no long-lived daemon, so it runs a best-effort
        // in-process reaper while this invocation is alive. If this root CLI is
        // SIGKILLed and no future entirely-mode invocation runs, its lockfile
        // entry and SleepDisabled=true can persist; install the helper to avoid
        // that inherent CLI-only limit.
        let reaper = coordinator.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(CLI_FALLBACK_RECONCILE_INTERVAL);
                if let Err(e) = reaper.reconcile() {
                    eprintln!("periodic reconcile failed: {e}");
                }
            }
        });
        coordinator
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be updated or sleep cannot be disabled.
    ///
    /// # Panics
    ///
    /// Panics if the internal operations mutex is poisoned.
    pub fn hold(
        &self,
        process_id: ProcessId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let _ops = inner.ops.lock().expect("ops lock");
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

    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be updated or sleep cannot be re-enabled.
    ///
    /// # Panics
    ///
    /// Panics if the internal operations mutex is poisoned.
    pub fn release(
        &self,
        process_id: ProcessId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let _ops = inner.ops.lock().expect("ops lock");
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
                // Mirror hold(): restore the lockfile entry when re-enabling sleep
                // fails so we don't drop the last holder while SleepDisabled stays on.
                let _ = lockfile::update_lockfile(
                    true,
                    inner.verbose,
                    &inner.lock_file_path,
                    inner.process_checker.as_ref(),
                    &process_id,
                );
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

    /// Re-sync the global sleep setting with the lockfile after a (re)start.
    ///
    /// Prunes dead holders, then force-disables sleep if live holders remain,
    /// so a helper crash can't leave active holders ineffective. It does not
    /// force the enable direction on an empty lockfile because that is
    /// indistinguishable from a manual `pmset disablesleep` made outside of
    /// caffeinate2.
    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be pruned or sleep cannot be reconciled.
    pub fn reconcile_startup(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.reconcile_with(true)
    }

    /// Periodic reaper: prune holders whose processes have died and converge
    /// the sleep setting to `holders > 0`.
    ///
    /// Only acts when the desired state differs from the state this
    /// coordinator last applied, so it doesn't fight a manual
    /// `pmset disablesleep` made outside of any holds.
    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be pruned or sleep cannot be reconciled.
    pub fn reconcile(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.reconcile_with(false)
    }

    fn reconcile_with(&self, force: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let _ops = inner.ops.lock().expect("ops lock");
        let holders = lockfile::prune_lockfile(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
        )?;
        let disable = holders > 0;
        let force_apply = force && disable;
        if !force_apply && disable == inner.sleep_disabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        if inner.verbose {
            eprintln!(
                "Reconcile: {holders} live holder(s); {} system sleep.",
                if disable { "disabling" } else { "enabling" }
            );
        }
        if let Err(code) = (inner.sleep_disabler)(disable, inner.verbose) {
            return Err(std::io::Error::other(format!(
                "Failed to reconcile sleep state (IOKit error: {code:X})"
            ))
            .into());
        }
        inner.sleep_disabled.store(disable, Ordering::SeqCst);
        Ok(())
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be read or pruned.
    ///
    /// # Panics
    ///
    /// Panics if the internal operations mutex is poisoned.
    pub fn status(&self) -> Result<EntirelyStatus, Box<dyn std::error::Error + Send + Sync>> {
        let inner = &self.inner;
        let _ops = inner.ops.lock().expect("ops lock");
        let holders = lockfile::prune_lockfile(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
        )?;
        Ok(EntirelyStatus {
            holders,
            sleep_disabled: holders > 0 || inner.sleep_disabled.load(Ordering::SeqCst),
        })
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the hold cannot be acquired for the current process.
    pub fn hold_current_process(
        &self,
    ) -> Result<EntirelyHoldGuard, Box<dyn std::error::Error + Send + Sync>> {
        let process_id = process_util::process_id_from_pid(std::process::id().cast_signed())?;
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
    ///
    /// # Errors
    ///
    /// Returns an error if the hold cannot be released.
    pub fn release(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.active {
            return Ok(());
        }
        self.coordinator.release(self.process_id)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for EntirelyHoldGuard {
    fn drop(&mut self) {
        if self.active
            && let Err(e) = self.release()
        {
            eprintln!("Error releasing entirely hold: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entirely::lockfile::{ProcessChecker, ProcessStartTime};
    use crate::entirely::process_util;
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
        let current_pid = std::process::id().cast_signed();
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

        {
            let calls = sleep_calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(calls[0]);
            drop(calls);
        }
        drop(guard);

        {
            let calls = sleep_calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert!(!calls[1]);
            drop(calls);
        }

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn reconcile_startup_does_not_force_enable_empty_lockfile() {
        let lock_path = temp_lock_path();
        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| false);
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        coordinator.reconcile_startup().unwrap();

        assert!(sleep_calls.lock().unwrap().is_empty());
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn reconcile_startup_force_disables_with_live_holders() {
        let lock_path = temp_lock_path();
        let holder = ProcessId {
            pid: 42,
            start_time: ProcessStartTime {
                seconds: 7,
                microseconds: 0,
            },
        };
        std::fs::write(&lock_path, format!("{holder}\n")).unwrap();

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| true);
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        coordinator.reconcile_startup().unwrap();

        assert_eq!(*sleep_calls.lock().unwrap(), vec![true]);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn failed_release_is_retried_on_drop() {
        let lock_path = temp_lock_path();
        let release_attempts = Arc::new(AtomicU64::new(0));
        let release_attempts_clone = release_attempts.clone();
        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            if !state && release_attempts_clone.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(0xE000_02C1u32);
            }
            Ok(())
        });
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| false);
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        let mut guard = coordinator.hold_current_process().unwrap();
        assert!(guard.release().is_err());
        drop(guard);

        assert_eq!(release_attempts.load(Ordering::Relaxed), 2);
        assert_eq!(*sleep_calls.lock().unwrap(), vec![true, false, false]);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn release_enable_failure_restores_lockfile_entry() {
        let lock_path = temp_lock_path();
        let release_attempts = Arc::new(AtomicU64::new(0));
        let release_attempts_clone = release_attempts.clone();
        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            if !state && release_attempts_clone.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(0xE000_02C1u32);
            }
            Ok(())
        });
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| false);
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        let mut guard = coordinator.hold_current_process().unwrap();
        assert!(guard.release().is_err());
        assert!(
            !std::fs::read_to_string(&lock_path)
                .unwrap()
                .trim()
                .is_empty(),
            "holder should remain in lockfile after failed re-enable"
        );
        drop(guard);

        assert_eq!(release_attempts.load(Ordering::Relaxed), 2);
        assert_eq!(*sleep_calls.lock().unwrap(), vec![true, false, false]);
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.trim().is_empty());
        let _ = std::fs::remove_file(&lock_path);
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
