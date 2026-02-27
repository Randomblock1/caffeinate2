use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[cfg(target_os = "macos")]
use crate::power_management;

use crate::lock_logic;

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
            #[cfg(target_os = "macos")]
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
                    #[cfg(target_os = "macos")]
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
    let mut file = open_and_lock_file(path)?;

    let pids = read_pids(&mut file)?;
    let current_pid = std::process::id() as i32;

    let (new_pids, should_toggle) = lock_logic::update_pid_list(pids, current_pid, add, verbose, is_process_alive);

    write_pids(&mut file, &new_pids)?;

    Ok(should_toggle)
}

fn open_and_lock_file(path: &Path) -> Result<Flock<std::fs::File>, std::io::Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o666)
        .open(path)?;

    match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(f) => Ok(f),
        Err((_, e)) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    }
}

fn read_pids(file: &mut std::fs::File) -> Result<Vec<i32>, std::io::Error> {
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect())
}

fn write_pids(file: &mut std::fs::File, pids: &[i32]) -> Result<(), std::io::Error> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    for pid in pids {
        writeln!(file, "{}", pid)?;
    }
    Ok(())
}

fn is_process_alive(pid: i32, verbose: bool) -> bool {
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
}
