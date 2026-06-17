pub fn parse_duration(duration: &str) -> Result<chrono::Duration, String> {
    let duration = duration.trim();

    match parse_human_duration(duration) {
        Ok(std_duration) => chrono::Duration::from_std(std_duration)
            .map_err(|_| "Error: Timeout is too large!".to_string()),
        Err(humantime::DurationError::NumberOverflow) => {
            Err("Error: Timeout is too large!".to_string())
        }
        Err(_) => {
            let seconds = duration
                .parse::<u64>()
                .map_err(|_| "Error: Timeout isn't a valid duration or number!".to_string())?;

            chrono::Duration::try_seconds(
                seconds
                    .try_into()
                    .map_err(|_| "Error: Timeout is too large!".to_string())?,
            )
            .ok_or_else(|| "Error: Timeout is too large!".to_string())
        }
    }
}

fn parse_human_duration(duration: &str) -> Result<std::time::Duration, humantime::DurationError> {
    match humantime::parse_duration(duration) {
        Ok(duration) => Ok(duration),
        Err(error) => match duration_without_connectors(duration) {
            Some(normalized) => humantime::parse_duration(&normalized),
            None => Err(error),
        },
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
pub fn format_duration_human(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds() % 60;
    let minutes = duration.num_minutes() % 60;
    let hours = duration.num_hours() % 24;
    let days = duration.num_days();
    let mut parts = Vec::new();

    if days > 0 {
        parts.push(format!("{} day{}", days, if days != 1 { "s" } else { "" }));
    }
    if hours > 0 {
        parts.push(format!(
            "{} hour{}",
            hours,
            if hours != 1 { "s" } else { "" }
        ));
    }
    if minutes > 0 {
        parts.push(format!(
            "{} minute{}",
            minutes,
            if minutes != 1 { "s" } else { "" }
        ));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!(
            "{} second{}",
            seconds,
            if seconds != 1 { "s" } else { "" }
        ));
    }

    parts.join(" ")
}

/// Compact remaining-time label for tray tooltips (e.g. "1h 30m remaining").
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

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 1;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn format_duration_human_omits_zero_components() {
        assert_eq!(
            format_duration_human(chrono::Duration::try_seconds(0).unwrap()),
            "0 seconds"
        );
        assert_eq!(
            format_duration_human(chrono::Duration::try_seconds(60).unwrap()),
            "1 minute"
        );
        assert_eq!(
            format_duration_human(chrono::Duration::try_seconds(3661).unwrap()),
            "1 hour 1 minute 1 second"
        );
        assert_eq!(
            format_duration_human(chrono::Duration::try_seconds(90_061).unwrap()),
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
    fn test_parse_duration_valid_strings() {
        let duration = "1d 2h 3m 4s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(
            result.num_seconds(),
            DAY + 2 * HOUR + 3 * MINUTE + 4 * SECOND
        );

        let duration = "1day 2h 3m";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), DAY + 2 * HOUR + 3 * MINUTE);

        let duration = "3min 17h 2s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 17 * HOUR + 3 * MINUTE + 2 * SECOND);

        let duration = "1 hour and 30 minutes";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), HOUR + 30 * MINUTE);
    }

    #[test]
    fn test_parse_duration_valid_numbers() {
        let duration = "45323";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 45323);

        let duration = "0";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 0);

        let duration = "60";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 60 * SECOND);
    }

    #[test]
    fn test_parse_duration_edge_cases() {
        let duration = "0s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 0);

        let duration = "1000000s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 1000000);

        let duration = "  15m  ";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 15 * MINUTE);

        let duration = "1.5h";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), HOUR + 30 * MINUTE);

        let duration = "250ms";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_milliseconds(), 250);
    }

    #[test]
    fn test_parse_duration_errors() {
        for duration in ["", "   ", "invalid", "-1", "1 lightyear", "3 movies"] {
            let result = parse_duration(duration);
            assert_eq!(
                result.unwrap_err(),
                "Error: Timeout isn't a valid duration or number!",
                "{duration:?}"
            );
        }

        let duration = "10000000000000000000";
        let result = parse_duration(duration);
        assert_eq!(result.unwrap_err(), "Error: Timeout is too large!");

        let duration = "100000000000000000000s";
        let result = parse_duration(duration);
        assert_eq!(result.unwrap_err(), "Error: Timeout is too large!");
    }
}
