//! Backgrounds the tray by re-execing it as a detached child process.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use caffeinate2::tray::{InstanceProbe, probe};

/// Re-exec this binary detached from the controlling terminal, then exit the
/// parent. Never returns: it always terminates the calling process.
///
/// A plain `fork()` is not an option here because the tray initializes AppKit
/// and spawns threads almost immediately, so the child is a fresh process
/// image spawned with no arguments — the absent `-d` is what stops it from
/// detaching again, and its own `acquire_or_exit` enforces single-instance.
pub fn spawn_detached_or_exit() -> ! {
    // The child's stderr goes to /dev/null, so its "already running" message
    // would be invisible; check the lock here where the user can see it. The
    // child's own acquire remains authoritative for the race window between
    // this probe and its startup.
    match probe() {
        InstanceProbe::Running => {
            eprintln!("caffeinate2-tray: another instance is already running");
            std::process::exit(1);
        }
        InstanceProbe::Indeterminate(message) => {
            eprintln!("caffeinate2-tray: {message}");
            std::process::exit(1);
        }
        InstanceProbe::Available => {}
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("caffeinate2-tray: could not resolve executable path: {error}");
            std::process::exit(1);
        }
    };

    let mut command = Command::new(exe);
    // All three streams go to /dev/null rather than a pipe or the inherited
    // terminal: the tray writes to stderr during normal operation, and those
    // writes must stay valid after this parent and its terminal are gone.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Start a new session (after fork, before exec) so the child has no
    // controlling terminal and outlives the terminal without seeing SIGHUP.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    match command.spawn() {
        Ok(child) => {
            eprintln!(
                "caffeinate2-tray: started in background (pid {})",
                child.id()
            );
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("caffeinate2-tray: failed to launch background instance: {error}");
            std::process::exit(1);
        }
    }
}
