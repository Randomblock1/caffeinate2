use crate::power_management;
use libc::{proc_pidinfo, proc_bsdinfo, PROC_PIDTBSDINFO};
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ProcessId {
    pid: i32,
    start_time: u64,
}

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.pid, self.start_time)
    }
}

impl std::str::FromStr for ProcessId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 2 {
            return Err(());
        }
        let pid = parts[0].parse().map_err(|_| ())?;
        let start_time = parts[1].parse().map_err(|_| ())?;
        Ok(ProcessId { pid, start_time })
    }
}

fn get_process_start_time(pid: i32) -> u64 {
    unsafe {
        let mut info = std::mem::zeroed::<proc_bsdinfo>();
        let size = std::mem::size_of::<proc_bsdinfo>() as i32;
        let ret = proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut _, size);
        if ret == size {
            info.pbi_start_tvsec
        } else {
            0
        }
    }
}

type SleepDisabler = Box<dyn Fn(bool, bool) -> Result<(), u32> + Send + Sync>;
type ProcessChecker = Box<dyn Fn(i32, u64) -> bool + Send + Sync>;

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
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to disable sleep (IOKit error: {:X})", code),
                )
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

fn default_process_checker(pid: i32, start_time: u64) -> bool {
    let current_id = std::process::id() as i32;
    if pid == current_id {
        return true;
    }

    // Check if PID is alive
    let is_alive = match kill(Pid::from_raw(pid), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true, // Assume alive on permission errors
    };

    if !is_alive {
        return false;
    }

    // Verify start time to prevent PID reuse issues
    let actual_start_time = get_process_start_time(pid);
    actual_start_time == start_time
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
    let mode = if nix::unistd::getuid().is_root() {
        0o644
    } else {
        0o600
    };

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(mode)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(path)?;

    // Lock the file
    let mut file = match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(f) => f,
        Err((_, e)) => return Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    };

    let fstat = nix::sys::stat::fstat(file.as_fd())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    if let Ok(path_stat) = nix::sys::stat::stat(path) {
        if fstat.st_ino != path_stat.st_ino {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Lockfile was replaced during acquisition",
            ));
        }
    }

    let mut content = String::new();
    file.read_to_string(&mut content)?;

    let current_id = std::process::id() as i32;
    let current_start_time = get_process_start_time(current_id);
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

        let process_checker = Box::new(|_pid: i32, _start_time: u64| false);

        let lock = ProcessLock::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        )
        .unwrap();

        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&std::process::id().to_string()));

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], true);

        drop(calls);
        drop(lock);

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], false);

        if lock_path.exists() {
             std::fs::remove_file(&lock_path).unwrap();
        }
    }

    #[test]
    fn test_subsequent_instance() {
        let lock_path = temp_lock_path();
        let other_pid = 99999;
        let other_start = 12345;
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{}:{}", other_pid, other_start).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(move |pid: i32, start: u64| pid == other_pid && start == other_start);

        let lock = ProcessLock::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        )
        .unwrap();

        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(content.contains(&format!("{}:{}", other_pid, other_start)));
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
        let dead_start = 67890;
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{}:{}", dead_pid, dead_start).unwrap();
        }

        let sleep_calls = Arc::new(Mutex::new(Vec::new()));
        let sleep_calls_clone = sleep_calls.clone();

        let sleep_disabler = Box::new(move |state: bool, _verbose: bool| {
            sleep_calls_clone.lock().unwrap().push(state);
            Ok(())
        });

        let process_checker = Box::new(|_pid: i32, _start_time: u64| false);

        let lock = ProcessLock::with_options(
            true,
            lock_path.clone(),
            sleep_disabler,
            process_checker,
        )
        .unwrap();

        let content = std::fs::read_to_string(&lock_path).unwrap();
        assert!(!content.contains(&dead_pid.to_string()));
        assert!(content.contains(&std::process::id().to_string()));

        let calls = sleep_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], true);

        drop(calls);
        drop(lock);

        if lock_path.exists() {
             std::fs::remove_file(&lock_path).unwrap();
        }
    }
}
