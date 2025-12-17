use crate::power_management;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const LOCK_FILE_PATH: &str = "/tmp/caffeinate2.lock";

pub struct ProcessLock {
    verbose: bool,
}

impl ProcessLock {
    pub fn new(verbose: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let should_disable = update_lockfile(true, verbose)?;

        if should_disable {
            if verbose {
                println!("First instance detected. Disabling system sleep globally.");
            }
            power_management::set_sleep_disabled(true, verbose).map_err(|code| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to disable sleep (IOKit error: {:X})", code),
                )
            })?;
        } else if verbose {
            println!("Other instances running. Sleep already disabled.");
        }

        Ok(Self { verbose })
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        match update_lockfile(false, self.verbose) {
            Ok(should_enable) => {
                if should_enable {
                    if self.verbose {
                        println!("Last instance exiting. Re-enabling system sleep globally.");
                    }
                    if let Err(code) = power_management::set_sleep_disabled(false, self.verbose) {
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
fn update_lockfile(add: bool, verbose: bool) -> Result<bool, std::io::Error> {
    let path = Path::new(LOCK_FILE_PATH);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o666)
        .open(path)?;

    let mut file = match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(f) => f,
        Err((_, e)) => return Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    };

    let mut content = String::new();
    file.read_to_string(&mut content)?;

    let current_pid = std::process::id() as i32;
    let mut pids: Vec<i32> = content
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect();

    // Filter out dead processes
    pids.retain(|&pid| {
        if pid == current_pid {
            return true;
        }
        match kill(Pid::from_raw(pid), None) {
            Ok(_) => true,
            Err(nix::errno::Errno::ESRCH) => {
                if verbose {
                    println!("Removing stale PID {} from lockfile", pid);
                }
                false
            }
            Err(_) => true, // Assume alive on other errors (e.g. permission) to be safe
        }
    });

    let active_count_before = pids.len();

    if add {
        if !pids.contains(&current_pid) {
            pids.push(current_pid);
        }
    } else {
        if let Some(pos) = pids.iter().position(|&x| x == current_pid) {
            pids.remove(pos);
        }
    }

    let active_count_after = pids.len();

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    for pid in &pids {
        writeln!(file, "{}", pid)?;
    }

    let should_toggle = if add {
        active_count_before == 0
    } else {
        active_count_after == 0
    };

    Ok(should_toggle)
}
