pub fn parse_duration(duration: &str) -> Result<chrono::Duration, String> {
    let duration = duration.trim();

    match humantime::parse_duration(duration) {
        Ok(std_duration) => chrono::Duration::from_std(std_duration)
            .map_err(|_| "Error: Timeout is too large!".to_string()),
        Err(_) => {
            let seconds = duration.parse::<u64>().map_err(|_| {
                "Error: Timeout isn't a valid duration or number!".to_string()
            })?;

            chrono::Duration::try_seconds(seconds.try_into().map_err(|_| {
                "Error: Timeout is too large!".to_string()
            })?)
            .ok_or_else(|| "Error: Timeout is too large!".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_valid_strings() {
        let duration = "1d 2h 3m 4s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 93784);

        let duration = "1day 2h 3m";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 93780);

        let duration = "3min 17h 2s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 61382);
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
        assert_eq!(result.num_seconds(), 60);
    }

    #[test]
    fn test_parse_duration_edge_cases() {
        // Test 0s
        let duration = "0s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 0);

        // Test large number
        let duration = "1000000s";
        let result = parse_duration(duration).unwrap();
        assert_eq!(result.num_seconds(), 1000000);
    }

    #[test]
    fn test_parse_duration_errors() {
        // Invalid format
        let duration = "invalid";
        let result = parse_duration(duration);
        assert_eq!(
            result.unwrap_err(),
            "Error: Timeout isn't a valid duration or number!"
        );

        // Too large number (overflows i64 for chrono::Duration)
        // i64::MAX is roughly 9.22e18.
        // 10000000000000000000 is > i64::MAX
        let duration = "10000000000000000000";
        let result = parse_duration(duration);
        assert_eq!(
            result.unwrap_err(),
            "Error: Timeout is too large!"
        );
    }
}
