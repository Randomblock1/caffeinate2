//! Backgrounds the tray by re-execing it as a detached child process.

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use caffeinate2::tray::{InstanceProbe, probe};

/// Where the detached child's stderr goes. The tray writes its tracing output
/// to stderr, and — more importantly — any startup failure lands here instead
/// of vanishing: by the time the child dies, this parent (and its terminal)
/// are gone.
fn child_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/caffeinate2-tray.log"))
}

/// Re-exec this binary detached from the controlling terminal, then exit the
/// parent. Never returns: it always terminates the calling process.
///
/// A plain `fork()` is not an option here because the tray initializes AppKit
/// and spawns threads almost immediately, so the child is a fresh process
/// image spawned with no arguments — the absent `-d` is what stops it from
/// detaching again, and its own `acquire_or_exit` enforces single-instance.
pub fn spawn_detached_or_exit() -> ! {
    // The child's "already running" message lands only in its log file, not
    // this terminal; check the lock here where the user can see the outcome.
    // The child's own acquire remains authoritative for the race window
    // between this probe and its startup.
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
    // stdin/stdout go to /dev/null rather than a pipe or the inherited
    // terminal: writes must stay valid after this parent and its terminal are
    // gone. stderr goes to a log file when one can be opened, so a child that
    // dies after this parent exits leaves a diagnostic; otherwise it too is
    // discarded, as before.
    let log_path = child_log_path();
    let log_file = log_path.as_ref().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let logged_to: Option<&Path> = log_file.as_ref().and(log_path.as_deref());
    let stderr = log_file.map_or_else(Stdio::null, Stdio::from);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);

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
        Ok(mut child) => {
            // Give the child a moment to die on the spot (lost the
            // single-instance race to a concurrent launch, tray init failure)
            // so this doesn't report success for a process that is already
            // gone. A failure slower than this window still slips past; the
            // log file is the durable diagnostic either way.
            for _ in 0..10 {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!(
                            "caffeinate2-tray: background instance exited immediately ({status})"
                        );
                        if let Some(path) = logged_to {
                            eprintln!("caffeinate2-tray: see {} for details", path.display());
                        }
                        std::process::exit(1);
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    // try_wait failing is no reason to distrust the spawn;
                    // fall through to the success report.
                    Err(_) => break,
                }
            }
            eprintln!(
                "caffeinate2-tray: started in background (pid {})",
                child.id()
            );
            if let Some(path) = logged_to {
                eprintln!("caffeinate2-tray: logging to {}", path.display());
            }
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("caffeinate2-tray: failed to launch background instance: {error}");
            std::process::exit(1);
        }
    }
}
