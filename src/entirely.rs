use crate::{
    lockfile::{self, ProcessChecker, ProcessId},
    power_management,
    process_util,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const HELPER_LOCK_PATH: &str = "/var/run/caffeinate2.lock";

pub fn helper_lock_path() -> PathBuf {
    PathBuf::from(HELPER_LOCK_PATH)
}

pub struct EntirelyCoordinator {
    verbose: bool,
    lock_file_path: PathBuf,
    process_checker: Arc<ProcessChecker>,
}

impl EntirelyCoordinator {
    pub fn new(verbose: bool, lock_file_path: PathBuf, process_checker: Arc<ProcessChecker>) -> Self {
        Self {
            verbose,
            lock_file_path,
            process_checker,
        }
    }

    pub fn helper_daemon(verbose: bool) -> Self {
        Self::new(
            verbose,
            helper_lock_path(),
            Arc::new(|pid, start| process_util::default_process_checker(pid, start)),
        )
    }

    pub fn hold(&self, process_id: ProcessId) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let should_disable = lockfile::update_lockfile(
            true,
            self.verbose,
            &self.lock_file_path,
            self.process_checker.as_ref(),
            &process_id,
        )?;

        if should_disable {
            if self.verbose {
                eprintln!("First holder detected. Disabling system sleep globally.");
            }
            if let Err(code) = power_management::set_sleep_disabled(true, self.verbose) {
                let _ = lockfile::update_lockfile(
                    false,
                    self.verbose,
                    &self.lock_file_path,
                    self.process_checker.as_ref(),
                    &process_id,
                );
                return Err(std::io::Error::other(format!(
                    "Failed to disable sleep (IOKit error: {:X})",
                    code
                ))
                .into());
            }
        } else if self.verbose {
            eprintln!("Other holders running. Sleep already disabled.");
        }

        Ok(())
    }

    pub fn release(&self, process_id: ProcessId) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let should_enable = lockfile::update_lockfile(
            false,
            self.verbose,
            &self.lock_file_path,
            self.process_checker.as_ref(),
            &process_id,
        )?;

        if should_enable {
            if self.verbose {
                eprintln!("Last holder released. Re-enabling system sleep globally.");
            }
            if let Err(code) = power_management::set_sleep_disabled(false, self.verbose) {
                return Err(std::io::Error::other(format!(
                    "Failed to re-enable sleep (IOKit error: {:X})",
                    code
                ))
                .into());
            }
        } else if self.verbose {
            eprintln!("Other holders still running. Keeping sleep disabled.");
        }

        Ok(())
    }

    pub fn holder_count(&self) -> Result<usize, std::io::Error> {
        EntirelySession::lockfile_holder_count(&self.lock_file_path)
    }
}

pub struct EntirelySession {
    verbose: bool,
    lock_file_path: PathBuf,
    process_checker: Arc<ProcessChecker>,
    process_id: ProcessId,
    active: bool,
}

impl EntirelySession {
    pub fn lockfile_holder_count(lock_path: &Path) -> Result<usize, std::io::Error> {
        let content = std::fs::read_to_string(lock_path).unwrap_or_default();
        Ok(content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| line.parse::<ProcessId>().ok())
            .count())
    }

    pub fn hold_for_current_process(
        verbose: bool,
        lock_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let process_id = process_util::process_id_from_pid(std::process::id() as i32)?;
        let checker = Arc::new(|pid, start| process_util::default_process_checker(pid, start));
        Self::hold_for_pid(verbose, lock_path, process_id, checker)
    }

    pub fn hold_for_pid(
        verbose: bool,
        lock_file_path: PathBuf,
        process_id: ProcessId,
        process_checker: Arc<ProcessChecker>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let should_disable = lockfile::update_lockfile(
            true,
            verbose,
            &lock_file_path,
            process_checker.as_ref(),
            &process_id,
        )?;

        if should_disable {
            if verbose {
                eprintln!("First instance detected. Disabling system sleep globally.");
            }
            if let Err(code) = power_management::set_sleep_disabled(true, verbose) {
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
            eprintln!("Other instances running. Sleep already disabled.");
        }

        Ok(Self {
            verbose,
            lock_file_path,
            process_checker,
            process_id,
            active: true,
        })
    }

    pub fn release(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.active {
            return Ok(());
        }
        self.active = false;

        match lockfile::update_lockfile(
            false,
            self.verbose,
            &self.lock_file_path,
            self.process_checker.as_ref(),
            &self.process_id,
        )? {
            true => {
                if self.verbose {
                    eprintln!("Last instance exiting. Re-enabling system sleep globally.");
                }
                if let Err(code) = power_management::set_sleep_disabled(false, self.verbose) {
                    return Err(std::io::Error::other(format!(
                        "Failed to re-enable sleep (IOKit error: {:X})",
                        code
                    ))
                    .into());
                }
            }
            false if self.verbose => {
                eprintln!("Other instances still running. Keeping sleep disabled.");
            }
            false => {}
        }
        Ok(())
    }
}

impl Drop for EntirelySession {
    fn drop(&mut self) {
        if self.active {
            if let Err(e) = self.release() {
                eprintln!("Error releasing entirely session: {e}");
            }
        }
    }
}
