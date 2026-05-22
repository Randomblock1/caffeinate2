use crate::{
    lockfile::{self, ProcessChecker, ProcessId},
    power_management,
    process_util,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const HELPER_LOCK_PATH: &str = "/var/run/caffeinate2.lock";

pub type SleepDisabler = Box<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;

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

pub fn lockfile_holder_count(lock_path: &Path) -> Result<usize, std::io::Error> {
    let content = std::fs::read_to_string(lock_path).unwrap_or_default();
    Ok(content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| line.parse::<ProcessId>().ok())
        .count())
}

struct EntirelyCoordinatorInner {
    verbose: bool,
    lock_file_path: PathBuf,
    sleep_disabler: Arc<SleepDisabler>,
    process_checker: Arc<ProcessChecker>,
}

pub struct EntirelyCoordinator {
    inner: Arc<EntirelyCoordinatorInner>,
}

impl EntirelyCoordinator {
    pub fn with_options(
        verbose: bool,
        lock_file_path: PathBuf,
        sleep_disabler: Arc<SleepDisabler>,
        process_checker: Arc<ProcessChecker>,
    ) -> Self {
        Self {
            inner: Arc::new(EntirelyCoordinatorInner {
                verbose,
                lock_file_path,
                sleep_disabler,
                process_checker,
            }),
        }
    }

    pub fn helper_daemon(verbose: bool) -> Self {
        Self::with_options(
            verbose,
            helper_lock_path(),
            Arc::new(Box::new(power_management::set_sleep_disabled)),
            Arc::new(|pid, start| process_util::default_process_checker(pid, start)),
        )
    }

    pub fn cli_fallback(verbose: bool) -> Self {
        Self::with_options(
            verbose,
            cli_fallback_lock_path(),
            Arc::new(Box::new(power_management::set_sleep_disabled)),
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
        } else if inner.verbose {
            eprintln!("Other holders still running. Keeping sleep disabled.");
        }

        Ok(())
    }

    pub fn hold_current_process(
        &self,
    ) -> Result<EntirelyHoldGuard, Box<dyn std::error::Error + Send + Sync>> {
        let process_id = process_util::process_id_from_pid(std::process::id() as i32)?;
        self.hold(process_id)?;
        Ok(EntirelyHoldGuard {
            coordinator: Arc::clone(&self.inner),
            process_id,
            active: true,
        })
    }

    pub fn holder_count(&self) -> Result<usize, std::io::Error> {
        lockfile_holder_count(&self.inner.lock_file_path)
    }
}

pub struct EntirelyHoldGuard {
    coordinator: Arc<EntirelyCoordinatorInner>,
    process_id: ProcessId,
    active: bool,
}

impl EntirelyHoldGuard {
    pub fn release(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.active {
            return Ok(());
        }
        self.active = false;

        let inner = &self.coordinator;
        let should_enable = lockfile::update_lockfile(
            false,
            inner.verbose,
            &inner.lock_file_path,
            inner.process_checker.as_ref(),
            &self.process_id,
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
        } else if inner.verbose {
            eprintln!("Other holders still running. Keeping sleep disabled.");
        }

        Ok(())
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
