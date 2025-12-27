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
            let duration = match unit {
                "d" | "day" | "days" => Duration::days(val),
                "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(val),
                "m" | "min" | "mins" | "minute" | "minutes" => Duration::minutes(val),
                "w" | "wk" | "wks" | "week" | "weeks" => Duration::weeks(val),
                _ => return None,
            };
            return Some((now_utc - duration).timestamp_millis());
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
            let yesterday = Local::now().date_naive() - Duration::days(1);
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
            return Some(n * 1000);
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
    }
}
