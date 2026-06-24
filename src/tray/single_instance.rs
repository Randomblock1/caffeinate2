//! Ensures only one tray process holds the menu-bar icon at a time.

use crate::tray::tray_mode;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::unistd::{ftruncate, write};
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;

static INSTANCE_LOCK: OnceLock<Flock<File>> = OnceLock::new();

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
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
            eprintln!("caffeinate2-tray: could not create {}: {error}", parent.display());
        std::process::exit(1);
    }

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(&path)
    {
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
    if let Err(error) = write(flock.as_fd(), line.as_bytes()) {
        eprintln!(
            "caffeinate2-tray: could not write lock file {}: {error}",
            path.display()
        );
        std::process::exit(1);
    }

    if INSTANCE_LOCK.set(flock).is_err() {
        eprintln!("caffeinate2-tray: single-instance lock already initialized");
        std::process::exit(1);
    }
}
