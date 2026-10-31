//! Bounded, read-only receipt caching for live watch scans.
//!
//! Receipts are an optimization: an absent entry makes the connector parse its
//! source normally. A large archive or malformed oversized receipt must not
//! require an unbounded metadata allocation before watch ingestion can begin.

use crate::franken_sync::compat::{ConnectionExt, ParamValue, RowExt};
use crate::storage::sqlite::FrankenStorage;
use anyhow::Result;
use std::collections::HashMap;

pub(super) const MAX_WATCH_SOURCE_LEDGER_ENTRIES: usize = 65_536;
pub(super) const MAX_WATCH_SOURCE_LEDGER_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES: usize = 256 * 1024;
const MAX_WATCH_SOURCE_LEDGER_KEY_BYTES: usize = 512;
const WATCH_SOURCE_LEDGER_PAGE_ROWS: usize = 256;
const SOURCE_LEDGER_PREFIX: &str = "source_ingest_v1:";

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: usize,
    value_bytes: usize,
    key_bytes: usize,
    page_rows: usize,
}

/// Cache only a bounded prefix of source observations. Omitted rows remain in
/// canonical storage and are never acknowledged or deleted by this reader.
pub(super) fn load_bounded_watch_source_ledger(
    storage: &FrankenStorage,
    progress: Option<&super::IndexingProgress>,
) -> Result<HashMap<String, String>> {
    load_with_limits(
        storage,
        Limits {
            entries: MAX_WATCH_SOURCE_LEDGER_ENTRIES,
            bytes: MAX_WATCH_SOURCE_LEDGER_BYTES,
            value_bytes: MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES,
            key_bytes: MAX_WATCH_SOURCE_LEDGER_KEY_BYTES,
            page_rows: WATCH_SOURCE_LEDGER_PAGE_ROWS,
        },
        progress,
    )
}

fn load_with_limits(
    storage: &FrankenStorage,
    limits: Limits,
    progress: Option<&super::IndexingProgress>,
) -> Result<HashMap<String, String>> {
    let mut cache = HashMap::new();
    let mut after = SOURCE_LEDGER_PREFIX.to_string();
    let mut bytes = 0usize;
    let mut examined = 0usize;
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
                continue;
            };
            // String::len counts UTF-8 bytes, matching the SQL BLOB length.
            // Keep the guard even if a backend returns unexpected coercions.
            let entry_bytes = key.len().saturating_add(value.len());
            if entry_bytes <= limits.bytes.saturating_sub(bytes) {
                bytes += entry_bytes;
                cache.insert(key, value);
            }
        }
        if page_len < page_rows {
            break;
        }
    }
    tracing::debug!(
        cached_sources = cache.len(),
        source_rows_examined = examined,
        cached_payload_bytes = bytes,
        "loaded bounded watch source receipts"
    );
    Ok(cache)
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
