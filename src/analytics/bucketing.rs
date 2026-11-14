//! Time-bucket conversions for analytics.
//!
//! Converts between integer day_id / hour_id keys (as stored in SQLite rollup
//! tables) and human-readable ISO date strings used in JSON output.

use crate::storage::sqlite::FrankenStorage;

use super::types::AnalyticsFilter;

/// Format a `day_id` as an ISO date string (`YYYY-MM-DD`).
pub fn day_id_to_iso(day_id: i64) -> String {
    use chrono::{TimeZone, Utc};
    let ms = FrankenStorage::millis_from_day_id(day_id);
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| format!("day:{day_id}"))
}

/// Format an `hour_id` as an ISO datetime string (`YYYY-MM-DDTHH:00Z`).
pub fn hour_id_to_iso(hour_id: i64) -> String {
    use chrono::{TimeZone, Utc};
    let ms = FrankenStorage::millis_from_hour_id(hour_id);
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%dT%H:00Z").to_string())
        .unwrap_or_else(|| format!("hour:{hour_id}"))
}

/// Compute the ISO week key (`YYYY-Www`) from a `day_id`.
pub fn day_id_to_iso_week(day_id: i64) -> String {
    use chrono::{Datelike, TimeZone, Utc};
    let ms = FrankenStorage::millis_from_day_id(day_id);
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| {
            let iso = dt.iso_week();
            format!("{}-W{:02}", iso.year(), iso.week())
        })
        .unwrap_or_else(|| format!("day:{day_id}"))
}

/// Compute the month key (`YYYY-MM`) from a `day_id`.
pub fn day_id_to_month(day_id: i64) -> String {
    use chrono::{TimeZone, Utc};
    let ms = FrankenStorage::millis_from_day_id(day_id);
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m").to_string())
        .unwrap_or_else(|| format!("day:{day_id}"))
}

/// Resolve the time-range from [`AnalyticsFilter`] into an inclusive
/// `(min_day_id, max_day_id)` range.  Returns `(None, None)` when no time
/// filter is active.
pub fn resolve_day_range(filter: &AnalyticsFilter) -> (Option<i64>, Option<i64>) {
    let min_day = filter.since_ms.map(FrankenStorage::day_id_from_millis);
    let max_day = filter.until_ms.map(FrankenStorage::day_id_from_millis);
    (min_day, max_day)
}

/// Resolve the time-range from [`AnalyticsFilter`] into an inclusive
/// `(min_hour_id, max_hour_id)` range.
pub fn resolve_hour_range(filter: &AnalyticsFilter) -> (Option<i64>, Option<i64>) {
    let min_hour = filter.since_ms.map(FrankenStorage::hour_id_from_millis);
    let max_hour = filter.until_ms.map(FrankenStorage::hour_id_from_millis);
    (min_hour, max_hour)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Known epoch: 2025-01-15 00:00:00 UTC → day_id = 1736899200000 / 86400000 = 20102
    // Actually let's use the FrankenStorage functions to get the right day_id.

    #[test]
    fn day_id_roundtrip() {
        // 2025-06-15 00:00:00 UTC = 1749945600000 ms
        let ms = 1_749_945_600_000_i64;
        let day_id = FrankenStorage::day_id_from_millis(ms);
        let iso = day_id_to_iso(day_id);
        assert_eq!(iso, "2025-06-15");
    }

    #[test]
    fn hour_id_roundtrip() {
        // 2025-06-15 14:00:00 UTC = 1749996000000 ms
        let ms = 1_749_996_000_000_i64;
        let hour_id = FrankenStorage::hour_id_from_millis(ms);
        let iso = hour_id_to_iso(hour_id);
        assert_eq!(iso, "2025-06-15T14:00Z");
    }

    #[test]
    fn day_id_to_week() {
        // 2025-01-06 (Monday) and 2025-01-12 (Sunday) are in ISO week 2025-W02
        let mon_ms = 1_736_121_600_000_i64; // 2025-01-06 00:00 UTC
        let sun_ms = 1_736_640_000_000_i64; // 2025-01-12 00:00 UTC
        let mon_id = FrankenStorage::day_id_from_millis(mon_ms);
        let sun_id = FrankenStorage::day_id_from_millis(sun_ms);
        assert_eq!(day_id_to_iso_week(mon_id), day_id_to_iso_week(sun_id));
        assert!(day_id_to_iso_week(mon_id).contains("W02"));
    }

    #[test]
    fn day_id_to_month_format() {
        let ms = 1_750_032_000_000_i64; // 2025-06-15
        let day_id = FrankenStorage::day_id_from_millis(ms);
        assert_eq!(day_id_to_month(day_id), "2025-06");
    }

    #[test]
    fn resolve_empty_filter_gives_none() {
        let f = AnalyticsFilter::default();
        let (min, max) = resolve_day_range(&f);
        assert!(min.is_none());
        assert!(max.is_none());
        let (hmin, hmax) = resolve_hour_range(&f);
        assert!(hmin.is_none());
        assert!(hmax.is_none());
    }

    #[test]
    fn resolve_day_range_with_since() {
        let f = AnalyticsFilter {
            since_ms: Some(1_750_032_000_000),
            ..Default::default()
        };
        let (min, max) = resolve_day_range(&f);
        assert!(min.is_some());
        assert!(max.is_none());
    }

    #[test]
    fn unrepresentable_bucket_ids_use_fallback_labels() {
        for id in [i64::MIN, i64::MAX] {
            let day_label = format!("day:{id}");
            assert_eq!(day_id_to_iso(id), day_label);
            assert_eq!(day_id_to_iso_week(id), day_label);
            assert_eq!(day_id_to_month(id), day_label);
            assert_eq!(hour_id_to_iso(id), format!("hour:{id}"));
        }
    }

    #[test]
    fn out_of_range_rollups_preserve_timeseries_values() -> anyhow::Result<()> {
        use crate::analytics::{query::query_tokens_timeseries, types::GroupBy};
        use crate::franken_sync::compat::ConnectionExt;

        let storage = FrankenStorage::open(std::path::Path::new(":memory:"))?;
        for id in [i64::MIN, 0, i64::MAX] {
            storage.raw().execute_compat(
                "INSERT INTO usage_daily (
                    day_id, agent_slug, source_id, message_count, content_tokens_est_total
                 ) VALUES (?1, 'codex', 'local', 1, 7)",
                &[id.into()],
            )?;
            storage.raw().execute_compat(
                "INSERT INTO usage_hourly (
                    hour_id, agent_slug, source_id, message_count, content_tokens_est_total
                 ) VALUES (?1, 'codex', 'local', 1, 7)",
                &[id.into()],
            )?;
        }

        for (group_by, prefix, ordinary_label) in [
            (GroupBy::Hour, "hour", "2020-01-01T00:00Z"),
            (GroupBy::Day, "day", "2020-01-01"),
            (GroupBy::Week, "day", "2020-W01"),
            (GroupBy::Month, "day", "2020-01"),
        ] {
            let result =
                query_tokens_timeseries(storage.raw(), &AnalyticsFilter::default(), group_by)?;
            assert_eq!(result.path, "rollup");
            assert_eq!(result.buckets.len(), 3, "{group_by:?}");
            assert_eq!(result.totals.message_count, 3, "{group_by:?}");
            assert_eq!(result.totals.content_tokens_est_total, 21, "{group_by:?}");
            for label in [
                format!("{prefix}:{}", i64::MIN),
                ordinary_label.to_string(),
                format!("{prefix}:{}", i64::MAX),
            ] {
                assert!(
                    result.buckets.iter().any(|(key, bucket)| key == &label
                        && bucket.message_count == 1
                        && bucket.content_tokens_est_total == 7),
                    "{group_by:?}: missing or altered bucket {label}"
                );
            }
        }
        Ok(())
    }
}
