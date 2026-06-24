use super::Args;
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
    if args
        .command
        .as_ref()
        .is_some_and(|command| !command.is_empty())
    {
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

/// Detect `-t 1 hour ...` style invocations where clap parsed trailing duration
/// tokens as a command instead of part of the timeout string. Returns a help
/// message so the caller can reject the invocation before taking a sleep hold,
/// rather than silently running the first stray word (`hour`) as a command.
#[must_use]
pub fn misquoted_duration_error(args: &Args) -> Option<String> {
    let timeout = args.timeout.as_deref()?;
    let command = args.command.as_ref()?;
    if command.is_empty() {
        return None;
    }

    const DURATION_WORDS: &[&str] = &[
        "hour", "hours", "hr", "hrs", "minute", "minutes", "min", "mins", "second", "seconds",
        "sec", "secs", "day", "days", "week", "weeks", "month", "months", "year", "years", "and",
    ];

    let timeout_is_lone_number = timeout.parse::<u64>().is_ok();
    let command_has_duration_words = command.iter().any(|word| {
        let lower = word.to_lowercase();
        DURATION_WORDS.contains(&lower.as_str())
            || matches!(word.as_str(), "m" | "h" | "d" | "w" | "s")
    });

    if timeout_is_lone_number && command_has_duration_words {
        let suggested = format!("{timeout} {}", command.join(" "));
        return Some(format!(
            "Error: multi-word durations must be quoted (did you mean: caffeinate2 -t \"{suggested}\")?"
        ));
    }

    None
}

#[derive(Debug, PartialEq, Eq)]
pub enum WaitForPidResult {
    Exited(i32),
    TimedOut,
}

#[derive(Debug)]
pub enum WaitForPidError {
    InvalidPid,
    NotFound,
    Kevent(nix::Error),
}

fn timespec_from_duration(duration: Duration) -> libc::timespec {
    let max_seconds = <libc::time_t>::MAX as u64;
    libc::timespec {
        tv_sec: duration
            .as_secs()
            .min(max_seconds)
            .try_into()
            .unwrap_or(<libc::time_t>::MAX),
        tv_nsec: libc::c_long::from(duration.subsec_nanos()),
    }
}

/// `NOTE_EXITSTATUS` delivers the raw `wait(2)` status word, not the exit
/// code: a normal exit N arrives as `N << 8`. Decode it like a shell would
/// (128 + signal number for signal deaths).
const fn exit_code_from_wait_status(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        status
    }
}

/// Wait until `pid` exits, or until `timeout` elapses when set.
///
/// macOS recycles PIDs: this watches the numeric PID only and cannot prove the
/// exiting process is the same one that was running at registration (the same
/// limitation as `caffeinate -w`).
pub fn wait_for_pid(
    pid: i32,
    timeout: Option<Duration>,
    verbose: bool,
) -> Result<WaitForPidResult, WaitForPidError> {
    if pid <= 0 {
        return Err(WaitForPidError::InvalidPid);
    }

    let kq = event::Kqueue::new().map_err(WaitForPidError::Kevent)?;
    let kev = event::KEvent::new(
        pid.cast_unsigned() as usize,
        event::EventFilter::EVFILT_PROC,
        event::EvFlags::EV_ADD | event::EvFlags::EV_ENABLE | event::EvFlags::EV_ONESHOT,
        // `NOTE_EXITSTATUS` is documented as valid only on child processes and
        // only alongside `NOTE_EXIT`; request both so the subscription is
        // well-formed for arbitrary PIDs (for non-children `NOTE_EXIT` still
        // fires the wake, the status just decodes to 0 — the same limitation as
        // `caffeinate -w`).
        event::FilterFlag::NOTE_EXIT | event::FilterFlag::NOTE_EXITSTATUS,
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
            return Err(WaitForPidError::NotFound);
        }
        return Err(WaitForPidError::Kevent(nix::Error::from_raw(
            i32::try_from(event.data()).unwrap_or(i32::MAX),
        )));
    }

    Ok(WaitForPidResult::Exited(exit_code_from_wait_status(
        i32::try_from(event.data()).unwrap_or(i32::MAX),
    )))
}

#[cfg(test)]
mod tests {
    use super::super::parse_args;
    use super::*;

    #[test]
    fn dead_pid_at_registration_is_not_found() {
        let result = wait_for_pid(i32::MAX, Some(Duration::from_millis(1)), false);
        assert!(matches!(result, Err(WaitForPidError::NotFound)));
    }

    #[test]
    fn wait_for_pid_rejects_non_positive_pids() {
        assert!(matches!(
            wait_for_pid(0, Some(Duration::from_millis(1)), false),
            Err(WaitForPidError::InvalidPid)
        ));
        assert!(matches!(
            wait_for_pid(-1, Some(Duration::from_millis(1)), false),
            Err(WaitForPidError::InvalidPid)
        ));
    }

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
    fn misquoted_duration_is_detected() {
        let args = parse_args(&["caffeinate2", "-t", "1", "hour", "and", "30", "minutes"]);
        let message = misquoted_duration_error(&args).expect("should detect misquoted duration");
        assert!(message.contains("multi-word durations must be quoted"));
        assert!(message.contains("-t \"1 hour and 30 minutes\""));
    }

    #[test]
    fn genuine_command_after_numeric_timeout_is_not_flagged() {
        // `-t 3600 -- myscript` is a legitimate timeout+command, not a misquote.
        let args = parse_args(&["caffeinate2", "-t", "3600", "--", "myscript"]);
        assert!(misquoted_duration_error(&args).is_none());
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
