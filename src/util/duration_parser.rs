use jiff::SignedDuration;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DurationParseError {
    #[error("Error: Timeout isn't a valid duration or number!")]
    Invalid,
    #[error("Error: Timeout is too large!")]
    TooLarge,
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

    // A comma adjacent to a digit on the side facing a number is ambiguous
    // (decimal comma like "0,5h", a thousands separator like "1,000", or a
    // truncated/malformed value like ",5" or "5,"). Silently deleting it
    // could produce a wildly wrong timeout ("0,5h" -> "05h" = 5 hours, not 30
    // minutes) or hide a typo, so reject it rather than guess. A side counts
    // as facing a digit when the neighboring char is a digit, or when there
    // is no neighboring char because the comma sits at the string edge.
    // Connector commas (e.g. "1 hour, 30 minutes") sit between a letter and a
    // space on both sides and are still stripped below.
    let bytes = trimmed.as_bytes();
    if bytes.iter().enumerate().any(|(i, &b)| {
        b == b','
            && (i == 0 || bytes[i - 1].is_ascii_digit())
            && bytes.get(i + 1).is_none_or(u8::is_ascii_digit)
    }) {
        return Err(DurationParseError::Invalid);
    }
    let duration = trimmed.replace(',', "");

    match parse_human_duration(&duration) {
        Ok(std_duration) => {
            SignedDuration::try_from(std_duration).map_err(|_| DurationParseError::TooLarge)
        }
        Err(humantime::DurationError::NumberOverflow) => Err(DurationParseError::TooLarge),
        Err(_) => Ok(SignedDuration::from_secs(
            duration
                .parse::<u64>()
                .map_err(|_| DurationParseError::Invalid)?
                .try_into()
                .map_err(|_| DurationParseError::TooLarge)?,
        )),
    }
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

/// Split a remaining-seconds count into whole hours and minutes, rounding the
/// minutes *up* so any partial minute reads as a full one. Shared by the
/// menu-bar title and the tooltip so the two simultaneously-visible labels
/// always agree at minute granularity (a floor in one and a ceil in the other
/// disagree by a full minute for almost the whole countdown).
const fn ceil_hours_minutes(secs: u64) -> (u64, u64) {
    let total_minutes = secs.div_ceil(60);
    (total_minutes / 60, total_minutes % 60)
}

/// Compact remaining-time label for tray tooltips (e.g. "1h 30m remaining").
/// Minutes are rounded *up* to match the menu-bar title (see
/// [`format_countdown_minutes`]); sub-minute values keep second granularity,
/// which is a finer resolution than the title, not a disagreement with it.
#[must_use]
pub fn format_remaining_secs(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s remaining");
    }

    let (hours, minutes) = ceil_hours_minutes(secs);
    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m remaining")
        } else {
            format!("{hours}h remaining")
        }
    } else {
        format!("{minutes}m remaining")
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
    let (hours, minutes) = ceil_hours_minutes(secs);

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
        // Sub-minute keeps second granularity.
        assert_eq!(format_remaining_secs(45), "45s remaining");
        // Minute granularity rounds up, matching the menu-bar title.
        assert_eq!(format_remaining_secs(90), "2m remaining");
        assert_eq!(format_remaining_secs(3661), "1h 2m remaining");
    }

    #[test]
    fn title_and_tooltip_agree_at_minute_granularity() {
        // The menu-bar title and the tooltip are visible at the same time; once
        // past the sub-minute range they must render the identical hours/minutes
        // (the tooltip merely appends " remaining"), never off by a minute.
        for secs in [60, 61, 90, 1799, 1800, 3599, 3600, 3601, 3661, 5400] {
            assert_eq!(
                format_remaining_secs(secs),
                format!("{} remaining", format_countdown_minutes(secs)),
                "secs={secs}"
            );
        }
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

        // Connector commas (not adjacent to a digit on both relevant sides)
        // are stripped, not rejected.
        let duration = "1 hour, 30 minutes";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), HOUR + 30 * MINUTE);

        let duration = "1h,30m";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.as_secs(), HOUR + 30 * MINUTE);
    }

    #[test]
    fn test_parse_duration_rejects_ambiguous_comma() {
        // A comma between digits (decimal comma / thousands separator) must not
        // be silently deleted: "0,5h" must not become "05h" (5 hours).
        // A comma at the string edge next to a digit is equally ambiguous
        // (truncated/malformed input) and must not be silently stripped:
        // ",5" and "5," must not become "5". A lone "," must also be rejected.
        for duration in ["0,5h", "2,5m", "1,5h", "1,000", ",5", "5,", ","] {
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
