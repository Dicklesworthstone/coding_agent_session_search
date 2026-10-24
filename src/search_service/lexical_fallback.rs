//! Explicit, request-scoped recovery from a Quill work-budget refusal.
//!
//! Only the supervised stdio/MCP service calls this lane. Its EXISTING request
//! watchdog covers primary search, fallback, response output and reader drop;
//! fallback never arms a fresh deadline or leaves an abandoned native thread.
//! The size admission below is a metadata observation, not a snapshot-size or
//! peak-memory guarantee. The service's deadline and RSS supervisor remain on.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use coding_agent_search::search::query::{
    FieldMask, SearchClient, SearchClientOptions, SearchFilters, SearchHit,
};
use coding_agent_search::search::quill_bridge::is_query_fuel_exhausted;
use serde_json::{Value, json};

pub(super) const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Recovery {
    pub(super) enabled: bool,
    attempts: u64,
    completed: u64,
}

pub(super) struct Page {
    pub(super) hits: Vec<SearchHit>,
    pub(super) metadata: Value,
}

pub(super) struct Search<'a> {
    pub(super) query: &'a str,
    pub(super) filters: SearchFilters,
    pub(super) limit: usize,
    pub(super) offset: usize,
}

impl Recovery {
    pub(super) fn archive_access_attempted(&self) -> bool {
        self.attempts != 0
    }

    pub(super) fn status(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "backend": "frankensqlite",
            "trigger": "quill_query_fuel_exhausted_only",
            "attempts": self.attempts,
            "completed": self.completed,
            "max_observed_archive_bytes": MAX_ARCHIVE_BYTES,
            "includes_wal": true,
            "retains_reader": false,
            "shares_request_deadline": true,
        })
    }

    /// Do not call outside a live service request deadline. The SQLite lane
    /// can execute FTS5 or a source scan, so a page LIMIT is not a time bound.
    pub(super) fn recover(
        &mut self,
        archive: Option<&Path>,
        primary_error: anyhow::Error,
        search: Search<'_>,
    ) -> Result<Page> {
        if !is_query_fuel_exhausted(&primary_error) {
            return Err(primary_error);
        }
        let remedy = "Rebuild separately with `cass index --full --json`, using the same \
                      --db and --data-dir as this index. No automatic rebuild or fuel increase was attempted.";
        if !self.enabled {
            return Err(primary_error.context(format!(
                "Quill refused the query's work budget; archive fallback is disabled. \
                 For a small archive, add `--lexical-fallback --db PATH` to `cass serve --stdio`. {remedy}"
            )));
        }
        let Some(archive) = archive else {
            return Err(primary_error.context(format!(
                "Quill refused the query's work budget; lexical fallback requires an explicit --db. {remedy}"
            )));
        };

        self.attempts = self.attempts.saturating_add(1);
        let primary_message = format!("{primary_error:#}");
        let fallback = (|| -> Result<Page> {
            // Metadata first: do not open a multi-GB canonical archive merely
            // because a fragmented lexical generation rejected the query.
            let observed_archive_bytes = archive_bytes(archive, MAX_ARCHIVE_BYTES)?;
            let client = SearchClient::open_semantic(
                archive,
                SearchClientOptions {
                    enable_reload: false,
                    enable_warm: false,
                    strict_read_only: true,
                },
            )?
            .context("configured fallback archive is unavailable")?;
            // open_semantic is the existing lazy SQLite-only constructor. No
            // semantic context is attached and no lexical index is opened.
            // A fresh client prevents caching fallback hits under a Quill epoch.
            let hits = client.search(
                search.query,
                search.filters,
                search.limit,
                search.offset,
                FieldMask::new(false, true, true, false),
            )?;
            ensure!(
                hits.len() <= search.limit,
                "SQLite fallback exceeded the requested page"
            );
            // Drop the connection under the SAME request watchdog, before we
            // report a completed fallback or let the next request reuse a slot.
            drop(client);
            Ok(Page {
                hits,
                metadata: json!({
                    "reason": "quill_query_fuel_exhausted",
                    "primary_error": primary_message,
                    "backend": "frankensqlite",
                    // SearchClient does not expose which of these it used.
                    // Do not mislabel a source-table scan as indexed FTS5.
                    "sqlite_lane": null,
                    "possible_sqlite_lanes": ["fts5", "source_scan"],
                    "observed_archive_bytes": observed_archive_bytes,
                    "snapshot_policy": "fresh_archive_reads_not_the_pinned_quill_generation",
                    "ranking_equivalent_to_quill": false,
                    "quill_prefix_semantics_preserved": false,
                    "shares_request_deadline": true,
                    "repair_performed": false,
                }),
            })
        })();
        match fallback {
            Ok(page) => {
                self.completed = self.completed.saturating_add(1);
                Ok(page)
            }
            Err(error) => Err(primary_error.context(format!(
                "Quill refused the query's work budget and the bounded archive fallback failed: \
                 {error:#}. {remedy}"
            ))),
        }
    }
}

fn wal_path(archive: &Path) -> PathBuf {
    let mut path = archive.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}

fn archive_bytes(archive: &Path, maximum: u64) -> Result<u64> {
    let database = std::fs::metadata(archive)
        .with_context(|| format!("reading fallback archive metadata at {}", archive.display()))?;
    ensure!(database.is_file(), "fallback archive is not a regular file");
    let wal = match std::fs::metadata(wal_path(archive)) {
        Ok(metadata) => {
            ensure!(metadata.is_file(), "fallback WAL is not a regular file");
            metadata.len()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error).context("reading fallback WAL metadata"),
    };
    let total = database
        .len()
        .checked_add(wal)
        .context("fallback archive size overflow")?;
    ensure!(
        total <= maximum,
        "fallback archive exceeds the automatic search budget: database_bytes={}, \
         wal_bytes={wal}, archive_bytes={total}, maximum_bytes={maximum}",
        database.len(),
    );
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_admission_counts_wal_and_refuses_non_files_before_opening_sqlite() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db = temp.path().join("archive.db");
        std::fs::write(&db, b"invalid SQLite but valid size fixture")?;
        let size = std::fs::metadata(&db)?.len();
        assert_eq!(archive_bytes(&db, size)?, size);
        assert!(archive_bytes(&db, size - 1).is_err());
        std::fs::write(wal_path(&db), b"wal")?;
        assert!(archive_bytes(&db, size).is_err());
        assert_eq!(archive_bytes(&db, size + 3)?, size + 3);
        assert!(archive_bytes(temp.path(), u64::MAX).is_err());
        assert!(archive_bytes(&temp.path().join("absent"), u64::MAX).is_err());
        Ok(())
    }

    #[test]
    fn corrupt_and_invariant_errors_never_open_the_archive() {
        let mut recovery = Recovery {
            enabled: true,
            ..Recovery::default()
        };
        for message in [
            "posting cursor invariant failed",
            "corrupt MANIFEST",
            "query cancelled",
        ] {
            let result = recovery.recover(
                Some(Path::new("/never-opened")),
                anyhow::anyhow!(message),
                Search {
                    query: "git fixture",
                    filters: SearchFilters::default(),
                    limit: 1,
                    offset: 0,
                },
            );
            assert_eq!(result.err().unwrap().to_string(), message);
        }
        assert!(!recovery.archive_access_attempted());
        assert_eq!(recovery.status()["attempts"], 0);
    }
}
