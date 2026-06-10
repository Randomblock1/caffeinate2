use crate::cli::Args;
use nix::sys::event;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum WaitMode {
    Command,
    Timeout,
    Pid,
    TimeoutOrPid,
    UntilInterrupt,
}

pub fn wait_mode(args: &Args) -> WaitMode {
    if args.command.is_some() {
        WaitMode::Command
    } else {
        match (args.timeout.is_some(), args.waitfor.is_some()) {
            (true, true) => WaitMode::TimeoutOrPid,
            (true, false) => WaitMode::Timeout,
            (false, true) => WaitMode::Pid,
            (false, false) => WaitMode::UntilInterrupt,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WaitForPidResult {
    Exited(i32),
    TimedOut,
}

#[derive(Debug)]
pub enum WaitForPidError {
    NotFound,
    Kevent(nix::Error),
}

fn timespec_from_duration(duration: Duration) -> libc::timespec {
    let max_seconds = <libc::time_t>::MAX as u64;
    libc::timespec {
        tv_sec: duration.as_secs().min(max_seconds) as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

/// `NOTE_EXITSTATUS` delivers the raw `wait(2)` status word, not the exit
/// code: a normal exit N arrives as `N << 8`. Decode it like a shell would
/// (128 + signal number for signal deaths).
fn exit_code_from_wait_status(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        status
    }
}

pub fn wait_for_pid(
    pid: i32,
    timeout: Option<Duration>,
    verbose: bool,
) -> Result<WaitForPidResult, WaitForPidError> {
    let kq = event::Kqueue::new().map_err(WaitForPidError::Kevent)?;
    let kev = event::KEvent::new(
        pid as usize,
        event::EventFilter::EVFILT_PROC,
        event::EvFlags::EV_ADD
            | event::EvFlags::EV_ENABLE
            | event::EvFlags::EV_ONESHOT
            | event::EvFlags::EV_ERROR,
        event::FilterFlag::NOTE_EXITSTATUS,
        0,
        0,
    );

    let mut eventlist = [kev];
    let timeout = timeout.map(timespec_from_duration);
    let event_count = kq
        .kevent(&[kev], &mut eventlist, timeout)
        .map_err(WaitForPidError::Kevent)?;

    if event_count == 0 {
        return Ok(WaitForPidResult::TimedOut);
    }

    let event = eventlist[0];
    if verbose {
        println!("{event:#?}");
    }

    if event.flags().contains(event::EvFlags::EV_ERROR) {
        if event.data() == nix::Error::ESRCH as isize {
            Err(WaitForPidError::NotFound)
        } else {
            Err(WaitForPidError::Kevent(nix::Error::from_raw(
                event.data() as i32,
            )))
        }
    } else {
        Ok(WaitForPidResult::Exited(exit_code_from_wait_status(
            event.data() as i32,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse_args;

    #[test]
    fn wait_status_decodes_normal_exit() {
        assert_eq!(exit_code_from_wait_status(0), 0);
        assert_eq!(exit_code_from_wait_status(3 << 8), 3);
        assert_eq!(exit_code_from_wait_status(255 << 8), 255);
    }

    #[test]
    fn wait_status_decodes_signal_death() {
        assert_eq!(exit_code_from_wait_status(libc::SIGTERM), 128 + 15);
        assert_eq!(exit_code_from_wait_status(libc::SIGKILL), 128 + 9);
    }

    #[test]
    fn wait_mode_matches_cli_priority() {
        assert_eq!(
            wait_mode(&parse_args(&["caffeinate2"])),
            WaitMode::UntilInterrupt
        );
        assert_eq!(
            wait_mode(&parse_args(&["caffeinate2", "--timeout", "10m"])),
            WaitMode::Timeout
        );
        assert_eq!(
            wait_mode(&parse_args(&["caffeinate2", "--waitfor", "123"])),
            WaitMode::Pid
        );
        assert_eq!(
            wait_mode(&parse_args(&[
                "caffeinate2",
                "--timeout",
                "10m",
                "--waitfor",
                "123"
            ])),
            WaitMode::TimeoutOrPid
        );
        assert_eq!(
            wait_mode(&parse_args(&[
                "caffeinate2",
                "--timeout",
                "10m",
                "echo",
                "ok"
            ])),
            WaitMode::Command
        );
    }
}
