//! Ensures only one tray process holds the menu-bar icon at a time.

use crate::tray::tray_mode;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::unistd::{ftruncate, write};
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static INSTANCE_LOCK: OnceLock<Flock<File>> = OnceLock::new();

/// Result of a non-destructive single-instance check.
pub enum InstanceProbe {
    Available,
    Running,
    Indeterminate(String),
}

/// Opens (creating if needed) the single-instance lock file. Shared by
/// `acquire_or_exit` and `probe` so the path and permission choices live once.
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(path)
}

/// Check whether another tray instance holds the single-instance lock, without
/// taking ownership: the lock is released immediately, and the lock file's
/// contents (the owner's PID) are left untouched. Used by the detaching parent
/// before it spawns a background child whose stderr goes to /dev/null, where
/// the child's own "already running" message would be invisible.
pub fn probe() -> InstanceProbe {
    let path = match lock_path() {
        Ok(path) => path,
        Err(error) => return InstanceProbe::Indeterminate(error.to_string()),
    };
    let file = match open_lock_file(&path) {
        Ok(file) => file,
        Err(error) => {
            return InstanceProbe::Indeterminate(format!(
                "could not open lock file {}: {error}",
                path.display()
            ));
        }
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            // Nobody holds the lock; release it (drop unlocks and closes) so
            // the instance we are about to spawn can take it.
            drop(flock);
            InstanceProbe::Available
        }
        Err((_, Errno::EWOULDBLOCK)) => InstanceProbe::Running,
        Err((_, error)) => {
            InstanceProbe::Indeterminate(format!("could not lock {}: {error}", path.display()))
        }
    }
}

fn lock_path() -> Result<PathBuf, crate::tray::error::TrayError> {
    let config = tray_mode::config_path()?;
    Ok(config
        .parent()
        .map(|dir| dir.join("tray.lock"))
        .unwrap_or_else(|| PathBuf::from("tray.lock")))
}

/// Acquire the tray single-instance lock, or exit the process if another tray
/// is already running. The lock is held until process exit.
pub fn acquire_or_exit() {
    let path = match lock_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("caffeinate2-tray: {error}");
            std::process::exit(1);
        }
    };
    let file = match open_lock_file(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "caffeinate2-tray: could not open lock file {}: {error}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    let flock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => flock,
        Err((_, Errno::EWOULDBLOCK)) => {
            eprintln!("caffeinate2-tray: another instance is already running");
            std::process::exit(1);
        }
        Err((_, error)) => {
            eprintln!(
                "caffeinate2-tray: could not lock {}: {error}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    let _ = ftruncate(flock.as_fd(), 0);
    let line = format!("{}\n", std::process::id());
    match write(flock.as_fd(), line.as_bytes()) {
        Ok(written) if written == line.len() => {}
        Ok(written) => {
            // A short write would leave a truncated PID in the lock file, so
            // treat it as fatal rather than record a corrupt owner.
            eprintln!(
                "caffeinate2-tray: short write to lock file {} ({written} of {} bytes)",
                path.display(),
                line.len()
            );
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!(
                "caffeinate2-tray: could not write lock file {}: {error}",
                path.display()
            );
            std::process::exit(1);
        }
    }

    if INSTANCE_LOCK.set(flock).is_err() {
        eprintln!("caffeinate2-tray: single-instance lock already initialized");
        std::process::exit(1);
    }
}
