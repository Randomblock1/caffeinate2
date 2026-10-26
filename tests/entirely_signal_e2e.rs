//! End-to-end check of the signal-driven release path, against the real
//! binary and the real ledger: spawn `caffeinate2 --entirely`, wait for its
//! holder entry to appear in the lockfile, SIGTERM it, and assert the entry is
//! gone and the exit code reflects the signal. Everything between the signal
//! and the ledger write — the signal thread, the shutdown handshake, the
//! hold's Drop/release — runs for real; the unit tests and models cover the
//! pieces, this covers the seams.
//!
//! Root-only (entirely mode toggles the system-wide SleepDisabled setting), so
//! it is `#[ignore]`d for ordinary `cargo test` runs and exercised by hand:
//!
//! ```sh
//! sudo cargo test --test entirely_signal_e2e -- --ignored
//! ```
//!
//! It briefly disables real system sleep and releases it on the way out. With
//! a helper daemon installed the hold goes through the helper (which must be a
//! current build — it shares the lockfile path asserted here); with none, the
//! root CLI fallback writes the lockfile directly.

#![cfg(target_os = "macos")]

use caffeinate2::entirely::coordinator::HELPER_LOCK_PATH;
use caffeinate2::entirely::lockfile::ProcessId;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Whether the ledger currently records `pid` as a holder.
fn ledger_holds(pid: i32) -> bool {
    std::fs::read_to_string(HELPER_LOCK_PATH)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse::<ProcessId>().ok())
        .any(|holder| holder.pid == pid)
}

#[test]
#[ignore = "requires root and briefly disables real system sleep; run: sudo cargo test --test entirely_signal_e2e -- --ignored"]
fn sigterm_releases_the_entirely_ledger_entry() {
    assert!(
        nix::unistd::Uid::effective().is_root(),
        "this test must run as root (sudo cargo test ...)"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_caffeinate2"))
        .arg("--entirely")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn caffeinate2 --entirely");
    let pid = i32::try_from(child.id()).expect("pid fits in i32");

    // The hold lands in the ledger BEFORE main registers its signal handler,
    // so a SIGTERM sent on ledger evidence alone can hit the default
    // disposition and kill the process without a release. The "until Ctrl+C
    // pressed" banner is printed only after `Signals::new` has installed the
    // handler — wait for it before signalling.
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let banner_seen = loop {
        match lines.next() {
            Some(Ok(line)) if line.contains("until Ctrl+C pressed") => break true,
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break false,
        }
    };
    if !banner_seen {
        let _ = child.kill();
        panic!("caffeinate2 (pid {pid}) exited before arming its signal handler");
    }

    // The banner also orders after the hold, but re-check the ledger so a
    // failure names the right culprit (e.g. a stale helper writing elsewhere).
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ledger_holds(pid) {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "caffeinate2 (pid {pid}) never appeared in {HELPER_LOCK_PATH}; \
                 is a stale helper daemon installed?"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("SIGTERM the child");

    let status = child.wait().expect("wait for the child");
    // The signal path exits 128 + signo, releasing the hold first.
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(
        !ledger_holds(pid),
        "the SIGTERM'd session's holder entry must be released, not left \
         for the reaper"
    );
}
