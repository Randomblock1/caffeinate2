use crate::power_management;
use libc::{PROC_PIDTBSDINFO, S_IFMT, S_IFREG, proc_bsdinfo, proc_pidinfo};
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::sys::signal::kill;
use nix::sys::stat::{Mode, fchmod, fstat, lstat};
use nix::unistd::Pid;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const LOCK_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct ProcessStartTime {
    seconds: u64,
    microseconds: u64,
}

impl std::fmt::Display for ProcessStartTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.seconds, self.microseconds)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ProcessId {
    pid: i32,
    start_time: ProcessStartTime,
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
        Ok(ProcessId { pid, start_time })
    }
}

fn get_process_start_time(pid: i32) -> Option<ProcessStartTime> {
    unsafe {
        let mut info = std::mem::zeroed::<proc_bsdinfo>();
        let size = std::mem::size_of::<proc_bsdinfo>() as i32;
        let ret = proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut _,
            size,
        );
        if ret == size {
            Some(ProcessStartTime {
                seconds: info.pbi_start_tvsec,
                microseconds: info.pbi_start_tvusec,
            })
        } else {
            None
        }
    }
}

type SleepDisabler = Box<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;
type ProcessChecker = Box<dyn Fn(i32, ProcessStartTime) -> bool + Send + Sync>;

pub struct ProcessLock {
    verbose: bool,
    lock_file_path: PathBuf,
    sleep_disabler: SleepDisabler,
    process_checker: ProcessChecker,
}

impl ProcessLock {
    pub fn new(verbose: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let lock_path = if nix::unistd::getuid().is_root() {
            PathBuf::from("/var/run/caffeinate2.lock")
        } else {
            PathBuf::from(format!("/tmp/caffeinate2_{}.lock", nix::unistd::getuid()))
        };

        Self::with_options(
            verbose,
            lock_path,
            Box::new(power_management::set_sleep_disabled),
            Box::new(default_process_checker),
        )
    }

    fn with_options(
        verbose: bool,
        lock_file_path: PathBuf,
        sleep_disabler: SleepDisabler,
        process_checker: ProcessChecker,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let should_disable = update_lockfile(true, verbose, &lock_file_path, &process_checker)?;

        if should_disable {
            if verbose {
                println!("First instance detected. Disabling system sleep globally.");
            }
            sleep_disabler(true, verbose).map_err(|code| {
                std::io::Error::other(format!("Failed to disable sleep (IOKit error: {:X})", code))
            })?;
        } else if verbose {
            println!("Other instances running. Sleep already disabled.");
        }

        Ok(Self {
            verbose,
            lock_file_path,
            sleep_disabler,
            process_checker,
        })
    }
}

fn default_process_checker(pid: i32, start_time: ProcessStartTime) -> bool {
    let is_alive = match kill(Pid::from_raw(pid), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true, // Assume alive on permission errors
    };

    if !is_alive {
        return false;
    }

    // Verify start time to prevent PID reuse issues
    match get_process_start_time(pid) {
        Some(actual_start_time) => actual_start_time == start_time,
        None => true,
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        match update_lockfile(
            false,
            self.verbose,
            &self.lock_file_path,
            &self.process_checker,
        ) {
            Ok(should_enable) => {
                if should_enable {
                    if self.verbose {
                        println!("Last instance exiting. Re-enabling system sleep globally.");
                    }
                    if let Err(code) = (self.sleep_disabler)(false, self.verbose) {
                        eprintln!("Error: Failed to re-enable sleep (IOKit error: {:X})", code);
                    }
                } else if self.verbose {
                    println!("Other instances still running. Keeping sleep disabled.");
                }
            }
            Err(e) => {
                eprintln!("Error updating lockfile during exit: {}", e);
            }
        }
    }
}

/// Returns true if the state should change
fn update_lockfile(
    add: bool,
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
) -> Result<bool, std::io::Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(LOCK_FILE_MODE)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(path)?;

    // Lock the file
    let mut file = match Flock::lock(file, FlockArg::LockExclusive) {
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
        Mode::from_bits_truncate(LOCK_FILE_MODE as libc::mode_t),
    )
    .map_err(std::io::Error::other)?;

    let mut content = String::new();
    file.read_to_string(&mut content)?;

    let current_id = std::process::id() as i32;
    let current_start_time = get_process_start_time(current_id)
        .ok_or_else(|| std::io::Error::other("Failed to determine current process start time"))?;
    let current_proc = ProcessId {
        pid: current_id,
        start_time: current_start_time,
    };

    let mut pids: HashSet<ProcessId> = content
        .lines()
        .filter_map(|line| line.parse::<ProcessId>().ok())
        .collect();

    // Filter out dead processes or stale entries (PID reuse)
    pids.retain(|p| {
        if p.pid == current_id && p.start_time == current_start_time {
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

    let active_count_before = pids.len();

    if add {
        pids.insert(current_proc);
    } else {
        pids.remove(&current_proc);
    }

    let active_count_after = pids.len();

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    {
        let mut writer = BufWriter::new(&mut *file);
        for p in &pids {
            writeln!(writer, "{}", p)?;
        }
        writer.flush()?;
    }

    let should_toggle = if add {
        active_count_before == 0
    } else {
        active_count_after == 0
    };

    Ok(should_toggle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    // Since we don't have rand crate in Cargo.toml and don't want to add it just for this if not needed,
    // let's use a simple counter or SystemTime for unique names.
    fn temp_lock_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let start = SystemTime::now();
        let since_the_epoch = start.duration_since(UNIX_EPOCH).unwrap();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_test_{}.lock",
            since_the_epoch.as_nanos()
        ));
        path
    }

    fn test_start_time(seconds: u64) -> ProcessStartTime {
        ProcessStartTime {
            seconds,
            microseconds: 0,
        }
    }

    #[test]
    fn test_process_id_round_trips_with_microseconds() {
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
    fn test_process_id_rejects_malformed_entries() {
        assert!("123".parse::<ProcessId>().is_err());
        assert!("123:456".parse::<ProcessId>().is_err());
        assert!("0:456:789".parse::<ProcessId>().is_err());
        assert!("-1:456:789".parse::<ProcessId>().is_err());
        assert!("123:456:1000000".parse::<ProcessId>().is_err());
        assert!("123:456:789:extra".parse::<ProcessId>().is_err());
    }

    #[test]
    fn test_process_checker_rejects_current_pid_with_wrong_start_time() {
        let current_pid = std::process::id() as i32;
        let current_start_time = get_process_start_time(current_pid).unwrap();
        let wrong_start_time = ProcessStartTime {
            seconds: current_start_time.seconds.saturating_add(1),
            microseconds: current_start_time.microseconds,
        };

        assert!(default_process_checker(current_pid, current_start_time));
        assert!(!default_process_checker(current_pid, wrong_start_time));
    }

    #[test]
    fn test_first_instance() {
        let lock_path = temp_lock_path();
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(|_pid: i32, _start_time: ProcessStartTime| false);

        let lock =
            ProcessLock::with_options(true, lock_path.clone(), sleep_disabler, process_checker)
                .unwrap();

        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&std::process::id().to_string()));
        let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, LOCK_FILE_MODE);

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
    fn test_subsequent_instance() {
        let lock_path = temp_lock_path();
        let other_pid = 99999;
        let other_start = test_start_time(12345);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(
                file,
                "{}:{}:{}",
                other_pid, other_start.seconds, other_start.microseconds
            )
            .unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(move |pid: i32, start: ProcessStartTime| {
            pid == other_pid && start == other_start
        });

        let lock =
            ProcessLock::with_options(true, lock_path.clone(), sleep_disabler, process_checker)
                .unwrap();

        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&format!(
            "{}:{}:{}",
            other_pid, other_start.seconds, other_start.microseconds
        )));
        assert!(content.contains(&std::process::id().to_string()));

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 0);

        drop(calls);
        drop(lock);

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 0);

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn test_stale_pid_cleanup() {
        let lock_path = temp_lock_path();
        let dead_pid = 88888;
        let dead_start = test_start_time(67890);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(
                file,
                "{}:{}:{}",
                dead_pid, dead_start.seconds, dead_start.microseconds
            )
            .unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(|_pid: i32, _start_time: ProcessStartTime| false);

        let lock =
            ProcessLock::with_options(true, lock_path.clone(), sleep_disabler, process_checker)
                .unwrap();

        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(!content.contains(&dead_pid.to_string()));
        assert!(content.contains(&std::process::id().to_string()));

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0]);

        drop(calls);
        drop(lock);

        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
