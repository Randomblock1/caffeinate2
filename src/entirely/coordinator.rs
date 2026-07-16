use crate::entirely::{
    error::CoordinatorError,
    lockfile::{self, ProcessChecker, ProcessId},
    process_util,
};
use crate::sleep::power_management;
use std::path::PathBuf;
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
    /// Serializes hold/release/reconcile so concurrent connections can't
    /// interleave their lockfile reads with each other's sleep toggles.
    ///
    /// Whether caffeinate2 owns the current sleep disable is *not* cached here:
    /// it lives durably in the lockfile (the ownership marker) and is read back
    /// under the lock on every operation. A cached flag cannot survive a helper
    /// restart, which is exactly when the intent must be recovered.
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
            tracing::warn!("startup reconcile failed: {e}");
        }
        // The CLI fallback has no long-lived daemon, so it runs a best-effort
        // in-process reaper while this invocation is alive. If this root CLI is
        // SIGKILLed and no future entirely-mode invocation runs, its lockfile
        // entry and SleepDisabled=true can persist; install the helper to avoid
        // that inherent CLI-only limit.
        //
        // Spawn that reaper at most once per process. `cli_fallback` can be
        // invoked repeatedly, and a fresh non-stoppable loop per call would
        // leak a thread each time. The reaper only reconciles the shared
        // lockfile, so a single thread bound to the first coordinator suffices.
        static REAPER_STARTED: std::sync::Once = std::sync::Once::new();
        let reaper = coordinator.clone();
        REAPER_STARTED.call_once(move || {
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(CLI_FALLBACK_RECONCILE_INTERVAL);
                    if let Err(e) = reaper.reconcile() {
                        tracing::warn!("periodic reconcile failed: {e}");
                    }
                }
            });
        });
        coordinator
    }

    /// Lock the operations mutex, recovering from poisoning instead of
    /// panicking. A worker thread that panicked mid-operation must not take
    /// down every future hold/release on the helper; the lockfile is re-read
    /// under this lock on each operation, so stale in-memory state can't
    /// corrupt the on-disk source of truth.
    fn lock_ops(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner.ops.lock().unwrap_or_else(|e| e.into_inner())
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be updated or sleep cannot be disabled.
    pub fn hold(&self, process_id: ProcessId) -> Result<(), CoordinatorError> {
        let inner = &self.inner;
        let _ops = self.lock_ops();
        // `acquire` records ownership of the disable in the same write that adds
        // the holder, before we actually toggle sleep below.
        let lockfile::AcquireOutcome {
            first_holder: should_disable,
            prior_owns_disable,
            disable_generation,
        } = lockfile::acquire(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
            &process_id,
        )?;

        if should_disable {
            if inner.verbose {
                tracing::info!("First holder detected. Disabling system sleep globally.");
            }
            if let Err(code) = (inner.sleep_disabler)(true, inner.verbose) {
                // The disable failed, so we don't actually own it: back out the
                // holder entry, and clear the ownership marker only if this
                // acquire created it (and it is still the generation this
                // acquire wrote). A pre-existing marker (left by an earlier
                // failed re-enable) records a disable that is still in effect,
                // and clearing it would strand that disable with nothing for
                // reconcile to act on.
                let _ = lockfile::release(
                    inner.verbose,
                    &inner.lock_file_path,
                    inner.process_checker.as_ref(),
                    &process_id,
                );
                if !prior_owns_disable {
                    let _ = lockfile::clear_owns_disable_if_current(
                        inner.verbose,
                        &inner.lock_file_path,
                        inner.process_checker.as_ref(),
                        disable_generation,
                    );
                }
                return Err(CoordinatorError::DisableSleepFailed { code });
            }
        } else if inner.verbose {
            tracing::info!("Other holders running. Sleep already disabled.");
        }

        Ok(())
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be updated or sleep cannot be re-enabled.
    pub fn release(&self, process_id: ProcessId) -> Result<(), CoordinatorError> {
        let inner = &self.inner;
        let _ops = self.lock_ops();
        // `release` removes the holder but leaves the ownership marker set; it
        // reports whether no live holders remain while we still own the disable.
        let lockfile::ReleaseOutcome {
            should_enable,
            removed,
            disable_generation,
        } = lockfile::release(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
            &process_id,
        )?;

        if should_enable {
            if inner.verbose {
                tracing::info!("Last holder released. Re-enabling system sleep globally.");
            }
            if let Err(code) = (inner.sleep_disabler)(false, inner.verbose) {
                // Restore the holder entry when re-enabling sleep fails so we
                // don't drop the last holder while SleepDisabled stays on — but
                // only when this call actually removed one. Re-adding a caller
                // that never held (a stray Release that merely pruned dead
                // entries to empty) would fabricate a live hold and make
                // reconcile keep sleep disabled instead of retrying the
                // re-enable. The marker is still set either way, so the state
                // remains recoverable.
                if removed {
                    let _ = lockfile::acquire(
                        inner.verbose,
                        &inner.lock_file_path,
                        inner.process_checker.as_ref(),
                        &process_id,
                    );
                }
                return Err(CoordinatorError::EnableSleepFailed { code });
            }
            // Only now that sleep is confirmed re-enabled do we clear ownership.
            // A crash before this point leaves the marker set, so a reconcile
            // re-enables (idempotently) instead of stranding sleep disabled.
            // The clear re-checks holders and the ownership generation under
            // the flock: in the CLI fallback another process can take a first
            // hold — or a concurrent reconcile can re-record ownership —
            // between the toggle above and this write, and that fresh marker
            // must survive.
            lockfile::clear_owns_disable_if_current(
                inner.verbose,
                &inner.lock_file_path,
                inner.process_checker.as_ref(),
                disable_generation,
            )?;
        } else if inner.verbose {
            tracing::info!("Other holders still running. Keeping sleep disabled.");
        }

        Ok(())
    }

    /// Re-sync the global sleep setting with the lockfile after a (re)start.
    ///
    /// Prunes dead holders, then re-disables sleep if live holders remain, so a
    /// helper crash can't leave active holders ineffective. It does not force
    /// the enable direction on an empty lockfile because that is
    /// indistinguishable from a manual `pmset disablesleep` made outside of
    /// caffeinate2.
    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be pruned or sleep cannot be reconciled.
    pub fn reconcile_startup(&self) -> Result<(), CoordinatorError> {
        let _ops = self.lock_ops();
        // At (re)start there is no in-memory state, so intent must come from the
        // lockfile: the durable ownership marker says whether caffeinate2 had
        // disabled sleep. `treat_pruned_as_intent` additionally treats "the
        // lockfile still had holders we just pruned to zero" as intent — a
        // legacy fallback for lockfiles written before the marker existed.
        self.reconcile_locked(true).map(|_| ())
    }

    /// Periodic reaper: prune holders whose processes have died and converge
    /// the sleep setting to `holders > 0`.
    ///
    /// When live holders remain it always re-applies the disable (the IOKit set
    /// is idempotent), so an external `pmset enablesleep` (or a wake from a
    /// sleep cycle) made while holds are active is corrected on the next pass
    /// instead of being trusted away by cached state. It still never
    /// force-enables on an empty lockfile *that caffeinate2 does not own*, so it
    /// won't fight a manual `pmset disablesleep` made outside of any holds.
    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be pruned or sleep cannot be reconciled.
    pub fn reconcile(&self) -> Result<(), CoordinatorError> {
        let _ops = self.lock_ops();
        self.reconcile_locked(false).map(|_| ())
    }

    /// Prune dead holders and converge the system sleep setting, returning the
    /// live holder count. The caller must already hold the ops lock.
    ///
    /// The durable ownership marker (`owns_disable`) is the source of truth for
    /// whether caffeinate2 disabled sleep, so it survives a helper restart.
    /// `treat_pruned_as_intent` additionally re-enables when pruning empties a
    /// lockfile that *had* holders — a legacy fallback (used only at startup)
    /// for lockfiles written before the marker existed. Neither path
    /// force-enables an empty, unmarked lockfile, so a manual `pmset
    /// disablesleep` is left untouched.
    fn reconcile_locked(&self, treat_pruned_as_intent: bool) -> Result<usize, CoordinatorError> {
        let inner = &self.inner;
        let lockfile::PruneOutcome {
            live: holders,
            had_entries,
            owns_disable,
            disable_generation,
        } = lockfile::prune_lockfile(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
        )?;
        if holders > 0 {
            // Live holders remain: always re-apply the disable. The
            // SleepDisabler abstraction is write-only, so we can't read the
            // real SleepDisabled state to compare against; trusting cached
            // intent would silently leave holders ineffective if something
            // external (manual `pmset enablesleep`, another tool, a sleep/wake
            // cycle) re-enabled sleep. The IOKit set is idempotent and this
            // runs at most once per reconcile interval, so re-applying is cheap.
            if inner.verbose {
                tracing::info!("Reconcile: {holders} live holder(s); disabling system sleep.");
            }
            if let Err(code) = (inner.sleep_disabler)(true, inner.verbose) {
                return Err(CoordinatorError::ReconcileSleepFailed { code });
            }
            // Re-record ownership even when the prune saw the marker set: in
            // the CLI fallback another process can complete a release —
            // re-enabling sleep and clearing the marker — between our prune and
            // the toggle above, and the disable we just re-applied would
            // otherwise be owned by nobody, stranding sleep disabled once the
            // holders drain. (This also covers legacy lockfiles and holders
            // that appeared without us having toggled sleep yet.)
            lockfile::set_owns_disable(
                inner.verbose,
                &inner.lock_file_path,
                inner.process_checker.as_ref(),
                true,
            )?;
        } else if owns_disable || (treat_pruned_as_intent && had_entries) {
            // No live holders, but caffeinate2 owns the disable (durable marker),
            // or — as a startup-only fallback — the lockfile held holders we just
            // pruned to zero. Either way, re-enable sleep to converge. An empty,
            // unmarked lockfile never reaches here, so a manual `pmset
            // disablesleep` made outside caffeinate2 is left untouched.
            if inner.verbose {
                tracing::info!("Reconcile: no live holders; re-enabling system sleep.");
            }
            if let Err(code) = (inner.sleep_disabler)(false, inner.verbose) {
                return Err(CoordinatorError::ReconcileSleepFailed { code });
            }
            // Clear ownership only after the re-enable succeeded, and only if
            // the marker is still the one the prune above observed: a first
            // hold or an ownership re-record by another process sharing the
            // lockfile may have set a fresh marker since, and clobbering it
            // would strand that disable.
            if owns_disable {
                lockfile::clear_owns_disable_if_current(
                    inner.verbose,
                    &inner.lock_file_path,
                    inner.process_checker.as_ref(),
                    disable_generation,
                )?;
            }
        }
        Ok(holders)
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the lockfile cannot be read or pruned, or if
    /// converging the sleep setting after pruning fails.
    pub fn status(&self) -> Result<EntirelyStatus, CoordinatorError> {
        let inner = &self.inner;
        let _ops = self.lock_ops();
        let lockfile::PruneOutcome {
            live: holders,
            owns_disable,
            disable_generation,
            ..
        } = lockfile::prune_lockfile(
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
        )?;
        let mut owns_disable = owns_disable;
        if holders == 0 && owns_disable {
            // Pruning just dropped the last holder while we owned the disable.
            // Converge immediately by re-enabling sleep instead of waiting up to
            // a full reconcile interval for the periodic reaper to notice.
            if inner.verbose {
                tracing::info!("Status: last holder pruned; re-enabling system sleep.");
            }
            if let Err(code) = (inner.sleep_disabler)(false, inner.verbose) {
                return Err(CoordinatorError::EnableSleepFailed { code });
            }
            // As in release()/reconcile: clear only if the marker is still the
            // one the prune observed, so a fresh marker set by another process
            // since then survives.
            lockfile::clear_owns_disable_if_current(
                inner.verbose,
                &inner.lock_file_path,
                inner.process_checker.as_ref(),
                disable_generation,
            )?;
            owns_disable = false;
        }
        Ok(EntirelyStatus {
            holders,
            // Derived from the durable ownership marker, not a fresh read of the
            // kernel's SleepDisabled setting (the SleepDisabler abstraction is
            // write-only). Because the periodic reconcile re-applies the disable
            // while holders remain, this stays aligned with reality in the steady
            // state; a transient external change can briefly make it stale until
            // the next reconcile.
            sleep_disabled: holders > 0 || owns_disable,
        })
    }

    ///
    /// # Errors
    ///
    /// Returns an error if the hold cannot be acquired for the current process.
    pub fn hold_current_process(&self) -> Result<EntirelyHoldGuard, CoordinatorError> {
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
    pub fn release(&mut self) -> Result<(), CoordinatorError> {
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
            tracing::warn!("Error releasing entirely hold: {e}");
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

    /// Lockfile lines recording holders (parseable `ProcessId`s). The persisted
    /// ownership-generation sentinel legitimately outlives holders and marker,
    /// so "rolled back" means no holder lines rather than an empty file.
    fn holder_lines(lock_path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(lock_path)
            .unwrap()
            .lines()
            .filter(|line| {
                line.trim()
                    .parse::<crate::entirely::lockfile::ProcessId>()
                    .is_ok()
            })
            .map(str::to_string)
            .collect()
    }

    fn owns_disable(lock_path: &std::path::Path) -> bool {
        std::fs::read_to_string(lock_path)
            .unwrap()
            .lines()
            .any(|line| line.trim() == "!disabled")
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
    fn reconcile_startup_reenables_after_pruning_dead_holders() {
        // A helper that crashed while holding the disable leaves a stale holder
        // in the lockfile. On restart the fresh coordinator must notice the
        // lockfile *had* a holder, prune the dead entry, and re-enable sleep —
        // even though its cached flag starts false.
        let lock_path = temp_lock_path();
        let stale = ProcessId {
            pid: 999_999,
            start_time: ProcessStartTime {
                seconds: 7,
                microseconds: 0,
            },
        };
        std::fs::write(&lock_path, format!("{stale}\n")).unwrap();

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });
        // Every holder is dead, so pruning empties the lockfile.
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| false);
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        coordinator.reconcile_startup().unwrap();

        assert_eq!(*sleep_calls.lock().unwrap(), vec![false]);
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
    fn reconcile_startup_reenables_from_owned_marker_without_holders() {
        // Models a crash between release()'s holder-removal write and its
        // re-enable: the lockfile carries the ownership marker but zero holders,
        // and sleep is still disabled. A restart must re-enable and clear the
        // marker — driven purely by the marker, since `had_entries` is false.
        let lock_path = temp_lock_path();
        let _ = std::fs::remove_file(&lock_path);
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| false);
        // Create the marker-only state (no holders).
        lockfile::set_owns_disable(false, &lock_path, process_checker.as_ref(), true).unwrap();

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        coordinator.reconcile_startup().unwrap();
        assert_eq!(*sleep_calls.lock().unwrap(), vec![false]);

        // The marker was cleared, so a second reconcile is a no-op.
        coordinator.reconcile().unwrap();
        assert_eq!(*sleep_calls.lock().unwrap(), vec![false]);

        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn periodic_reconcile_honors_owned_marker_without_holders() {
        // The periodic reaper (treat_pruned_as_intent = false) must also honor
        // the durable marker: no live holders while we own the disable =>
        // re-enable. This is the path that self-heals the found bug even without
        // the immediate re-enable in release().
        let lock_path = temp_lock_path();
        let _ = std::fs::remove_file(&lock_path);
        let process_checker: Arc<ProcessChecker> = Arc::new(|_, _| false);
        lockfile::set_owns_disable(false, &lock_path, process_checker.as_ref(), true).unwrap();

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();
        let sleep_disabler: SleepDisabler = Arc::new(move |state, _verbose| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });
        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        coordinator.reconcile().unwrap();
        assert_eq!(*sleep_calls.lock().unwrap(), vec![false]);

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
            !holder_lines(&lock_path).is_empty(),
            "holder should remain in lockfile after failed re-enable"
        );
        drop(guard);

        assert_eq!(release_attempts.load(Ordering::Relaxed), 2);
        assert_eq!(*sleep_calls.lock().unwrap(), vec![true, false, false]);
        assert!(holder_lines(&lock_path).is_empty());
        assert!(!owns_disable(&lock_path));
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
        assert!(holder_lines(&lock_path).is_empty());
        assert!(!owns_disable(&lock_path));

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
