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
///
/// `command_explicitly_separated` reports whether the user delimited the command
/// with `--` (clap strips the separator, so it is invisible in `args`). The
/// documented escape hatch `caffeinate2 -t 3600 -- hour` runs a command that
/// happens to be a duration word *on purpose*; honoring `--` keeps that from
/// being misread as a misquoted duration.
#[must_use]
pub fn misquoted_duration_error(args: &Args, command_explicitly_separated: bool) -> Option<String> {
    // An explicit `--` means the trailing tokens are unambiguously the command,
    // not a continuation of the duration, so never second-guess them.
    if command_explicitly_separated {
        return None;
    }
    let timeout = args.timeout.as_deref()?;
    let command = args.command.as_ref()?;
    if command.is_empty() {
        return None;
    }

    const DURATION_WORDS: &[&str] = &[
        "hour", "hours", "hr", "hrs", "minute", "minutes", "min", "mins", "second", "seconds",
        "sec", "secs", "day", "days", "week", "weeks", "month", "months", "year", "years",
    ];

    let is_unit = |lower: &str| -> bool {
        DURATION_WORDS.contains(&lower) || matches!(lower, "m" | "h" | "d" | "w" | "s")
    };

    // Whether a token fits a duration continuation, and if so (`Some`) whether
    // it carries an actual unit. Unit words and the `and` connector fit as-is;
    // a digit-led token fits only when every letter run in it is a unit (`30`,
    // `30m`, `1h30m`). A letter run that is no unit (`7z`) marks a command
    // name, and a bare number (`2048`) carries no unit — without a unit
    // somewhere in the tail, the "command" is just a program with a numeric
    // name, not a spilled duration.
    let duration_shape = |word: &str| -> Option<bool> {
        let lower = word.to_lowercase();
        if is_unit(&lower) {
            return Some(true);
        }
        if lower == "and" {
            return Some(false);
        }
        if !lower.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let mut has_unit = false;
        let mut rest = lower.as_str();
        while !rest.is_empty() {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(rest.len());
            rest = &rest[digits..];
            if rest.is_empty() {
                break;
            }
            let letters = rest
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            if letters == 0 || !is_unit(&rest[..letters]) {
                return None;
            }
            has_unit = true;
            rest = &rest[letters..];
        }
        Some(has_unit)
    };

    // A full unit word among the command tokens (`hour`, `minutes`) is a
    // spilled duration on its own, so `-t 1 hour` trips the check even with no
    // digit in the tail. A lone single-letter alias still needs a digit, so
    // `caffeinate2 -t 300 w` (the w(1) tool) stays a real command rather than a
    // botched duration.
    let timeout_is_lone_number = timeout.parse::<u64>().is_ok();
    let command_has_digit = command
        .iter()
        .any(|word| word.chars().any(|c| c.is_ascii_digit()));
    let command_has_unit_word = command
        .iter()
        .any(|word| DURATION_WORDS.contains(&word.to_lowercase().as_str()));
    let command_is_duration_continuation = command
        .iter()
        .map(|word| duration_shape(word))
        .collect::<Option<Vec<bool>>>()
        .is_some_and(|units| units.contains(&true))
        && (command_has_digit || command_has_unit_word);

    if timeout_is_lone_number && command_is_duration_continuation {
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
    /// The process exited, but its status could not be read: subscribing to
    /// another user's exit status is refused (EACCES), so the wait fell back
    /// to a bare exit notification.
    ExitedStatusUnknown,
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
) -> Result<WaitForPidResult, WaitForPidError> {
    if pid <= 0 {
        return Err(WaitForPidError::InvalidPid);
    }

    // The man page documents `NOTE_EXITSTATUS` as valid only on child
    // processes, but the kernel is more permissive: verified on Darwin 25, the
    // subscription is accepted for any process of the caller's own user — even
    // an unrelated, launchd-parented one — and delivers the real wait(2)
    // status word. Only another user's process refuses it (the attach fails
    // with EACCES); fall back to a bare `NOTE_EXIT` subscription then, which
    // any observable process accepts, and report the exit without a status.
    match wait_for_pid_with(pid, timeout, true)? {
        WaitForPidResult::ExitedStatusUnknown => wait_for_pid_with(pid, timeout, false),
        outcome => Ok(outcome),
    }
}

fn wait_for_pid_with(
    pid: i32,
    timeout: Option<Duration>,
    with_status: bool,
) -> Result<WaitForPidResult, WaitForPidError> {
    let flags = if with_status {
        event::FilterFlag::NOTE_EXIT | event::FilterFlag::NOTE_EXITSTATUS
    } else {
        event::FilterFlag::NOTE_EXIT
    };
    let kq = event::Kqueue::new().map_err(WaitForPidError::Kevent)?;
    let kev = event::KEvent::new(
        pid.cast_unsigned() as usize,
        event::EventFilter::EVFILT_PROC,
        event::EvFlags::EV_ADD | event::EvFlags::EV_ENABLE | event::EvFlags::EV_ONESHOT,
        flags,
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
    tracing::debug!("{event:#?}");

    if event.flags().contains(event::EvFlags::EV_ERROR) {
        if event.data() == nix::Error::ESRCH as isize {
            // On the status-less retry the process was alive at the first
            // attach, so ESRCH means it exited in between — that is an exit
            // with an unknown status, not a bad pid.
            if !with_status {
                return Ok(WaitForPidResult::ExitedStatusUnknown);
            }
            return Err(WaitForPidError::NotFound);
        }
        // EACCES on the status subscription: the caller may not read this
        // process's exit status (it belongs to another user). Signal the
        // caller to retry without one.
        if with_status && event.data() == nix::Error::EACCES as isize {
            return Ok(WaitForPidResult::ExitedStatusUnknown);
        }
        return Err(WaitForPidError::Kevent(nix::Error::from_raw(
            i32::try_from(event.data()).unwrap_or(i32::MAX),
        )));
    }

    if !with_status {
        return Ok(WaitForPidResult::ExitedStatusUnknown);
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
        let result = wait_for_pid(i32::MAX, Some(Duration::from_millis(1)));
        assert!(matches!(result, Err(WaitForPidError::NotFound)));
    }

    #[test]
    fn wait_for_pid_rejects_non_positive_pids() {
        assert!(matches!(
            wait_for_pid(0, Some(Duration::from_millis(1))),
            Err(WaitForPidError::InvalidPid)
        ));
        assert!(matches!(
            wait_for_pid(-1, Some(Duration::from_millis(1))),
            Err(WaitForPidError::InvalidPid)
        ));
    }

    #[test]
    fn another_users_process_is_waited_on_not_errored() {
        // pid 1 (launchd, root-owned) always exists. As a non-root test run the
        // status subscription is refused (EACCES) and the fallback must wait —
        // here, time out — instead of failing the whole wait as it used to. A
        // root test run attaches directly and times out the same way.
        let result = wait_for_pid(1, Some(Duration::from_millis(10)));
        assert!(matches!(result, Ok(WaitForPidResult::TimedOut)));
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
        let message =
            misquoted_duration_error(&args, false).expect("should detect misquoted duration");
        assert!(message.contains("multi-word durations must be quoted"));
        assert!(message.contains("-t \"1 hour and 30 minutes\""));
    }

    #[test]
    fn bare_unit_word_after_numeric_timeout_is_flagged() {
        // A full unit word spilled past a numeric timeout is a misquote even
        // when the tail carries no digit of its own (`-t 2 hours`, `-t 1 hour`).
        let args = parse_args(&["caffeinate2", "-t", "2", "hours"]);
        assert!(misquoted_duration_error(&args, false).is_some());
        let args = parse_args(&["caffeinate2", "-t", "1", "hour"]);
        assert!(misquoted_duration_error(&args, false).is_some());
    }

    #[test]
    fn genuine_command_after_numeric_timeout_is_not_flagged() {
        // `-t 3600 -- myscript` is a legitimate timeout+command, not a misquote.
        let args = parse_args(&["caffeinate2", "-t", "3600", "--", "myscript"]);
        assert!(misquoted_duration_error(&args, true).is_none());
    }

    #[test]
    fn duration_continuation_after_separator_is_not_flagged() {
        // `caffeinate2 -t 1 -- 30 minutes` runs a command that happens to look
        // like a duration continuation on purpose. The `--` separator (reported
        // via the flag) must suppress the misquote check.
        let args = parse_args(&["caffeinate2", "-t", "1", "--", "30", "minutes"]);
        assert!(misquoted_duration_error(&args, true).is_none());
        // Without the separator the same tokens look like a misquoted duration.
        assert!(misquoted_duration_error(&args, false).is_some());
    }

    #[test]
    fn legitimate_commands_after_numeric_timeout_are_not_flagged() {
        // A bare unit-word command (`w`, the load-average tool) is duration-shaped
        // but carries no number, so it is not a misquoted duration.
        let args = parse_args(&["caffeinate2", "-t", "300", "w"]);
        assert!(misquoted_duration_error(&args, false).is_none());
        // `echo a and b` contains a non-duration token (`echo`), so even though
        // it has `and` it is a real command, not a botched duration.
        let args = parse_args(&["caffeinate2", "-t", "3600", "echo", "a", "and", "b"]);
        assert!(misquoted_duration_error(&args, false).is_none());
    }

    #[test]
    fn digit_led_command_names_are_not_flagged() {
        // A bare number is a plausible program name (the 2048 game), not a
        // duration continuation: a spilled duration always carries a unit.
        let args = parse_args(&["caffeinate2", "-t", "300", "2048"]);
        assert!(misquoted_duration_error(&args, false).is_none());
        // A digit-led name whose letters are no unit (`7z`) is a command.
        let args = parse_args(&["caffeinate2", "-t", "60", "7z"]);
        assert!(misquoted_duration_error(&args, false).is_none());
        // ... whereas a real unit suffix still reads as a spilled duration.
        let args = parse_args(&["caffeinate2", "-t", "1", "30m"]);
        assert!(misquoted_duration_error(&args, false).is_some());
        let args = parse_args(&["caffeinate2", "-t", "1", "1h30m"]);
        assert!(misquoted_duration_error(&args, false).is_some());
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
