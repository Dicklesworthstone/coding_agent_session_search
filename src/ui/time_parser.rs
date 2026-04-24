use chrono::{Duration, Local, LocalResult, NaiveDate, TimeZone, Utc};

/// Parses human-readable time input into a UTC timestamp (milliseconds).
///
/// Supported formats:
/// - Relative: "-7d", "-24h", "-30m", "-1w"
/// - Keywords: "now", "today", "yesterday"
/// - ISO dates: "2024-11-25", "2024-11-25T14:30:00Z"
/// - Date formats: "YYYY-MM-DD", "YYYY/MM/DD", "MM/DD/YYYY", "MM-DD-YYYY"
/// - Unix timestamp: seconds (if < 10^11) or milliseconds
pub fn parse_time_input(input: &str) -> Option<i64> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return None;
    }

    let now_utc = Utc::now();
    let now_ms = now_utc.timestamp_millis();

    // Relative: -7d, -24h, -1w, -30m
    if let Some(stripped) = input.strip_prefix('-') {
        let val_str: String = stripped.chars().take_while(|c| c.is_numeric()).collect();
        if let Ok(val) = val_str.parse::<i64>() {
            let unit = stripped.trim_start_matches(&val_str).trim();
            let duration = relative_duration(unit, val)?;
            return subtract_duration_ms(now_utc, duration);
        }
    }

    // Relative: 7d, 24h, 1w, 30m (no leading '-')
    {
        let val_str: String = input.chars().take_while(|c| c.is_numeric()).collect();
        if !val_str.is_empty() {
            let unit = input.trim_start_matches(&val_str).trim();
            if !unit.is_empty()
                && let Ok(val) = val_str.parse::<i64>()
            {
                let duration = relative_duration(unit, val);
                if let Some(duration) = duration {
                    return subtract_duration_ms(now_utc, duration);
                }
            }
        }
    }

    // Relative: "30 days ago", "2 weeks ago", "1 hour ago"
    {
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.len() == 3
            && parts[2] == "ago"
            && let Ok(val) = parts[0].parse::<i64>()
        {
            let duration = relative_duration(parts[1], val);
            if let Some(duration) = duration {
                return subtract_duration_ms(now_utc, duration);
            }
        }
        if parts.len() == 2 && parts[1] == "ago" {
            let val_str: String = parts[0].chars().take_while(|c| c.is_numeric()).collect();
            if let Ok(val) = val_str.parse::<i64>() {
                let unit = parts[0].trim_start_matches(&val_str);
                let duration = relative_duration(unit, val);
                if let Some(duration) = duration {
                    return subtract_duration_ms(now_utc, duration);
                }
            }
        }
    }

    // Keywords
    match input.as_str() {
        "now" => return Some(now_ms),
        "today" => {
            let today = Local::now().date_naive();
            return local_midnight_to_utc(today);
        }
        "yesterday" => {
            let yesterday = Local::now()
                .date_naive()
                .checked_sub_signed(Duration::try_days(1)?)?;
            return local_midnight_to_utc(yesterday);
        }
        _ => {}
    }

    // ISO date formats (RFC3339)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&input) {
        return Some(dt.timestamp_millis());
    }

    // YYYY-MM-DD or YYYY/MM/DD (Local midnight)
    if let Ok(date) = NaiveDate::parse_from_str(&input, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(&input, "%Y/%m/%d"))
    {
        return local_midnight_to_utc(date);
    }

    // US Formats: MM/DD/YYYY or MM-DD-YYYY
    if let Ok(date) = NaiveDate::parse_from_str(&input, "%m/%d/%Y")
        .or_else(|_| NaiveDate::parse_from_str(&input, "%m-%d-%Y"))
    {
        return local_midnight_to_utc(date);
    }
    // Numeric fallback (ms or seconds)
    if let Ok(n) = input.parse::<i64>() {
        // Heuristic: timestamps < 10^11 (year 5138) are likely seconds.
        if n < 100_000_000_000 {
            return n.checked_mul(1000);
        }
        return Some(n);
    }

    None
}

fn local_midnight_to_utc(date: NaiveDate) -> Option<i64> {
    let dt = date.and_hms_opt(0, 0, 0)?;
    let local = match Local.from_local_datetime(&dt) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => {
            // Fall back to treating the naive datetime as UTC for DST gaps.
            return Some(Utc.from_utc_datetime(&dt).timestamp_millis());
        }
    };
    Some(local.with_timezone(&Utc).timestamp_millis())
}

fn relative_duration(unit: &str, val: i64) -> Option<Duration> {
    match unit {
        "d" | "day" | "days" => Duration::try_days(val),
        "h" | "hr" | "hrs" | "hour" | "hours" => Duration::try_hours(val),
        "m" | "min" | "mins" | "minute" | "minutes" => Duration::try_minutes(val),
        "w" | "wk" | "wks" | "week" | "weeks" => Duration::try_weeks(val),
        _ => None,
    }
}

fn subtract_duration_ms(now_utc: chrono::DateTime<Utc>, duration: Duration) -> Option<i64> {
    now_utc
        .checked_sub_signed(duration)
        .map(|value| value.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relative_time() {
        let now = Utc::now().timestamp_millis();
        let tolerance = 60 * 1000; // 1 minute

        // -1h
        let t1 = parse_time_input("-1h").unwrap();
        let diff = now - t1;
        assert!((diff - 3600 * 1000).abs() < tolerance);

        // -1d
        let t2 = parse_time_input("-1d").unwrap();
        let diff = now - t2;
        assert!((diff - 86400 * 1000).abs() < tolerance);

        // 7d (no leading '-')
        let t3 = parse_time_input("7d").unwrap();
        let diff = now - t3;
        assert!((diff - 7 * 86400 * 1000).abs() < tolerance);

        // 30 days ago
        let t4 = parse_time_input("30 days ago").unwrap();
        let diff = now - t4;
        assert!((diff - 30 * 86400 * 1000).abs() < tolerance);

        // 2 weeks ago
        let t5 = parse_time_input("2 weeks ago").unwrap();
        let diff = now - t5;
        assert!((diff - 14 * 86400 * 1000).abs() < tolerance);
    }

    #[test]
    fn test_relative_time_overflow_returns_none() {
        let max = i64::MAX;
        let inputs = [
            format!("{max}d"),
            format!("{max}h"),
            format!("{max}m"),
            format!("{max}w"),
            format!("-{max}d"),
            format!("{max} days ago"),
            format!("{max}h ago"),
        ];

        for input in inputs {
            assert_eq!(parse_time_input(&input), None, "{input}");
        }

        let duration = Duration::try_milliseconds(i64::MAX).unwrap();
        assert_eq!(
            subtract_duration_ms(chrono::DateTime::<Utc>::MIN_UTC, duration),
            None
        );
    }

    #[test]
    fn test_keywords() {
        assert!(parse_time_input("now").is_some());
        let today = parse_time_input("today").unwrap();
        let yesterday = parse_time_input("yesterday").unwrap();
        assert!(today > yesterday);
        let diff = today - yesterday;
        let min = 23 * 60 * 60 * 1000;
        let max = 25 * 60 * 60 * 1000;
        assert!(
            diff >= min && diff <= max,
            "expected 23-25h difference due to DST, got {} ms",
            diff
        );
    }

    #[test]
    fn test_date_formats() {
        // Just check they parse
        assert!(parse_time_input("2023-01-01").is_some());
        assert!(parse_time_input("2023/01/01").is_some());
        assert!(parse_time_input("01/01/2023").is_some());
        assert!(parse_time_input("01-01-2023").is_some());
    }

    #[test]
    fn test_numeric() {
        let _sec = 1700000000;
        let ms = 1700000000000;
        assert_eq!(parse_time_input("1700000000").unwrap(), ms);
        assert_eq!(parse_time_input("1700000000000").unwrap(), ms);
        assert_eq!(parse_time_input(&i64::MIN.to_string()), None);
    }
}
