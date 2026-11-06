//! One model selector contract for admission, execution and cancellation.
//!
//! Legacy durable rows retain their original labels. Matching an alias during
//! cancellation does not establish which backend produced an older artifact.

use super::{EmbeddingJobConfig, FastEmbedder, HASH_EMBEDDER_MODEL};
use crate::franken_sync::Connection;
use crate::franken_sync::compat::{ConnectionExt, ParamValue, RowExt, TransactionExt};

const CANCELLATION_PAGE_SIZE: i64 = 128;
// Earlier mailbox versions bounded the whole request to 16 KiB but did not
// canonicalize model labels. Bound legacy projections before reading into Rust.
const MAX_LEGACY_MODEL_BYTES: usize = 16 * 1024;

pub(super) fn canonical_model(model: &str) -> Option<&'static str> {
    let model = model.trim();
    if model.eq_ignore_ascii_case(HASH_EMBEDDER_MODEL) || model.eq_ignore_ascii_case("fnv1a-384") {
        Some(HASH_EMBEDDER_MODEL)
    } else {
        FastEmbedder::canonical_name(model)
    }
}

pub(super) fn cancellation_key(model: &str) -> String {
    canonical_model(model)
        .map(str::to_string)
        // Preserve cleanup of unsupported legacy requests. Such a selector
        // may cancel a historical row, but is never admitted for execution.
        .unwrap_or_else(|| model.trim().to_ascii_lowercase())
}

impl EmbeddingJobConfig {
    /// Resolve defaults and aliases before queue ownership or durable writes.
    /// A fast pass may use hash control or a native profile; the quality pass
    /// must select a distinct native profile. Slot names never select a backend.
    pub(super) fn normalize_models(mut self) -> Result<Self, String> {
        let normalize = |field, value: Option<String>| {
            value
                .map(|value| {
                    canonical_model(&value).map(str::to_string).ok_or_else(|| {
                        format!(
                            "embedding job requires a supported {field}: hash, minilm, or multilingual-minilm"
                        )
                    })
                })
                .transpose()
        };
        self.fast_model = normalize("fast_model", self.fast_model)?;
        self.quality_model = normalize("quality_model", self.quality_model)?;
        if self.two_tier {
            let fast = self.fast_pass_model();
            let quality = self.quality_pass_model();
            if quality == HASH_EMBEDDER_MODEL {
                return Err(
                    "two-tier embedding job requires a native quality_model; use a single hash pass for hash control"
                        .to_string(),
                );
            }
            if fast == quality {
                return Err(
                    "two-tier embedding job requires distinct fast_model and quality_model profiles; use a single pass for one model"
                        .to_string(),
                );
            }
            self.fast_model = Some(fast);
            self.quality_model = Some(quality);
        } else {
            // Retain the public single-pass quality-over-fast precedence, but
            // erase unused/default spellings so equivalent pending work coalesces.
            let model = self.single_pass_model();
            self.fast_model = None;
            self.quality_model = Some(model);
        }
        Ok(self)
    }
}

/// Cancel the same model aliases as the mailbox, including rows left by an
/// older daemon. Keyset pages bound identity materialization independently of
/// the number of historical jobs. The caller owns archive mutation admission.
pub(super) fn cancel_persisted_jobs(
    connection: &Connection,
    db_path: &str,
    model: Option<&str>,
) -> anyhow::Result<usize> {
    let Some(model) = model else {
        return Ok(connection.execute_compat(
            "UPDATE embedding_jobs SET status = 'cancelled' \
             WHERE db_path = ?1 AND status IN ('pending', 'running')",
            &[ParamValue::from(db_path)],
        )?);
    };
    let selected = cancellation_key(model);
    let mut cursor = i64::MIN;
    let mut cancelled = 0;
    loop {
        let rows = connection.query_map_collect(
            "SELECT id, substr(model_id, 1, ?3) FROM embedding_jobs \
             WHERE db_path = ?1 AND id >= ?2 AND status IN ('pending', 'running') \
             ORDER BY id LIMIT ?4",
            &[
                ParamValue::from(db_path),
                ParamValue::from(cursor),
                ParamValue::from((MAX_LEGACY_MODEL_BYTES + 1) as i64),
                ParamValue::from(CANCELLATION_PAGE_SIZE),
            ],
            |row| Ok((row.get_typed::<i64>(0)?, row.get_typed::<String>(1)?)),
        )?;
        let matching = rows
            .iter()
            .filter(|(_, stored_model)| {
                stored_model.len() <= MAX_LEGACY_MODEL_BYTES
                    && cancellation_key(stored_model) == selected
            })
            .collect::<Vec<_>>();
        if !matching.is_empty() {
            // One bounded commit per page avoids a durability barrier for
            // every alias row while keeping cancellation retries idempotent.
            let mut transaction = connection.transaction()?;
            let mut page_cancelled = 0;
            for (id, stored_model) in matching {
                // Keep the original identity in the predicate: an independently
                // changed row must not inherit the earlier matching decision.
                page_cancelled += transaction.execute_compat(
                    "UPDATE embedding_jobs SET status = 'cancelled' \
                     WHERE id = ?1 AND db_path = ?2 AND model_id = ?3 \
                     AND status IN ('pending', 'running')",
                    &[
                        ParamValue::from(*id),
                        ParamValue::from(db_path),
                        ParamValue::from(stored_model.as_str()),
                    ],
                )?;
            }
            transaction.commit()?;
            cancelled += page_cancelled;
        }
        if rows.len() < CANCELLATION_PAGE_SIZE as usize {
            break;
        }
        let Some(next) = rows.last().and_then(|(id, _)| id.checked_add(1)) else {
            break;
        };
        cursor = next;
    }
    Ok(cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_cancellation_matches_aliases_without_relabelling_legacy_rows() -> anyhow::Result<()>
    {
        let connection = Connection::open(":memory:")?;
        connection.execute_batch(
            "CREATE TABLE embedding_jobs (
                id INTEGER PRIMARY KEY, db_path TEXT NOT NULL,
                model_id TEXT NOT NULL, status TEXT NOT NULL
             );",
        )?;
        let rows = [
            (1_i64, "archive", "minilm", "pending"),
            (2, "archive", " \tMINILM-384\n", "running"),
            (3, "archive", "fastembed", "pending"),
            (4, "archive", "all-MiniLM-L6-v2", "completed"),
            (5, "other", "minilm", "running"),
            (6, "archive", "multilingual", "running"),
            (7, "archive", "FNV1A-384", "pending"),
            (8, "archive", "Legacy-Unavailable-Model", "running"),
        ];
        for (id, path, model, state) in rows {
            connection.execute_compat(
                "INSERT INTO embedding_jobs VALUES (?1, ?2, ?3, ?4)",
                &[
                    ParamValue::from(id),
                    ParamValue::from(path),
                    ParamValue::from(model),
                    ParamValue::from(state),
                ],
            )?;
        }
        assert_eq!(
            cancel_persisted_jobs(&connection, "archive", Some(" ALL-MINILM-L6-V2 "))?,
            3
        );
        assert_eq!(
            cancel_persisted_jobs(&connection, "archive", Some("minilm"))?,
            0
        );
        let actual = connection.query_map_collect(
            "SELECT model_id, status FROM embedding_jobs ORDER BY id",
            &[],
            |row| Ok((row.get_typed::<String>(0)?, row.get_typed::<String>(1)?)),
        )?;
        for (index, (_, _, model, state)) in rows.iter().enumerate() {
            assert_eq!(actual[index].0, *model, "legacy labels must remain intact");
            assert_eq!(actual[index].1, if index < 3 { "cancelled" } else { state });
        }
        assert_eq!(
            cancel_persisted_jobs(&connection, "archive", Some("hash"))?,
            1
        );
        assert_eq!(
            cancel_persisted_jobs(&connection, "archive", Some("legacy-unavailable-model"))?,
            1
        );
        assert_eq!(cancel_persisted_jobs(&connection, "archive", None)?, 1);
        Ok(())
    }

    #[test]
    fn durable_cancellation_continues_across_pages_and_never_matches_a_truncated_label()
    -> anyhow::Result<()> {
        let connection = Connection::open(":memory:")?;
        connection.execute_batch(
            "CREATE TABLE embedding_jobs (
                id INTEGER PRIMARY KEY, db_path TEXT NOT NULL,
                model_id TEXT NOT NULL, status TEXT NOT NULL
             );",
        )?;
        connection.execute("BEGIN IMMEDIATE")?;
        for id in 1..=CANCELLATION_PAGE_SIZE * 2 + 1 {
            let model = if id % 2 == 0 {
                "minilm-384"
            } else {
                "multilingual"
            };
            connection.execute_compat(
                "INSERT INTO embedding_jobs VALUES (?1, 'archive', ?2, 'running')",
                &[ParamValue::from(id), ParamValue::from(model)],
            )?;
        }
        let oversized = "minilm".to_string() + &" ".repeat(MAX_LEGACY_MODEL_BYTES);
        connection.execute_compat(
            "INSERT INTO embedding_jobs VALUES (1000, 'archive', ?1, 'running')",
            &[ParamValue::from(oversized.as_str())],
        )?;
        connection.execute("COMMIT")?;
        assert_eq!(
            cancel_persisted_jobs(&connection, "archive", Some("minilm"))?,
            CANCELLATION_PAGE_SIZE as usize
        );
        let status: String = connection.query_row_map(
            "SELECT status FROM embedding_jobs WHERE id = 1000",
            &[],
            |row| row.get_typed(0),
        )?;
        assert_eq!(status, "running");
        assert_eq!(
            cancel_persisted_jobs(&connection, "archive", Some("multilingual-minilm-384"))?,
            CANCELLATION_PAGE_SIZE as usize + 1
        );
        Ok(())
    }
}
