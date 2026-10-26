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
#[derive(Debug)]
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
/// taking ownership or touching the lock state at all. Used by the detaching
/// parent so the user sees "already running" in their terminal (the background
/// child's own message goes only to its log).
///
/// Look-don't-touch on purpose: the previous implementation briefly *took* the
/// exclusive lock and dropped it, and a tray starting during that instant
/// (login item racing a manual launch) saw the lock held and quit spuriously.
/// `F_GETLK` only asks the kernel whether a lock would conflict — verified on
/// Darwin to observe another process's `flock` — so the probe can no longer
/// make anyone else's acquisition fail.
pub fn probe() -> InstanceProbe {
    let path = match lock_path() {
        Ok(path) => path,
        Err(error) => return InstanceProbe::Indeterminate(error.to_string()),
    };
    probe_path(&path)
}

fn probe_path(path: &Path) -> InstanceProbe {
    use std::os::fd::AsRawFd;

    let file = match open_lock_file(path) {
        Ok(file) => file,
        Err(error) => {
            return InstanceProbe::Indeterminate(format!(
                "could not open lock file {}: {error}",
                path.display()
            ));
        }
    };
    // SAFETY: zeroed is a valid flock value; the fd is open for the whole call.
    let mut query: libc::flock = unsafe { std::mem::zeroed() };
    query.l_type = libc::F_WRLCK;
    query.l_whence = libc::SEEK_SET as i16;
    // l_start/l_len stay 0: the whole file, matching what flock() covers.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &raw mut query) };
    if ret == -1 {
        let error = std::io::Error::last_os_error();
        return InstanceProbe::Indeterminate(format!(
            "could not query lock {}: {error}",
            path.display()
        ));
    }
    if query.l_type == libc::F_UNLCK {
        InstanceProbe::Available
    } else {
        InstanceProbe::Running
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_sees_a_held_lock_without_disturbing_it() {
        // Failures print the actual variant: Indeterminate carries the errno
        // detail, which matters on a test validating kernel behavior.
        #[track_caller]
        fn assert_probe(path: &Path, want_running: bool) {
            let got = probe_path(path);
            let ok = matches!(
                (&got, want_running),
                (InstanceProbe::Running, true) | (InstanceProbe::Available, false)
            );
            assert!(
                ok,
                "expected {}, got {got:?}",
                if want_running { "Running" } else { "Available" }
            );
        }

        let path = std::env::temp_dir().join(format!(
            "caffeinate2-probe-test-{}.lock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        assert_probe(&path, false);

        // A separate descriptor in the same process is a distinct flock owner,
        // and F_GETLK reports its lock (verified on Darwin).
        let file = open_lock_file(&path).unwrap();
        let held = Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| e)
            .unwrap();
        assert_probe(&path, true);
        // The probe must not have released the holder's lock.
        assert_probe(&path, true);

        drop(held);
        assert_probe(&path, false);
        let _ = std::fs::remove_file(&path);
    }
}
