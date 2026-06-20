use libc::{S_IFMT, S_IFREG};
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::sys::stat::{Mode, fchmod, fstat, lstat};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) const LOCK_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ProcessStartTime {
    pub seconds: u64,
    pub microseconds: u64,
}

impl std::fmt::Display for ProcessStartTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.seconds, self.microseconds)
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ProcessId {
    pub pid: i32,
    pub start_time: ProcessStartTime,
}

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.pid, self.start_time)
    }
}

impl std::str::FromStr for ProcessId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split(':');
        let pid = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        if pid <= 0 {
            return Err(());
        }
        let seconds = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        let microseconds = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        if microseconds >= 1_000_000 {
            return Err(());
        }
        if parts.next().is_some() {
            return Err(());
        }
        let start_time = ProcessStartTime {
            seconds,
            microseconds,
        };
        Ok(Self { pid, start_time })
    }
}

pub type ProcessChecker = dyn Fn(i32, ProcessStartTime) -> bool + Send + Sync;

fn open_validated_lockfile(path: &Path) -> Result<Flock<File>, std::io::Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(LOCK_FILE_MODE)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(path)?;

    let file = match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(f) => f,
        Err((_, e)) => return Err(std::io::Error::other(e)),
    };

    let file_stat = fstat(file.as_fd()).map_err(std::io::Error::other)?;
    if (file_stat.st_mode & S_IFMT) != S_IFREG {
        return Err(std::io::Error::other("Lockfile is not a regular file"));
    }

    let current_uid = nix::unistd::getuid().as_raw();
    if file_stat.st_uid != current_uid {
        return Err(std::io::Error::other(
            "Lockfile is not owned by current user",
        ));
    }

    let path_stat = lstat(path).map_err(|e| {
        std::io::Error::other(format!("Lockfile path disappeared during acquisition: {e}"))
    })?;
    if (path_stat.st_mode & S_IFMT) != S_IFREG
        || file_stat.st_dev != path_stat.st_dev
        || file_stat.st_ino != path_stat.st_ino
    {
        return Err(std::io::Error::other(
            "Lockfile was replaced during acquisition",
        ));
    }

    fchmod(
        file.as_fd(),
        Mode::from_bits_truncate(libc::mode_t::try_from(LOCK_FILE_MODE).unwrap_or(0)),
    )
    .map_err(std::io::Error::other)?;

    Ok(file)
}

fn read_holder_set(file: &mut Flock<File>) -> Result<HashSet<ProcessId>, std::io::Error> {
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content
        .lines()
        .filter_map(|line| line.parse::<ProcessId>().ok())
        .collect())
}

fn write_holder_set(
    file: &mut Flock<File>,
    pids: &HashSet<ProcessId>,
) -> Result<(), std::io::Error> {
    

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    let mut writer = BufWriter::new(&mut **file);
    for p in pids {
        writeln!(writer, "{p}")?;
    }
    writer.flush()?;
    Ok(())
}

fn prune_stale_holders(
    pids: &mut HashSet<ProcessId>,
    verbose: bool,
    process_checker: &ProcessChecker,
    pin: Option<&ProcessId>,
) {
    pids.retain(|p| {
        if pin == Some(p) {
            return true;
        }
        if process_checker(p.pid, p.start_time) {
            true
        } else {
            if verbose {
                println!(
                    "Removing stale process {}:{} from lockfile",
                    p.pid, p.start_time
                );
            }
            false
        }
    });
}

fn mutate_lockfile<R>(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
    pin: Option<&ProcessId>,
    mutate: impl FnOnce(&mut HashSet<ProcessId>) -> Result<R, std::io::Error>,
) -> Result<R, std::io::Error> {
    let mut file = open_validated_lockfile(path)?;
    let mut pids = read_holder_set(&mut file)?;
    prune_stale_holders(&mut pids, verbose, process_checker, pin);
    let result = mutate(&mut pids)?;
    write_holder_set(&mut file, &pids)?;
    Ok(result)
}

/// Returns true when the global sleep state should change.
pub(crate) fn update_lockfile(
    add: bool,
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
    current_proc: &ProcessId,
) -> Result<bool, std::io::Error> {
    mutate_lockfile(verbose, path, process_checker, Some(current_proc), |pids| {
        let active_count_before = pids.len();
        if add {
            pids.insert(*current_proc);
        } else {
            pids.remove(current_proc);
        }
        let active_count_after = pids.len();
        Ok(if add {
            active_count_before == 0
        } else {
            active_count_after == 0
        })
    })
}

/// Prune stale lockfile entries under an exclusive lock and return the live holder count.
#[cfg(target_os = "macos")]
pub(crate) fn prune_lockfile(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
) -> Result<usize, std::io::Error> {
    mutate_lockfile(verbose, path, process_checker, None, |pids| Ok(pids.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_lock_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_test_{}_{}.lock",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        path
    }

    fn proc(pid: i32, seconds: u64) -> ProcessId {
        ProcessId {
            pid,
            start_time: ProcessStartTime {
                seconds,
                microseconds: 0,
            },
        }
    }

    fn read_entries(path: &Path) -> Vec<ProcessId> {
        let mut entries = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| line.parse::<ProcessId>().unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.pid);
        entries
    }

    #[test]
    fn process_id_round_trips_with_microseconds() {
        let process_id = ProcessId {
            pid: 123,
            start_time: ProcessStartTime {
                seconds: 456,
                microseconds: 789,
            },
        };

        let serialized = process_id.to_string();
        assert_eq!(serialized, "123:456:789");
        assert_eq!(serialized.parse::<ProcessId>().unwrap(), process_id);
    }

    #[test]
    fn process_id_rejects_malformed_entries() {
        for entry in [
            "123",
            "123:456",
            "0:456:789",
            "-1:456:789",
            "123:456:1000000",
            "123:456:789:extra",
            "abc:456:789",
        ] {
            assert!(entry.parse::<ProcessId>().is_err(), "{entry:?}");
        }
    }

    #[test]
    fn first_instance_creates_secure_lockfile_and_requests_sleep_toggle() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let should_toggle =
            update_lockfile(true, false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(should_toggle);
        assert_eq!(read_entries(&lock_path), vec![current_proc]);
        let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, LOCK_FILE_MODE);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn subsequent_instance_preserves_live_entries_without_toggling() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let other_proc = proc(200, 456);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{other_proc}").unwrap();
        }

        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 200 && start_time.seconds == 456;

        let should_toggle =
            update_lockfile(true, false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!should_toggle);
        assert_eq!(read_entries(&lock_path), vec![current_proc, other_proc]);

        let should_toggle =
            update_lockfile(false, false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!should_toggle);
        assert_eq!(read_entries(&lock_path), vec![other_proc]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn stale_and_malformed_entries_are_cleaned_up() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let live_proc = proc(200, 456);
        let stale_proc = proc(300, 789);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{live_proc}").unwrap();
            writeln!(file, "{stale_proc}").unwrap();
            writeln!(file, "malformed").unwrap();
            writeln!(file, "0:1:0").unwrap();
            writeln!(file, "{live_proc}").unwrap();
        }

        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 200 && start_time.seconds == 456;

        let should_toggle =
            update_lockfile(true, false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!should_toggle);
        assert_eq!(read_entries(&lock_path), vec![current_proc, live_proc]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn removing_last_entry_requests_sleep_toggle() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{current_proc}").unwrap();
        }
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let should_toggle =
            update_lockfile(false, false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(should_toggle);
        assert!(read_entries(&lock_path).is_empty());

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn symlink_lockfile_is_rejected() {
        let target_path = temp_lock_path();
        let symlink_path = temp_lock_path();
        File::create(&target_path).unwrap();
        symlink(&target_path, &symlink_path).unwrap();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let result = update_lockfile(true, false, &symlink_path, &process_checker, &current_proc);

        assert!(result.is_err());
        assert!(read_entries(&target_path).is_empty());

        std::fs::remove_file(&target_path).unwrap();
        std::fs::remove_file(&symlink_path).unwrap();
    }
}
