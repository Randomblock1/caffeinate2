use jiff::SignedDuration;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DurationParseError {
    #[error("Error: Timeout isn't a valid duration or number!")]
    Invalid,
    #[error("Error: Timeout is too large!")]
    TooLarge,
    #[error("Error: timeout must be positive")]
    NotPositive,
}

/// Parse a timeout duration for the CLI.
///
/// `-t` / `--timeout` is a single argument. Bare numbers are seconds; otherwise
/// humantime-style strings are accepted (quote multi-word values on the shell
/// command line, e.g. `"1 hour and 30 minutes"`).
///
/// # Errors
///
/// Returns an error if the duration string is invalid or too large.
pub fn parse_duration(duration: &str) -> Result<SignedDuration, DurationParseError> {
    let trimmed = duration.trim();

    // A comma wedged between two digits is ambiguous (decimal comma like "0,5h",
    // or a thousands separator). Silently deleting it would produce a wildly
    // wrong timeout ("0,5h" -> "05h" = 5 hours, not 30 minutes), so reject it
    // rather than guess. Connector commas (e.g. "1 hour, 30 minutes") are not
    // between digits and are still stripped below.
    let bytes = trimmed.as_bytes();
    if bytes.iter().enumerate().any(|(i, &b)| {
        b == b','
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
    }) {
        return Err(DurationParseError::Invalid);
    }
    let duration = trimmed.replace(',', "");

    match parse_human_duration(&duration) {
        Ok(std_duration) => finish_parse(
            SignedDuration::try_from(std_duration).map_err(|_| DurationParseError::TooLarge)?,
        ),
        Err(humantime::DurationError::NumberOverflow) => Err(DurationParseError::TooLarge),
        Err(_) => finish_parse(
            SignedDuration::from_secs(
                duration
                    .parse::<u64>()
                    .map_err(|_| DurationParseError::Invalid)?
                    .try_into()
                    .map_err(|_| DurationParseError::TooLarge)?,
            ),
        ),
    }
}

fn finish_parse(duration: SignedDuration) -> Result<SignedDuration, DurationParseError> {
    if duration < SignedDuration::ZERO {
        return Err(DurationParseError::NotPositive);
    }
    Ok(duration)
}

fn parse_human_duration(duration: &str) -> Result<std::time::Duration, humantime::DurationError> {
    match humantime::parse_duration(duration) {
        Ok(duration) => Ok(duration),
        Err(error) => duration_without_connectors(duration).map_or(Err(error), |normalized| {
            humantime::parse_duration(&normalized)
        }),
    }
}

fn duration_without_connectors(duration: &str) -> Option<String> {
    let mut removed_connector = false;
    let parts = duration
        .split_whitespace()
        .filter(|part| {
            if part.eq_ignore_ascii_case("and") {
                removed_connector = true;
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();

    removed_connector.then(|| parts.join(" "))
}

/// Human-readable duration for CLI messages (e.g. "1 hour 30 minutes").
#[must_use]
pub fn format_duration_human(duration: SignedDuration) -> String {
    if duration <= SignedDuration::ZERO {
        return "0 seconds".to_string();
    }

    let total_ms = duration.as_millis();
    if total_ms > 0 && total_ms < 1_000 {
        return format!(
            "{total_ms} millisecond{}",
            if total_ms == 1 { "" } else { "s" }
        );
    }

    let seconds = duration.as_secs() % 60;
    let minutes = duration.as_mins() % 60;
    let hours = duration.as_hours() % 24;
    let days = duration.as_hours() / 24;
    let mut parts = Vec::new();

    if days > 0 {
        parts.push(format!("{} day{}", days, if days == 1 { "" } else { "s" }));
    }
    if hours > 0 {
        parts.push(format!(
            "{} hour{}",
            hours,
            if hours == 1 { "" } else { "s" }
        ));
    }
    if minutes > 0 {
        parts.push(format!(
            "{} minute{}",
            minutes,
            if minutes == 1 { "" } else { "s" }
        ));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!(
            "{} second{}",
            seconds,
            if seconds == 1 { "" } else { "s" }
        ));
    }

    parts.join(" ")
}

/// Compact remaining-time label for tray tooltips (e.g. "1h 30m remaining").
#[must_use]
pub fn format_remaining_secs(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m remaining")
        } else {
            format!("{hours}h remaining")
        }
    } else if minutes > 0 {
        format!("{minutes}m remaining")
    } else {
        format!("{seconds}s remaining")
    }
}

/// Format a remaining-seconds count for the menu bar title at minute
/// granularity: `29m`, `1h 29m`, `8h`. Rounded *up*, so a fresh 30-minute
/// limit reads `30m` (not `29m`), and the final minute reads `1m` (never a
/// confusing `0m`) until expiry.
///
/// Minute granularity is deliberate — a per-second clock was tried and
/// abandoned: WindowServer composites background status items erratically
/// enough (macOS 26) that seconds visibly stutter even when every stage the
/// app controls (title set, view draw, CA commit) is provably metronomic.
#[must_use]
pub fn format_countdown_minutes(secs: u64) -> String {
    let total_minutes = secs.div_ceil(60);
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;

    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 1;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn format_duration_human_shows_sub_second_durations() {
        assert_eq!(
            format_duration_human(SignedDuration::from_millis(250)),
            "250 milliseconds"
        );
        assert_eq!(
            format_duration_human(SignedDuration::from_millis(1)),
            "1 millisecond"
        );
    }

    #[test]
    fn format_duration_human_omits_zero_components() {
        assert_eq!(
            format_duration_human(SignedDuration::from_secs(0)),
            "0 seconds"
        );
        assert_eq!(
            format_duration_human(SignedDuration::from_secs(60)),
            "1 minute"
        );
        assert_eq!(
            format_duration_human(SignedDuration::from_secs(3661)),
            "1 hour 1 minute 1 second"
        );
        assert_eq!(
            format_duration_human(SignedDuration::from_secs(90_061)),
            "1 day 1 hour 1 minute 1 second"
        );
    }

    #[test]
    fn format_remaining_secs_display() {
        assert_eq!(format_remaining_secs(45), "45s remaining");
        assert_eq!(format_remaining_secs(90), "1m remaining");
        assert_eq!(format_remaining_secs(3661), "1h 1m remaining");
    }

    #[test]
    fn format_countdown_minutes_display() {
        // Rounds up: the final minute shows "1m", never "0m".
        assert_eq!(format_countdown_minutes(1), "1m");
        assert_eq!(format_countdown_minutes(45), "1m");
        assert_eq!(format_countdown_minutes(60), "1m");
        assert_eq!(format_countdown_minutes(61), "2m");
        // A fresh 30-minute limit reads "30m" for the whole first minute.
        assert_eq!(format_countdown_minutes(1799), "30m");
        assert_eq!(format_countdown_minutes(1800), "30m");
        assert_eq!(format_countdown_minutes(3600), "1h");
        assert_eq!(format_countdown_minutes(3601), "1h 1m");
        assert_eq!(format_countdown_minutes(5400), "1h 30m");
        assert_eq!(format_countdown_minutes(8 * 3600), "8h");
    }

    #[test]
    fn test_parse_duration_valid_strings() {
        let duration = "1d 2h 3m 4s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(
            result.as_secs(),
            DAY + 2 * HOUR + 3 * MINUTE + 4 * SECOND
        );

        let duration = "1day 2h 3m";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), DAY + 2 * HOUR + 3 * MINUTE);

        let duration = "3min 17h 2s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), 17 * HOUR + 3 * MINUTE + 2 * SECOND);

        let duration = "1 hour and 30 minutes";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), HOUR + 30 * MINUTE);

        // Connector commas (not between digits) are stripped, not rejected.
        let duration = "1 hour, 30 minutes";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), HOUR + 30 * MINUTE);
    }

    #[test]
    fn test_parse_duration_rejects_ambiguous_comma() {
        // A comma between digits (decimal comma / thousands separator) must not
        // be silently deleted: "0,5h" must not become "05h" (5 hours).
        for duration in ["0,5h", "2,5m", "1,5h", "1,000"] {
            assert_eq!(
                parse_duration(duration).unwrap_err(),
                DurationParseError::Invalid,
                "{duration:?}"
            );
        }
    }

    #[test]
    fn test_parse_duration_accepts_zero() {
        assert_eq!(parse_duration("0").unwrap().as_secs(), 0);
        assert_eq!(parse_duration("0s").unwrap().as_secs(), 0);
    }

    #[test]
    fn test_parse_duration_valid_numbers() {
        let duration = "45323";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), 45323);

        let duration = "60";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), 60 * SECOND);
    }

    #[test]
    fn test_parse_duration_edge_cases() {
        let duration = "1000000s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), 1_000_000);

        let duration = "  15m  ";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), 15 * MINUTE);

        let duration = "1.5h";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), HOUR + 30 * MINUTE);

        let duration = "250ms";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_millis(), 250);
    }

    #[test]
    fn test_parse_duration_errors() {
        for duration in ["", "   ", "invalid", "-1", "1 lightyear", "3 movies"] {
            let result = parse_duration(duration);
            assert_eq!(
                result.unwrap_err(),
                DurationParseError::Invalid,
                "{duration:?}"
            );
        }

        let duration = "10000000000000000000";
        let result = parse_duration(duration);
        assert_eq!(result.unwrap_err(), DurationParseError::TooLarge);

        let duration = "100000000000000000000s";
        let result = parse_duration(duration);
        assert_eq!(result.unwrap_err(), DurationParseError::TooLarge);
    }
}
