use crate::{
    entirely::{EntirelyCoordinator, EntirelyHoldGuard},
    helper_ipc::HelperHoldGuard,
};

pub enum EntirelyGuard {
    Local(EntirelyHoldGuard),
    Helper(HelperHoldGuard),
}

/// Acquire entirely sleep prevention via helper or in-process lock.
pub fn acquire_entirely(verbose: bool) -> Result<EntirelyGuard, Box<dyn std::error::Error>> {
    let client = crate::helper_ipc::HelperClient::new();
    if client.is_available() {
        return Ok(EntirelyGuard::Helper(HelperHoldGuard::acquire().map_err(
            |e| -> Box<dyn std::error::Error> { e.into() },
        )?));
    }
    Ok(EntirelyGuard::Local(
        EntirelyCoordinator::cli_fallback(verbose).hold_current_process()?,
    ))
}

/// RAII entirely-mode lock for the current process (CLI fallback path).
pub struct ProcessLock(EntirelyHoldGuard);

impl ProcessLock {
    pub fn new(verbose: bool) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self(
            EntirelyCoordinator::cli_fallback(verbose).hold_current_process()?,
        ))
    }

    pub(crate) fn with_coordinator(
        coordinator: EntirelyCoordinator,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self(coordinator.hold_current_process()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entirely::EntirelyCoordinator;
    use crate::lockfile::{ProcessChecker, ProcessStartTime};
    use crate::process_util;
    use std::path::PathBuf;
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
        let current_start_time = process_util::get_process_start_time(current_pid).unwrap();
        let wrong_start_time = crate::lockfile::ProcessStartTime {
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
    fn test_first_instance() {
        let lock_path = temp_lock_path();
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Arc::new(Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        }) as crate::entirely::SleepDisabler);

        let process_checker: Arc<ProcessChecker> =
            Arc::new(|_pid: i32, _start: ProcessStartTime| false);

        let coordinator = EntirelyCoordinator::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        let lock = ProcessLock::with_coordinator(coordinator).unwrap();

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

        let sleep_disabler = Arc::new(Box::new(|_state: bool, _verbose: bool| Err(0xE000_02C1))
            as crate::entirely::SleepDisabler);

        let process_checker: Arc<ProcessChecker> =
            Arc::new(|_pid: i32, _start: ProcessStartTime| false);

        let coordinator = EntirelyCoordinator::with_options(
            false,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        );

        let result = ProcessLock::with_coordinator(coordinator);

        assert!(result.is_err());
        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.trim().is_empty());

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
