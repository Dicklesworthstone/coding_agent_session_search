//! Bounded, read-only receipt caching for live watch scans.
//!
//! Whole-source receipts are reuse optimizations. Codex prefix evidence also
//! protects canonical message indices from divergent replay, so a bounded
//! omission must be distinguished from genuine absence before admission. A
//! large archive or malformed oversized receipt must not require an unbounded
//! metadata allocation before watch ingestion can begin.

use crate::franken_sync::compat::{ConnectionExt, OptionalExtension, ParamValue, RowExt};
use crate::storage::sqlite::FrankenStorage;
use anyhow::Result;
use std::collections::HashMap;

pub(super) const MAX_WATCH_SOURCE_LEDGER_ENTRIES: usize = 65_536;
pub(super) const MAX_WATCH_SOURCE_LEDGER_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES: usize = 256 * 1024;
const MAX_WATCH_SOURCE_LEDGER_KEY_BYTES: usize = 512;
const WATCH_SOURCE_LEDGER_PAGE_ROWS: usize = 256;
const SOURCE_LEDGER_PREFIX: &str = "source_ingest_v1:";
pub(super) const INCOMPLETE_SOURCE_LEDGER_KEY: &str = "cass_watch_source_cache_incomplete";

/// A cache miss is not an absence proof when bounded loading omitted rows.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SourceObservationLookup {
    Absent,
    Observation(String),
    Unusable,
}

/// Resolve one omitted key without growing the cache or returning oversized
/// metadata values to the caller. SQL failures never mean absent evidence.
pub(super) fn lookup_watch_source_observation(
    storage: &FrankenStorage,
    key: &str,
    progress: Option<&super::IndexingProgress>,
) -> Result<SourceObservationLookup> {
    anyhow::ensure!(
        key.len() <= MAX_WATCH_SOURCE_LEDGER_KEY_BYTES
            && key.strip_prefix(SOURCE_LEDGER_PREFIX).is_some_and(|suffix| !suffix.is_empty()),
        "invalid watch source observation key"
    );
    if let Some(progress) = progress {
        progress.check_stop()?;
    }
    let value: Option<Option<String>> = storage
        .raw()
        .query_row_map(
            "SELECT CASE WHEN typeof(value) = 'text' \
                              AND length(CAST(value AS BLOB)) <= ?2 \
                         THEN value ELSE NULL END \
             FROM meta WHERE key = ?1 LIMIT 1",
            &[
                ParamValue::from(key),
                ParamValue::from(MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES as i64),
            ],
            |row| row.get_typed::<Option<String>>(0),
        )
        .optional()?;
    if let Some(progress) = progress {
        progress.tick_activity();
        progress.check_stop()?;
    }
    Ok(match value {
        None => SourceObservationLookup::Absent,
        Some(Some(value)) if value.len() <= MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES => {
            SourceObservationLookup::Observation(value)
        }
        Some(_) => SourceObservationLookup::Unusable,
    })
}

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: usize,
    value_bytes: usize,
    key_bytes: usize,
    page_rows: usize,
}

/// Cache only a bounded prefix of source observations plus one fixed-size
/// completeness marker. Omitted rows remain in canonical storage and are never
/// acknowledged or deleted by this reader.
pub(super) fn load_bounded_watch_source_ledger(
    storage: &FrankenStorage,
    progress: Option<&super::IndexingProgress>,
) -> Result<HashMap<String, String>> {
    let (mut entries, complete) = load_with_completeness(
        storage,
        Limits {
            entries: MAX_WATCH_SOURCE_LEDGER_ENTRIES,
            bytes: MAX_WATCH_SOURCE_LEDGER_BYTES,
            value_bytes: MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES,
            key_bytes: MAX_WATCH_SOURCE_LEDGER_KEY_BYTES,
            page_rows: WATCH_SOURCE_LEDGER_PAGE_ROWS,
        },
        progress,
    )?;
    if !complete {
        // A missing optimization receipt may be reparsed. A missing prefix
        // high-water proof cannot: its absence must not authorize divergent
        // data at indices which canonical storage already contains.
        entries.insert(INCOMPLETE_SOURCE_LEDGER_KEY.to_string(), String::new());
    }
    Ok(entries)
}

#[cfg(test)]
fn load_with_limits(
    storage: &FrankenStorage,
    limits: Limits,
    progress: Option<&super::IndexingProgress>,
) -> Result<HashMap<String, String>> {
    load_with_completeness(storage, limits, progress).map(|(cache, _)| cache)
}

fn load_with_completeness(
    storage: &FrankenStorage,
    limits: Limits,
    progress: Option<&super::IndexingProgress>,
) -> Result<(HashMap<String, String>, bool)> {
    let mut cache = HashMap::new();
    let mut after = SOURCE_LEDGER_PREFIX.to_string();
    let mut bytes = 0usize;
    let mut examined = 0usize;
    let mut reached_end = false;
    let mut omitted = false;
    while examined < limits.entries && limits.page_rows > 0 {
        if let Some(progress) = progress {
            progress.check_stop()?;
        }
        let remaining = limits.bytes.saturating_sub(bytes);
        let value_bytes = limits
            .value_bytes
            .min(remaining.saturating_sub(limits.key_bytes));
        if value_bytes == 0 {
            break;
        }
        // Account for a whole page BEFORE requesting it. The existing cache
        // plus materialized page strings fit the payload limit; cursor copies
        // and container overhead are separately bounded by key/entry limits.
        let page_rows = (remaining / (limits.key_bytes + value_bytes))
            .max(1)
            .min(limits.page_rows)
            .min(limits.entries - examined);
        let page: Vec<(String, Option<String>)> = storage.raw().query_map_collect(
            "SELECT key, \
                    CASE WHEN typeof(value) = 'text' \
                         AND length(CAST(value AS BLOB)) <= ?3 \
                         THEN value ELSE NULL END \
             FROM meta \
             WHERE key > ?1 AND key < 'source_ingest_v1;' \
               AND length(CAST(key AS BLOB)) <= ?2 \
             ORDER BY key LIMIT ?4",
            &[
                ParamValue::from(after.as_str()),
                ParamValue::from(i64::try_from(limits.key_bytes)?),
                ParamValue::from(i64::try_from(value_bytes)?),
                ParamValue::from(i64::try_from(page_rows)?),
            ],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )?;
        if let Some(progress) = progress {
            progress.tick_activity();
            progress.check_stop()?;
        }
        let Some((last_key, _)) = page.last() else {
            reached_end = true;
            break;
        };
        anyhow::ensure!(
            last_key > &after,
            "watch source cache cursor did not advance"
        );
        after = last_key.clone();
        let page_len = page.len();
        examined = examined.saturating_add(page_len);
        for (key, value) in page {
            let Some(value) = value else {
                omitted = true;
                continue;
            };
            // String::len counts UTF-8 bytes, matching the SQL BLOB length.
            // Keep the guard even if a backend returns unexpected coercions.
            let entry_bytes = key.len().saturating_add(value.len());
            if entry_bytes <= limits.bytes.saturating_sub(bytes) {
                bytes += entry_bytes;
                cache.insert(key, value);
            } else {
                omitted = true;
            }
        }
        if page_len < page_rows {
            reached_end = true;
            break;
        }
    }
    tracing::debug!(
        cached_sources = cache.len(),
        source_rows_examined = examined,
        cached_payload_bytes = bytes,
        "loaded bounded watch source receipts"
    );
    Ok((cache, reached_end && !omitted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn insert(storage: &FrankenStorage, suffix: &str, value: &str) -> Result<String> {
        let key = format!("{SOURCE_LEDGER_PREFIX}{suffix}");
        storage.raw().execute_compat(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            &[ParamValue::from(key.as_str()), ParamValue::from(value)],
        )?;
        Ok(key)
    }

    fn small_limits() -> Limits {
        Limits {
            entries: 32,
            bytes: 4 * 1024,
            value_bytes: 64,
            key_bytes: 64,
            page_rows: 2,
        }
    }

    #[test]
    fn codex_integrity_exact_lookup_distinguishes_absent_bounded_and_unusable_rows() -> Result<()> {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        assert_eq!(
            lookup_watch_source_observation(&storage, "source_ingest_v1:absent", None)?,
            SourceObservationLookup::Absent,
        );
        let limit = MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES;
        let exact = format!("{}{}", "雪".repeat(limit / 3), "x".repeat(limit % 3));
        assert_eq!(exact.len(), limit);
        let exact_key = insert(&storage, "bounded", &exact)?;
        assert_eq!(
            lookup_watch_source_observation(&storage, &exact_key, None)?,
            SourceObservationLookup::Observation(exact.clone()),
        );
        let oversized = format!("{exact}雪");
        assert!(oversized.chars().count() < limit);
        let oversized_key = insert(&storage, "oversized", &oversized)?;
        assert_eq!(
            lookup_watch_source_observation(&storage, &oversized_key, None)?,
            SourceObservationLookup::Unusable,
        );
        storage.raw().execute_compat(
            "INSERT INTO meta(key, value) VALUES('source_ingest_v1:blob', X'7b7d')",
            &[],
        )?;
        assert_eq!(
            lookup_watch_source_observation(&storage, "source_ingest_v1:blob", None)?,
            SourceObservationLookup::Unusable,
        );
        let retained_bytes: i64 = storage.raw().query_row_map(
            "SELECT length(CAST(value AS BLOB)) FROM meta WHERE key = ?1",
            &[ParamValue::from(oversized_key.as_str())],
            |row| row.get_typed(0),
        )?;
        assert_eq!(retained_bytes as usize, oversized.len());
        assert!(lookup_watch_source_observation(&storage, "unrelated:key", None).is_err());
        assert!(lookup_watch_source_observation(&storage, SOURCE_LEDGER_PREFIX, None).is_err());
        assert!(lookup_watch_source_observation(
            &storage,
            &format!("{SOURCE_LEDGER_PREFIX}{}", "x".repeat(MAX_WATCH_SOURCE_LEDGER_KEY_BYTES)),
            None,
        ).is_err());
        Ok(())
    }

    #[test]
    fn codex_integrity_exact_lookup_sql_failure_is_not_absence() -> Result<()> {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        let key = insert(&storage, "retained", "prior evidence")?;
        // Preserve every row while making the real metadata query fail.
        storage.raw().execute_compat(
            "ALTER TABLE meta RENAME TO retained_meta_for_lookup_test",
            &[],
        )?;
        assert!(lookup_watch_source_observation(&storage, &key, None).is_err());
        let retained: String = storage.raw().query_row_map(
            "SELECT value FROM retained_meta_for_lookup_test WHERE key = ?1",
            &[ParamValue::from(key.as_str())],
            |row| row.get_typed(0),
        )?;
        assert_eq!(retained, "prior evidence");
        Ok(())
    }

    #[test]
    fn codex_integrity_cache_distinguishes_absence_from_bounded_omission() -> Result<()> {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        let (empty, complete) = load_with_completeness(&storage, small_limits(), None)?;
        assert!(empty.is_empty());
        assert!(complete);
        insert(&storage, "a", "first")?;
        let (_, complete) = load_with_completeness(&storage, small_limits(), None)?;
        assert!(complete);
        insert(&storage, "b", &"x".repeat(65))?;
        let (bounded, complete) = load_with_completeness(&storage, small_limits(), None)?;
        assert_eq!(bounded.len(), 1);
        assert!(!complete, "oversized evidence cannot be treated as absent");
        let (_, complete) = load_with_completeness(
            &storage,
            Limits {
                entries: 1,
                ..small_limits()
            },
            None,
        )?;
        assert!(!complete, "a stopped cursor cannot prove later keys absent");
        Ok(())
    }

    #[test]
    fn codex_integrity_production_cache_marks_omitted_large_receipts() -> Result<()> {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        let key = insert(
            &storage,
            "a",
            &"x".repeat(MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES + 1),
        )?;
        let cache = load_bounded_watch_source_ledger(&storage, None)?;
        assert!(cache.contains_key(INCOMPLETE_SOURCE_LEDGER_KEY));
        assert!(!cache.contains_key(&key));
        assert_eq!(storage.source_ingest_ledger_entries()?.len(), 1);
        Ok(())
    }

    #[test]
    fn gh538_watch_source_cache_pages_past_oversized_values_without_mutating_receipts() -> Result<()>
    {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        let first = insert(&storage, "a", "first receipt")?;
        insert(&storage, "b", &"x".repeat(65))?;
        // Character length is below the limit; UTF-8 byte length exceeds it.
        insert(&storage, "c", &"雪".repeat(22))?;
        let unicode_value = "雪".repeat(21);
        let unicode = insert(&storage, "d", &unicode_value)?;
        let last = insert(&storage, "e", "last receipt")?;
        insert(&storage, &"f".repeat(65), "oversized key")?;
        storage.raw().execute_batch(
            "INSERT INTO meta(key, value) VALUES \
             ('source_ingest_v0:older', 'wrong version'), \
             ('source_ingest_v1;', 'upper prefix neighbor'), \
             ('source_ingest_v1:', 'unnamed source');",
        )?;
        let original = storage.source_ingest_ledger_entries()?;
        let cache = load_with_limits(&storage, small_limits(), None)?;
        assert_eq!(
            cache,
            HashMap::from([
                (first, "first receipt".to_string()),
                (unicode, unicode_value),
                (last, "last receipt".to_string()),
            ]),
            "SQL must suppress oversized payloads while keyset pagination continues"
        );
        assert_eq!(
            storage.source_ingest_ledger_entries()?,
            original,
            "cache omissions must never erase durable recovery evidence"
        );
        Ok(())
    }

    #[test]
    fn gh538_watch_source_cache_bounds_examined_rows_even_when_values_are_skipped() -> Result<()> {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        insert(&storage, "0", &"x".repeat(65))?;
        let first = insert(&storage, "1", "one")?;
        let second = insert(&storage, "2", "two")?;
        insert(&storage, "3", "beyond the row budget")?;
        let cache = load_with_limits(
            &storage,
            Limits {
                entries: 3,
                ..small_limits()
            },
            None,
        )?;
        assert_eq!(
            cache,
            HashMap::from([(first, "one".into()), (second, "two".into())])
        );
        assert_eq!(storage.source_ingest_ledger_entries()?.len(), 4);
        Ok(())
    }

    #[test]
    fn gh538_watch_source_cache_caps_total_utf8_payload_and_returns_a_partial_cache() -> Result<()>
    {
        let temp = TempDir::new()?;
        let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
        let value = "雪".repeat(100);
        for index in 0..8 {
            insert(&storage, &format!("{index:03}"), &value)?;
        }
        let cache = load_with_limits(
            &storage,
            Limits {
                bytes: 1024,
                value_bytes: 512,
                ..small_limits()
            },
            None,
        )?;
        assert_eq!(
            cache.len(),
            3,
            "receipt caching must stop within its byte budget"
        );
        let retained_bytes: usize = cache
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum();
        assert!(retained_bytes <= 1024);
        for index in 0..3 {
            assert_eq!(
                cache.get(&format!("{SOURCE_LEDGER_PREFIX}{index:03}")),
                Some(&value)
            );
        }
        assert_eq!(storage.source_ingest_ledger_entries()?.len(), 8);
        Ok(())
    }
}
