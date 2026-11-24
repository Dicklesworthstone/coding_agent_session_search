//! Read-only, bounded canonical oracle for the synthetic GH565 qualification.
//!
//! Usage: forget_qualification_probe DATA_DIR [--quick-check]
//! DATA_DIR must contain .gh565-synthetic-fixture.json with exactly
//! {"schema_version":1,"synthetic":true}. This helper never creates, migrates,
//! checkpoints, repairs or clears a privacy fence. Run it only while its owned
//! synthetic archive is quiescent, outside the measured forget process/cgroup.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use coding_agent_search::franken_sync::compat::{OpenFlags, RowExt, open_with_flags};
use coding_agent_search::franken_sync::{Connection, Row, SqliteValue};
use coding_agent_search::search::privacy;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PAGE_ROWS: i64 = 64;
const MAX_TRACK_B_BUCKETS: usize = 65_536;
const MAX_DIMENSION_BYTES: usize = 256;
const MAX_MISMATCH_EXAMPLES: usize = 8;
const MAX_CATALOG_ROWS: i64 = 4_096;
const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;
const TOKEN_BOUNDS_SQL: &str =
    "SELECT MIN(id), MAX(id), COUNT(*) FROM token_usage INDEXED BY idx_token_usage_conv
     WHERE conversation_id = ?1";
const TOKEN_PAGE_SQL: &str = "SELECT * FROM token_usage NOT INDEXED
     WHERE conversation_id = ?1 AND id >= ?2 AND id <= ?3 ORDER BY id LIMIT ?4";
const TRACK_B_KEYS: [&str; 4] = ["day_id", "agent_slug", "source_id", "model_family"];
const TRACK_B_COUNTERS: [&str; 13] = [
    "api_call_count",
    "user_message_count",
    "assistant_message_count",
    "tool_message_count",
    "total_input_tokens",
    "total_output_tokens",
    "total_cache_read_tokens",
    "total_cache_creation_tokens",
    "total_thinking_tokens",
    "grand_total_tokens",
    "total_content_chars",
    "total_tool_calls",
    "session_count",
];
type BucketKey = (i64, String, String, String);
type Counters = [i64; TRACK_B_COUNTERS.len()];
type Buckets = BTreeMap<BucketKey, Counters>;

fn checked_key(day: i64, agent: &str, source: &str, model: &str) -> Result<BucketKey> {
    ensure!(
        [agent, source, model]
            .iter()
            .all(|value| value.len() <= MAX_DIMENSION_BYTES),
        "Track B dimension exceeds the synthetic oracle byte limit"
    );
    Ok((day, agent.to_owned(), source.to_owned(), model.to_owned()))
}

fn add_bucket(buckets: &mut Buckets, key: BucketKey, delta: &Counters) -> Result<()> {
    if let Some(total) = buckets.get_mut(&key) {
        for (value, increment) in total.iter_mut().zip(delta) {
            *value = value
                .checked_add(*increment)
                .context("Track B counter overflow")?;
        }
    } else {
        ensure!(
            buckets.len() < MAX_TRACK_B_BUCKETS,
            "Track B distinct bucket limit exceeded; refusing an unbounded oracle"
        );
        buckets.insert(key, *delta);
    }
    Ok(())
}

/// This is an independent oracle, not the production TokenStatsAggregator.
/// It retains only capped scalar buckets, never ledger rows or message bodies.
#[derive(Default)]
struct TrackBOracle {
    expected: Buckets,
}

impl TrackBOracle {
    fn record(
        &mut self,
        day: i64,
        agent: &str,
        source: &str,
        model: &str,
        delta: &Counters,
    ) -> Result<()> {
        // These are the five stored axes, not the eight-way cross product.
        // Literal "all" values must not count one contribution twice.
        let axes = [
            (agent, source, model),
            ("all", source, model),
            (agent, "all", model),
            (agent, source, "all"),
            ("all", "all", "all"),
        ];
        for (index, &(agent, source, model)) in axes.iter().enumerate() {
            if !axes[..index].contains(&(agent, source, model)) {
                add_bucket(
                    &mut self.expected,
                    checked_key(day, agent, source, model)?,
                    delta,
                )?;
            }
        }
        Ok(())
    }

    fn session(
        &mut self,
        started_at: Option<i64>,
        agent: &str,
        source: &str,
        model: &str,
    ) -> Result<()> {
        // Stored days are UTC days since 2020-01-01, including negative days.
        let day = started_at
            .map(|ms| ms.div_euclid(86_400_000) - 18_262)
            .unwrap_or(0);
        let mut delta = [0; TRACK_B_COUNTERS.len()];
        delta[12] = 1;
        self.record(day, agent, source, model, &delta)
    }

    fn report(&self, connection: &Connection) -> Result<Value> {
        let actual = read_track_b_buckets(connection)?;
        let mut mismatches = 0_usize;
        let mut examples = Vec::new();
        for (key, expected) in &self.expected {
            let fields: Vec<_> = match actual.get(key) {
                Some(actual) => TRACK_B_COUNTERS
                    .iter()
                    .enumerate()
                    .filter_map(|(index, name)| (expected[index] != actual[index]).then_some(*name))
                    .collect(),
                None => TRACK_B_COUNTERS.to_vec(),
            };
            if !fields.is_empty() {
                mismatches += 1;
                if examples.len() < MAX_MISMATCH_EXAMPLES {
                    examples.push(json!({
                        "kind": if actual.contains_key(key) { "different" } else { "missing" },
                        "key": {"day_id": key.0, "agent_slug": key.1,
                                "source_id": key.2, "model_family": key.3},
                        "differing_fields": fields,
                    }));
                }
            }
        }
        for key in actual
            .keys()
            .filter(|key| !self.expected.contains_key(*key))
        {
            mismatches += 1;
            if examples.len() < MAX_MISMATCH_EXAMPLES {
                examples.push(json!({"kind": "extra",
                    "key": {"day_id": key.0, "agent_slug": key.1,
                            "source_id": key.2, "model_family": key.3},
                    "differing_fields": TRACK_B_COUNTERS,
                }));
            }
        }
        Ok(json!({
            "schema_version": 1,
            "method": "canonical-ledger-conversations-v1",
            "key_columns": TRACK_B_KEYS,
            "verified_columns": TRACK_B_COUNTERS,
            "ignored_columns": ["estimated_cost_usd", "last_updated"],
            "expected_rows": self.expected.len(),
            "actual_rows": actual.len(),
            "expected_sha256": bucket_digest(&self.expected)?,
            "actual_sha256": bucket_digest(&actual)?,
            "mismatch_count": mismatches,
            "mismatch_examples": examples,
            "matches": mismatches == 0,
            "limits": {"max_buckets": MAX_TRACK_B_BUCKETS,
                       "max_dimension_utf8_bytes": MAX_DIMENSION_BYTES,
                       "page_rows": PAGE_ROWS},
        }))
    }
}

fn bucket_digest(buckets: &Buckets) -> Result<String> {
    let names: Vec<_> = TRACK_B_KEYS
        .iter()
        .chain(TRACK_B_COUNTERS.iter())
        .map(|name| (*name).to_owned())
        .collect();
    let mut hasher = table_hasher("track-b-integer-buckets-v1", &names)?;
    for (key, counters) in buckets {
        let mut values = vec![
            SqliteValue::Integer(key.0),
            SqliteValue::Text(key.1.clone().into()),
            SqliteValue::Text(key.2.clone().into()),
            SqliteValue::Text(key.3.clone().into()),
        ];
        values.extend(counters.iter().copied().map(SqliteValue::Integer));
        hash_row(&mut hasher, &values)?;
    }
    Ok(finish(hasher))
}

fn read_track_b_buckets(connection: &Connection) -> Result<Buckets> {
    let projection = TRACK_B_KEYS
        .iter()
        .chain(TRACK_B_COUNTERS.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT rowid, {projection} FROM token_daily_stats NOT INDEXED
         WHERE rowid >= ?1 ORDER BY rowid LIMIT ?2"
    );
    let mut buckets = Buckets::new();
    let mut next = Some(i64::MIN);
    while let Some(lower) = next {
        let rows = connection.query_with_params(
            &sql,
            &[SqliteValue::Integer(lower), SqliteValue::Integer(PAGE_ROWS)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let key = checked_key(
                row.get_typed(1)?,
                &row.get_typed::<String>(2)?,
                &row.get_typed::<String>(3)?,
                &row.get_typed::<String>(4)?,
            )?;
            ensure!(
                buckets.len() < MAX_TRACK_B_BUCKETS,
                "Track B distinct bucket limit exceeded; refusing an unbounded oracle"
            );
            let mut counters = [0; TRACK_B_COUNTERS.len()];
            for (index, value) in counters.iter_mut().enumerate() {
                *value = row.get_typed(index + 5)?;
            }
            ensure!(
                buckets.insert(key, counters).is_none(),
                "duplicate Track B bucket"
            );
        }
        next = rows
            .last()
            .context("empty Track B page")?
            .get_typed::<i64>(0)?
            .checked_add(1);
    }
    ensure!(
        i64::try_from(buckets.len())?
            == scalar(connection, "SELECT COUNT(*) FROM token_daily_stats")?,
        "Track B scan lost rows"
    );
    Ok(buckets)
}

struct LedgerColumns {
    id: usize,
    total: usize,
    day: usize,
    role: usize,
    model: usize,
    components: [usize; 5],
    content: usize,
    tools: usize,
}

impl LedgerColumns {
    fn new(names: &[String]) -> Result<Self> {
        Ok(Self {
            id: column(names, "id")?,
            total: column(names, "total_tokens")?,
            day: column(names, "day_id")?,
            role: column(names, "role")?,
            model: column(names, "model_family")?,
            components: [
                column(names, "input_tokens")?,
                column(names, "output_tokens")?,
                column(names, "cache_read_tokens")?,
                column(names, "cache_creation_tokens")?,
                column(names, "thinking_tokens")?,
            ],
            content: column(names, "content_chars")?,
            tools: column(names, "tool_call_count")?,
        })
    }

    fn counters(&self, row: &Row) -> Result<Counters> {
        let mut result = [0; TRACK_B_COUNTERS.len()];
        result[0] = 1;
        match row.get_typed::<String>(self.role)?.as_str() {
            "user" => result[1] = 1,
            "assistant" | "agent" => result[2] = 1,
            "tool" => result[3] = 1,
            _ => {}
        }
        for (index, column) in self.components.iter().enumerate() {
            result[index + 4] = row.get_typed::<Option<i64>>(*column)?.unwrap_or(0);
        }
        // FAD a0dd013a's total_tokens includes input/output/cache read/cache
        // creation with saturating addition; thinking is a separate counter.
        // Do not trust the redundant ledger total or invoke its production helper.
        result[9] = result[4..8]
            .iter()
            .fold(0_i64, |sum, value| sum.saturating_add(*value));
        result[10] = row.get_typed(self.content)?;
        let tools: i64 = row.get_typed(self.tools)?;
        result[11] = if (0..=i64::from(u32::MAX)).contains(&tools) {
            tools
        } else {
            0
        };
        Ok(result)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyntheticSentinel {
    schema_version: u32,
    synthetic: bool,
}

fn require_regular(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "expected a regular non-symlink file: {}",
        path.display()
    );
    Ok(())
}

fn admitted_database(directory: &Path) -> Result<PathBuf> {
    ensure!(directory.is_absolute(), "DATA_DIR must be absolute");
    // Check every component, not just the final directory. No fixture alias may
    // redirect the probe into a different archive or another user's home.
    let mut component_path = PathBuf::new();
    for component in directory.components() {
        ensure!(
            !matches!(component, std::path::Component::ParentDir),
            "DATA_DIR must not contain parent traversal"
        );
        component_path.push(component);
        let metadata = fs::symlink_metadata(&component_path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "DATA_DIR contains a non-directory or symlink component"
        );
    }
    let sentinel = directory.join(".gh565-synthetic-fixture.json");
    require_regular(&sentinel)?;
    ensure!(
        fs::metadata(&sentinel)?.len() <= 1024,
        "oversized fixture sentinel"
    );
    let sentinel: SyntheticSentinel = serde_json::from_slice(&fs::read(sentinel)?)?;
    ensure!(
        sentinel.schema_version == 1 && sentinel.synthetic,
        "refusing an archive not marked as a GH565 synthetic fixture"
    );
    let database = directory.join("agent_search.db");
    require_regular(&database)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = directory.join(format!("agent_search.db{suffix}"));
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => require_regular(&sidecar)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(database)
}

fn scalar(connection: &Connection, sql: &str) -> Result<i64> {
    Ok(connection.query_row(sql)?.get_typed(0)?)
}

fn columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    // All table identifiers here are compile-time literals, never CLI input.
    connection
        .query(&format!("PRAGMA table_info({table})"))?
        .into_iter()
        .map(|row| Ok(row.get_typed::<String>(1)?))
        .collect()
}

fn column(columns: &[String], name: &str) -> Result<usize> {
    columns
        .iter()
        .position(|column| column == name)
        .with_context(|| format!("required canonical column is absent: {name}"))
}

fn frame(hasher: &mut Sha256, kind: u8, bytes: &[u8]) -> Result<()> {
    hasher.update([kind]);
    hasher.update(u64::try_from(bytes.len())?.to_le_bytes());
    hasher.update(bytes);
    Ok(())
}

fn table_hasher(table: &str, columns: &[String]) -> Result<Sha256> {
    let mut hasher = Sha256::new();
    hasher.update(b"cass-gh565-typed-rows-v1\0");
    frame(&mut hasher, b't', table.as_bytes())?;
    for column in columns {
        frame(&mut hasher, b'c', column.as_bytes())?;
    }
    Ok(hasher)
}

fn hash_row(hasher: &mut Sha256, values: &[SqliteValue]) -> Result<()> {
    hasher.update([b'r']);
    hasher.update(u64::try_from(values.len())?.to_le_bytes());
    for value in values {
        match value {
            SqliteValue::Null => frame(hasher, b'n', &[])?,
            SqliteValue::Integer(value) => frame(hasher, b'i', &value.to_le_bytes())?,
            SqliteValue::Float(value) => frame(hasher, b'f', &value.to_bits().to_le_bytes())?,
            SqliteValue::Text(value) => frame(hasher, b't', value.as_bytes_direct())?,
            SqliteValue::Blob(value) => frame(hasher, b'b', value.as_ref())?,
        }
    }
    Ok(())
}

fn finish(hasher: Sha256) -> String {
    format!("{:x}", hasher.finalize())
}

fn json_cell(value: &SqliteValue) -> Value {
    match value {
        SqliteValue::Null => Value::Null,
        SqliteValue::Integer(value) => json!(value),
        SqliteValue::Float(value) => json!({"float_bits": format!("{:016x}", value.to_bits())}),
        SqliteValue::Text(value) => json!(value.to_string()),
        SqliteValue::Blob(value) => json!({"blob_bytes": value.as_ref()}),
    }
}

fn row_object(names: &[String], values: &[SqliteValue]) -> Result<BTreeMap<String, Value>> {
    ensure!(names.len() == values.len(), "schema and row width disagree");
    Ok(names
        .iter()
        .cloned()
        .zip(values.iter().map(json_cell))
        .collect())
}

fn dump_tombstones(connection: &Connection, table: &str) -> Result<Value> {
    let names = columns(connection, table)?;
    ensure!(!names.is_empty(), "missing tombstone table {table}");
    let mut hasher = table_hasher(table, &names)?;
    let mut result = Vec::new();
    let mut next = Some(i64::MIN);
    while let Some(lower) = next {
        let rows = connection.query_with_params(
            &format!(
                "SELECT rowid, * FROM {table} NOT INDEXED WHERE rowid >= ?1 ORDER BY rowid LIMIT ?2"
            ),
            &[SqliteValue::Integer(lower), SqliteValue::Integer(PAGE_ROWS)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            hash_row(&mut hasher, &row.values()[1..])?;
            result.push(row_object(&names, &row.values()[1..])?);
        }
        next = rows
            .last()
            .context("empty tombstone page")?
            .get_typed::<i64>(0)?
            .checked_add(1);
    }
    Ok(json!({"rows": result, "sha256": finish(hasher)}))
}

fn catalog_summary(connection: &Connection, table: &str) -> Result<Value> {
    // Source-glob forget retains these catalogs. Bound their fixture-only
    // inventories separately; hash every stored field, including provenance.
    let names = columns(connection, table)?;
    ensure!(!names.is_empty(), "missing canonical catalog {table}");
    let count = scalar(connection, &format!("SELECT COUNT(*) FROM {table}"))?;
    ensure!(
        (0..=MAX_CATALOG_ROWS).contains(&count),
        "canonical catalog row limit exceeded"
    );
    let mut hasher = table_hasher(table, &names)?;
    let mut next = Some(i64::MIN);
    let mut seen = 0_i64;
    let mut bytes = 0_u64;
    while let Some(lower) = next {
        let rows = connection.query_with_params(
            &format!(
                "SELECT rowid, * FROM {table} NOT INDEXED
                 WHERE rowid >= ?1 ORDER BY rowid LIMIT ?2"
            ),
            &[SqliteValue::Integer(lower), SqliteValue::Integer(PAGE_ROWS)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            for value in &row.values()[1..] {
                let width = match value {
                    SqliteValue::Null => 0,
                    SqliteValue::Integer(_) | SqliteValue::Float(_) => 8,
                    SqliteValue::Text(value) => u64::try_from(value.as_bytes_direct().len())?,
                    SqliteValue::Blob(value) => u64::try_from(value.as_ref().len())?,
                };
                bytes = bytes
                    .checked_add(width)
                    .context("catalog byte count overflow")?;
                ensure!(
                    bytes <= MAX_CATALOG_BYTES,
                    "canonical catalog byte limit exceeded"
                );
            }
            hash_row(&mut hasher, &row.values()[1..])?;
            seen += 1;
            ensure!(
                seen <= MAX_CATALOG_ROWS,
                "canonical catalog row limit exceeded"
            );
        }
        next = rows
            .last()
            .context("empty catalog page")?
            .get_typed::<i64>(0)?
            .checked_add(1);
    }
    ensure!(seen == count, "canonical catalog scan lost rows");
    Ok(json!({"row_count": count, "sha256": finish(hasher)}))
}

fn optional_table_count(connection: &Connection, table: &str) -> Result<Value> {
    let names = columns(connection, table)?;
    if names.is_empty() {
        return Ok(json!({"exists": false}));
    }
    let mut result = json!({
        "exists": true,
        "row_count": scalar(connection, &format!("SELECT COUNT(*) FROM {table}"))?,
    });
    if names.iter().any(|name| name == "message_count") {
        let filter = if table == "daily_stats" {
            " WHERE agent_slug = 'all' AND source_id = 'all'"
        } else {
            ""
        };
        result["message_count_sum"] = json!(scalar(
            connection,
            &format!("SELECT COALESCE(SUM(message_count), 0) FROM {table}{filter}")
        )?);
    }
    if table == "token_usage" {
        result["total_tokens_sum"] = json!(scalar(
            connection,
            "SELECT COALESCE(SUM(total_tokens), 0) FROM token_usage"
        )?);
    }
    if table == "token_daily_stats" {
        result["total_tokens_sum"] = json!(scalar(
            connection,
            "SELECT COALESCE(SUM(grand_total_tokens), 0) FROM token_daily_stats WHERE agent_slug = 'all' AND source_id = 'all' AND model_family = 'all'"
        )?);
        result["message_count_sum"] = json!(scalar(
            connection,
            "SELECT COALESCE(SUM(user_message_count + assistant_message_count + tool_message_count), 0) FROM token_daily_stats WHERE agent_slug = 'all' AND source_id = 'all' AND model_family = 'all'"
        )?);
    }
    Ok(result)
}

struct MessageSummary {
    count: u64,
    bytes: u64,
    sha256: String,
}

fn message_summary(connection: &Connection, names: &[String], id: i64) -> Result<MessageSummary> {
    let idx = column(names, "idx")?;
    let content = column(names, "content")?;
    let mut hasher = table_hasher("messages", names)?;
    let mut next = Some(i64::MIN);
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    while let Some(lower) = next {
        let rows = connection.query_with_params(
            "SELECT * FROM messages INDEXED BY sqlite_autoindex_messages_1 WHERE conversation_id = ?1 AND idx >= ?2 ORDER BY idx LIMIT ?3",
            &[SqliteValue::Integer(id), SqliteValue::Integer(lower), SqliteValue::Integer(PAGE_ROWS)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            hash_row(&mut hasher, row.values())?;
            let width = match &row.values()[content] {
                SqliteValue::Text(text) => u64::try_from(text.as_bytes_direct().len())?,
                _ => bail!("canonical message content is not text"),
            };
            bytes = bytes
                .checked_add(width)
                .context("content byte count overflow")?;
            count = count.checked_add(1).context("message count overflow")?;
        }
        next = rows
            .last()
            .context("empty message page")?
            .get_typed::<i64>(idx)?
            .checked_add(1);
    }
    Ok(MessageSummary {
        count,
        bytes,
        sha256: finish(hasher),
    })
}

fn token_summary(
    connection: &Connection,
    names: &[String],
    conversation_id: i64,
    started_at: Option<i64>,
    agent: &str,
    source: &str,
    oracle: &mut TrackBOracle,
) -> Result<Value> {
    let fields = LedgerColumns::new(names)?;
    let mut hasher = table_hasher("token_usage", names)?;
    // The pinned engine cannot stream ORDER BY id through the conversation
    // index's implicit rowid suffix. Counted LIMIT stops on the last match;
    // an upper predicate alone need not stop traversal of the unrelated tail.
    // Interleaved IDs can still require reading unrelated rows inside the span.
    let bounds = connection
        .query_row_with_params(TOKEN_BOUNDS_SQL, &[SqliteValue::Integer(conversation_id)])?;
    let (mut next, upper, mut remaining) = match (
        bounds.get_typed::<Option<i64>>(0)?,
        bounds.get_typed::<Option<i64>>(1)?,
        bounds.get_typed::<i64>(2)?,
    ) {
        (Some(lower), Some(upper), count) if lower <= upper && count > 0 => {
            (Some(lower), upper, count)
        }
        (None, None, 0) => (None, i64::MIN, 0),
        _ => bail!("inconsistent ledger bounds"),
    };
    let mut count = 0_u64;
    let mut total_tokens = 0_i64;
    let mut session_model = String::from("unknown");
    while let Some(lower) = next {
        let requested = remaining.min(PAGE_ROWS);
        let rows = connection.query_with_params(
            TOKEN_PAGE_SQL,
            &[
                SqliteValue::Integer(conversation_id),
                SqliteValue::Integer(lower),
                SqliteValue::Integer(upper),
                SqliteValue::Integer(requested),
            ],
        )?;
        ensure!(
            rows.len() == usize::try_from(requested)?,
            "token ledger page lost counted rows"
        );
        let mut previous = None;
        for row in &rows {
            let id: i64 = row.get_typed(fields.id)?;
            ensure!(
                id >= lower && id <= upper && previous.is_none_or(|last| id > last),
                "token ledger page left its ordered bounds"
            );
            previous = Some(id);
            hash_row(&mut hasher, row.values())?;
            let value = row.get_typed::<Option<i64>>(fields.total)?.unwrap_or(0);
            total_tokens = total_tokens
                .checked_add(value)
                .context("token total overflow")?;
            count = count.checked_add(1).context("token row count overflow")?;
            let model = row
                .get_typed::<Option<String>>(fields.model)?
                .unwrap_or_else(|| "unknown".into());
            oracle.record(
                row.get_typed(fields.day)?,
                agent,
                source,
                &model,
                &fields.counters(row)?,
            )?;
            if model != "unknown" {
                session_model = model;
            }
        }
        let last = previous.context("empty counted token page")?;
        remaining -= requested;
        next = if remaining == 0 {
            ensure!(
                last == upper,
                "token ledger ended before its indexed maximum"
            );
            None
        } else {
            Some(
                last.checked_add(1)
                    .filter(|id| *id <= upper)
                    .context("token ledger ended before its indexed count")?,
            )
        };
    }
    oracle.session(started_at, agent, source, &session_model)?;
    Ok(json!({"count": count, "total_tokens": total_tokens, "sha256": finish(hasher)}))
}

fn snapshot(connection: &Connection, database: &Path, quick_check: bool) -> Result<Value> {
    let conversation_columns = columns(connection, "conversations")?;
    let message_columns = columns(connection, "messages")?;
    let token_columns = columns(connection, "token_usage")?;
    let id_column = column(&conversation_columns, "id")?;
    let source_column = column(&conversation_columns, "source_path")?;
    let external_column = column(&conversation_columns, "external_id")?;
    let provenance_column = column(&conversation_columns, "source_id")?;
    let agent_column = column(&conversation_columns, "agent_id")?;
    let started_column = column(&conversation_columns, "started_at")?;
    let mut conversations = Vec::new();
    let mut track_b = TrackBOracle::default();
    let agent_statement = connection.prepare("SELECT slug FROM agents WHERE id = ?1")?;
    let mut next = Some(i64::MIN);
    let mut messages = 0_u64;
    let mut content_bytes = 0_u64;
    let mut token_usage_count = 0_u64;
    while let Some(lower) = next {
        let rows = connection.query_with_params(
            "SELECT * FROM conversations WHERE id >= ?1 ORDER BY id LIMIT ?2",
            &[SqliteValue::Integer(lower), SqliteValue::Integer(PAGE_ROWS)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let id = row.get_typed::<i64>(id_column)?;
            let agent = agent_statement
                .query_row_with_params(&[SqliteValue::Integer(row.get_typed(agent_column)?)])?
                .get_typed::<String>(0)?;
            let source: String = row.get_typed(provenance_column)?;
            let message = message_summary(connection, &message_columns, id)?;
            let tokens = token_summary(
                connection,
                &token_columns,
                id,
                row.get_typed(started_column)?,
                &agent,
                &source,
                &mut track_b,
            )?;
            messages = messages
                .checked_add(message.count)
                .context("archive message count overflow")?;
            content_bytes = content_bytes
                .checked_add(message.bytes)
                .context("archive content size overflow")?;
            token_usage_count = token_usage_count
                .checked_add(tokens["count"].as_u64().context("invalid token count")?)
                .context("archive token row count overflow")?;
            let mut hasher = table_hasher("conversations", &conversation_columns)?;
            hash_row(&mut hasher, row.values())?;
            conversations.push(json!({
                "id": id,
                "source_path": row.get_typed::<Option<String>>(source_column)?,
                "external_id": row.get_typed::<Option<String>>(external_column)?,
                "source_id": source,
                "message_count": message.count,
                "content_bytes": message.bytes,
                "conversation_sha256": finish(hasher),
                "messages_sha256": message.sha256,
                "token_usage_count": tokens["count"],
                "token_usage_total_tokens": tokens["total_tokens"],
                "token_usage_sha256": tokens["sha256"],
            }));
        }
        next = rows
            .last()
            .context("empty conversation page")?
            .get_typed::<i64>(id_column)?
            .checked_add(1);
    }
    let canonical_conversations =
        u64::try_from(scalar(connection, "SELECT COUNT(*) FROM conversations")?)?;
    let canonical_messages = u64::try_from(scalar(connection, "SELECT COUNT(*) FROM messages")?)?;
    let canonical_tokens = u64::try_from(scalar(connection, "SELECT COUNT(*) FROM token_usage")?)?;
    ensure!(
        u64::try_from(conversations.len())? == canonical_conversations,
        "canonical conversation scan lost rows"
    );
    ensure!(
        messages <= canonical_messages,
        "canonical message scan duplicated rows"
    );
    ensure!(
        token_usage_count <= canonical_tokens,
        "token ledger scan duplicated rows"
    );

    let mut analytics = BTreeMap::new();
    for table in [
        "message_metrics",
        "usage_daily",
        "usage_hourly",
        "usage_models_daily",
        "daily_stats",
        "token_usage",
        "token_daily_stats",
    ] {
        analytics.insert(table, optional_table_count(connection, table)?);
    }
    let pending_rows =
        connection.query("SELECT value FROM meta WHERE key = 'forget_derived_rebuild_pending'")?;
    let reader_pending = privacy::repair_pending(database)?;
    let integrity = if quick_check {
        let findings = connection
            .query("PRAGMA quick_check(1)")?
            .iter()
            .map(|row: &Row| Ok(row.get_typed::<String>(0)?))
            .collect::<Result<Vec<_>>>()?;
        Some(
            json!({"ok": !findings.is_empty() && findings.iter().all(|finding| finding == "ok"), "findings": findings}),
        )
    } else {
        None
    };
    Ok(json!({
        "schema_version": 2,
        "digest_format": "cass-gh565-typed-rows-v1: schema plus all columns; type tag, u64 LE byte length, exact bytes; row delimiter and field count; integer/IEEE754 LE",
        "page_rows": PAGE_ROWS,
        "canonical": {
            "conversations": canonical_conversations,
            "messages": canonical_messages,
            "content_bytes": content_bytes,
            "token_usage_rows": canonical_tokens,
            "orphan_messages": canonical_messages - messages,
            "orphan_token_usage_conversations": canonical_tokens - token_usage_count,
        },
        "conversations": conversations,
        "canonical_catalogs": {
            "agents": catalog_summary(connection, "agents")?,
            "workspaces": catalog_summary(connection, "workspaces")?,
            "sources": catalog_summary(connection, "sources")?,
        },
        "canonical_catalog_limits": {"max_rows_per_table": MAX_CATALOG_ROWS,
                                     "max_value_bytes_per_table": MAX_CATALOG_BYTES},
        "tombstones": {
            "forgotten_sources": dump_tombstones(connection, "forgotten_sources")?,
            "forgotten_conversations": dump_tombstones(connection, "forgotten_conversations")?,
        },
        "pending": {"sql_marker_present": !pending_rows.is_empty(), "reader_pending": reader_pending,
                    "effective": !pending_rows.is_empty() || reader_pending},
        "analytics": analytics,
        "track_b_oracle": track_b.report(connection)?,
        "quick_check": integrity,
    }))
}

fn run() -> Result<Value> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(
        args.next()
            .context("usage: forget_qualification_probe DATA_DIR [--quick-check]")?,
    );
    let quick_check = match args.next() {
        Some(option) if option == "--quick-check" => true,
        Some(_) => bail!("only optional argument is --quick-check"),
        None => false,
    };
    ensure!(args.next().is_none(), "unexpected extra arguments");
    let database = admitted_database(&directory)?;
    let path = database.to_str().context("database path must be UTF-8")?;
    // Match the logical archive exporter's non-repairing FrankenSQLite open.
    // FrankenStorage::open_readonly may recover/checkpoint dirty WALs; this
    // evidence probe must not turn an interruption into an implicit repair.
    let connection = open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.execute("PRAGMA query_only = ON")?;
    connection.execute("PRAGMA busy_timeout = 5000")?;
    connection.execute("BEGIN")?;
    let result = snapshot(&connection, &database, quick_check);
    let rollback = connection.execute("ROLLBACK");
    let closed = connection.close_without_checkpoint();
    let result = result?;
    rollback?;
    closed?;
    Ok(result)
}

fn main() -> Result<()> {
    let result = run();
    let retired = coding_agent_search::shutdown_thread_local_bridge_runtimes();
    let snapshot = result?;
    ensure!(
        retired,
        "FrankenSQLite bridge teardown exceeded its deadline"
    );
    println!("{}", serde_json::to_string(&snapshot)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle_fixture() -> Result<Connection> {
        let connection = Connection::open(":memory:")?;
        connection.execute(
            "CREATE TABLE token_usage (
                id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL,
                day_id INTEGER NOT NULL, role TEXT NOT NULL, model_family TEXT,
                input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
                cache_creation_tokens INTEGER, thinking_tokens INTEGER, total_tokens INTEGER,
                content_chars INTEGER NOT NULL, tool_call_count INTEGER NOT NULL)",
        )?;
        connection.execute("CREATE INDEX idx_token_usage_conv ON token_usage(conversation_id)")?;
        let counters = TRACK_B_COUNTERS
            .iter()
            .map(|name| format!("{name} INTEGER NOT NULL DEFAULT 0"))
            .collect::<Vec<_>>()
            .join(", ");
        connection.execute(&format!(
            "CREATE TABLE token_daily_stats (
                day_id INTEGER NOT NULL, agent_slug TEXT NOT NULL, source_id TEXT NOT NULL,
                model_family TEXT NOT NULL, {counters}, estimated_cost_usd REAL NOT NULL DEFAULT 0,
                last_updated INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(day_id, agent_slug, source_id, model_family))"
        ))?;
        Ok(connection)
    }

    fn insert_actual(
        connection: &Connection,
        key: (i64, &str, &str, &str),
        counters: Counters,
    ) -> Result<()> {
        let names = TRACK_B_KEYS
            .iter()
            .chain(TRACK_B_COUNTERS.iter())
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let mut values = vec![
            SqliteValue::Integer(key.0),
            SqliteValue::Text(key.1.into()),
            SqliteValue::Text(key.2.into()),
            SqliteValue::Text(key.3.into()),
        ];
        values.extend(counters.into_iter().map(SqliteValue::Integer));
        let placeholders = (1..=values.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        connection.execute_with_params(
            &format!("INSERT INTO token_daily_stats ({names}) VALUES ({placeholders})"),
            &values,
        )?;
        Ok(())
    }

    #[test]
    fn native_track_b_oracle_detects_equal_total_bucket_and_session_corruption() -> Result<()> {
        let connection = oracle_fixture()?;
        connection.execute(
            "INSERT INTO token_usage VALUES (7, 1, 2, 'agent', 'alpha', 2, 3, 5, 7, 11, 17, 13, 2)",
        )?;
        connection.execute(
            "INSERT INTO token_usage VALUES (8, 1, 2, 'user', 'alpha', 19, NULL, NULL, NULL, 23, 19, 29, 0)",
        )?;
        connection.execute(
            "INSERT INTO token_usage VALUES (9, 1, 2, 'tool', 'alpha', NULL, 31, NULL, NULL, NULL, 31, 37, 5)",
        )?;
        connection.execute(
            "INSERT INTO token_usage VALUES (10, 1, 2, 'system', 'alpha', NULL, NULL, NULL, NULL, 41, NULL, 43, -1)",
        )?;
        let names = columns(&connection, "token_usage")?;
        let mut oracle = TrackBOracle::default();
        token_summary(
            &connection,
            &names,
            1,
            Some(1_578_009_600_000),
            "codex",
            "local",
            &mut oracle,
        )?;
        token_summary(&connection, &names, 2, None, "codex", "local", &mut oracle)?;
        // Literal golden counters: thinking=75 is separate from grand=67;
        // 'agent' counts as assistant, 'system' only as an API row, and the
        // negative tool count contributes zero. No app aggregator builds either side.
        let usage_and_session = [4, 1, 1, 1, 21, 34, 5, 7, 75, 67, 122, 7, 1];
        for key in [
            (2, "codex", "local", "alpha"),
            (2, "all", "local", "alpha"),
            (2, "codex", "all", "alpha"),
            (2, "codex", "local", "all"),
            (2, "all", "all", "all"),
        ] {
            insert_actual(&connection, key, usage_and_session)?;
        }
        let empty_session = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        for key in [
            (0, "codex", "local", "unknown"),
            (0, "all", "local", "unknown"),
            (0, "codex", "all", "unknown"),
            (0, "codex", "local", "all"),
            (0, "all", "all", "all"),
        ] {
            insert_actual(&connection, key, empty_session)?;
        }
        let good = oracle.report(&connection)?;
        assert_eq!(good["matches"], true);
        assert_eq!(good["expected_rows"], 10);
        assert_eq!(good["expected_sha256"], good["actual_sha256"]);
        let global_tokens = || {
            scalar(
                &connection,
                "SELECT SUM(grand_total_tokens) FROM token_daily_stats
                 WHERE agent_slug='all' AND source_id='all' AND model_family='all'",
            )
        };
        assert_eq!(global_tokens()?, 67);

        connection.execute("UPDATE token_daily_stats SET day_id=3 WHERE day_id=2")?;
        let wrong_day = oracle.report(&connection)?;
        assert_eq!(wrong_day["matches"], false);
        assert_eq!(wrong_day["expected_rows"], wrong_day["actual_rows"]);
        assert_ne!(wrong_day["expected_sha256"], wrong_day["actual_sha256"]);
        assert_eq!(global_tokens()?, 67);
        connection.execute("UPDATE token_daily_stats SET day_id=2 WHERE day_id=3")?;

        connection.execute(
            "UPDATE token_daily_stats SET model_family='wrong' WHERE model_family='alpha'",
        )?;
        assert_eq!(oracle.report(&connection)?["matches"], false);
        assert_eq!(global_tokens()?, 67);
        connection.execute(
            "UPDATE token_daily_stats SET model_family='alpha' WHERE model_family='wrong'",
        )?;

        connection.execute("UPDATE token_daily_stats SET session_count=0")?;
        assert_eq!(oracle.report(&connection)?["matches"], false);
        assert_eq!(global_tokens()?, 67);
        connection.execute("UPDATE token_daily_stats SET session_count=1")?;
        connection.execute("UPDATE token_daily_stats SET session_count=2 WHERE day_id=2")?;
        connection.execute("UPDATE token_daily_stats SET session_count=0 WHERE day_id=0")?;
        assert_eq!(
            scalar(
                &connection,
                "SELECT SUM(session_count) FROM token_daily_stats"
            )?,
            10
        );
        assert_eq!(
            oracle.report(&connection)?["matches"],
            false,
            "equal session totals cannot hide wrong attribution"
        );
        connection.execute("UPDATE token_daily_stats SET session_count=1")?;

        connection.execute(
            "UPDATE token_daily_stats SET total_input_tokens=0, total_output_tokens=55 WHERE day_id=2",
        )?;
        assert_eq!(oracle.report(&connection)?["matches"], false);
        assert_eq!(global_tokens()?, 67);
        connection.execute(
            "UPDATE token_daily_stats SET total_input_tokens=21, total_output_tokens=34 WHERE day_id=2",
        )?;
        connection
            .execute("UPDATE token_daily_stats SET estimated_cost_usd=123, last_updated=456")?;
        assert_eq!(
            oracle.report(&connection)?["matches"],
            true,
            "unverified pricing/publication columns are explicit exclusions"
        );

        insert_actual(
            &connection,
            (99, "codex", "local", "extra"),
            [0; TRACK_B_COUNTERS.len()],
        )?;
        assert_eq!(
            oracle.report(&connection)?["matches"],
            false,
            "even a zero-valued extra bucket is incorrect"
        );
        connection.execute("DELETE FROM token_daily_stats")?;
        let partial = oracle.report(&connection)?;
        assert_eq!(partial["matches"], false);
        assert_eq!(partial["actual_rows"], 0);
        assert_eq!(partial["mismatch_count"], 10);
        assert_eq!(
            partial["mismatch_examples"].as_array().unwrap().len(),
            MAX_MISMATCH_EXAMPLES
        );
        Ok(())
    }

    #[test]
    fn native_counted_ledger_pages_preserve_sparse_extreme_ids_and_last_known_model() -> Result<()>
    {
        let connection = oracle_fixture()?;
        let insert = |id: i64, conversation: i64, model: Option<&str>| -> Result<()> {
            connection.execute_with_params(
                "INSERT INTO token_usage VALUES (?1, ?2, 7, 'assistant', ?3, 1, 2, NULL, NULL, NULL, 3, 4, 0)",
                &[
                    SqliteValue::Integer(id),
                    SqliteValue::Integer(conversation),
                    model
                        .map(|value| SqliteValue::Text(value.into()))
                        .unwrap_or(SqliteValue::Null),
                ],
            )?;
            Ok(())
        };
        insert(i64::MIN, 1, Some("first"))?;
        insert(i64::MIN + 1, 9, Some("unrelated-prefix"))?;
        for index in 0_i64..65 {
            insert(
                index * 2,
                1,
                Some(if index == 64 { "last-known" } else { "first" }),
            )?;
            insert(index * 2 + 1, 9, Some("interleaved"))?;
        }
        insert(i64::MAX - 1, 1, Some("unknown"))?;
        insert(i64::MAX, 1, None)?;
        let names = columns(&connection, "token_usage")?;
        let mut oracle = TrackBOracle::default();
        let scanned = token_summary(&connection, &names, 1, None, "codex", "local", &mut oracle)?;
        assert_eq!(scanned["count"], 68);
        assert_eq!(scanned["total_tokens"], 204);
        assert_eq!(
            oracle.expected[&(0, "codex".into(), "local".into(), "last-known".into())][12],
            1
        );
        assert_eq!(
            oracle.expected[&(7, "all".into(), "all".into(), "all".into())][0],
            68
        );
        let empty = token_summary(&connection, &names, 2, None, "codex", "local", &mut oracle)?;
        assert_eq!(empty["count"], 0);
        assert_eq!(
            oracle.expected[&(0, "codex".into(), "local".into(), "unknown".into())][12],
            1
        );
        let middle = token_summary(&connection, &names, 9, None, "codex", "local", &mut oracle)?;
        assert_eq!(middle["count"], 66);
        assert_eq!(middle["total_tokens"], 198);
        assert_eq!(
            scalar(&connection, "SELECT COUNT(*) FROM token_usage")?,
            134
        );
        Ok(())
    }

    #[test]
    fn native_token_page_plan_seeks_lower_and_stops_at_counted_limit() -> Result<()> {
        let connection = oracle_fixture()?;
        let program = connection.query_with_params(
            &format!("EXPLAIN {TOKEN_PAGE_SQL}"),
            &[
                SqliteValue::Integer(9),
                SqliteValue::Integer(-100),
                SqliteValue::Integer(100),
                SqliteValue::Integer(2),
            ],
        )?;
        let opcodes: Vec<(i64, String, i64, i64, i64)> = program
            .iter()
            .map(|row| {
                Ok((
                    row.get_typed(0)?,
                    row.get_typed(1)?,
                    row.get_typed(2)?,
                    row.get_typed(3)?,
                    row.get_typed(4)?,
                ))
            })
            .collect::<Result<_>>()?;
        let root = scalar(
            &connection,
            "SELECT rootpage FROM sqlite_master WHERE name='token_usage'",
        )?;
        let cursor = opcodes
            .iter()
            .find_map(|(_, op, p1, p2, _)| (op == "OpenRead" && *p2 == root).then_some(*p1))
            .context("missing token table cursor")?;
        assert!(opcodes.iter().any(|(_, op, p1, _, p3)| {
            op == "SeekGE"
                && *p1 == cursor
                && opcodes.iter().any(|(_, op, parameter, register, _)| {
                    op == "Variable" && *parameter == 2 && register == p3
                })
        }));
        assert!(opcodes.iter().any(|(address, op, register, exit, _)| {
            op == "DecrJumpZero"
                && opcodes.iter().any(|(_, op, parameter, target, _)| {
                    op == "Variable" && *parameter == 4 && register == target
                })
                && opcodes.iter().any(|(next, op, p1, _, _)| {
                    op == "Next" && *p1 == cursor && next > address && next < exit
                })
        }));
        assert!(!opcodes.iter().any(|(_, op, p1, _, _)| {
            (matches!(op.as_str(), "Rewind" | "Last") && *p1 == cursor)
                || matches!(
                    op.as_str(),
                    "SorterOpen"
                        | "SorterInsert"
                        | "SorterSort"
                        | "OpenEphemeral"
                        | "OpenAutoindex"
                )
        }));
        Ok(())
    }

    #[test]
    fn oracle_caps_dimensions_buckets_and_arithmetic_without_double_counting_all() -> Result<()> {
        assert!(checked_key(0, &"λ".repeat(129), "local", "model").is_err());
        let mut oracle = TrackBOracle::default();
        let mut delta = [0; TRACK_B_COUNTERS.len()];
        delta[0] = 1;
        oracle.record(0, "all", "all", "all", &delta)?;
        assert_eq!(oracle.expected.len(), 1);
        assert_eq!(oracle.expected.values().next().unwrap()[0], 1);
        let mut buckets = Buckets::new();
        for day in 0..MAX_TRACK_B_BUCKETS {
            add_bucket(
                &mut buckets,
                checked_key(i64::try_from(day)?, "a", "s", "m")?,
                &delta,
            )?;
        }
        assert!(add_bucket(&mut buckets, checked_key(i64::MAX, "a", "s", "m")?, &delta).is_err());
        let key = checked_key(0, "a", "s", "m")?;
        buckets.get_mut(&key).unwrap()[0] = i64::MAX;
        assert!(add_bucket(&mut buckets, key, &delta).is_err());
        Ok(())
    }

    #[test]
    fn native_catalog_fingerprint_covers_referenced_metadata() -> Result<()> {
        let connection = Connection::open(":memory:")?;
        connection.execute(
            "CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT, display_name TEXT)",
        )?;
        connection.execute("INSERT INTO workspaces VALUES (1, '/original', 'retained')")?;
        let before = catalog_summary(&connection, "workspaces")?;
        connection.execute("UPDATE workspaces SET path='/wrong' WHERE id=1")?;
        let after = catalog_summary(&connection, "workspaces")?;
        assert_eq!(before["row_count"], after["row_count"]);
        assert_ne!(before["sha256"], after["sha256"]);
        Ok(())
    }

    #[test]
    fn typed_framing_distinguishes_boundaries_types_and_null() -> Result<()> {
        let hash = |rows: &[Vec<SqliteValue>]| -> Result<String> {
            let mut hasher = table_hasher("fixture", &["a".to_owned(), "b".to_owned()])?;
            for row in rows {
                hash_row(&mut hasher, row)?;
            }
            Ok(finish(hasher))
        };
        assert_ne!(
            hash(&[vec![
                SqliteValue::Text("ab".into()),
                SqliteValue::Text("c".into())
            ]])?,
            hash(&[vec![
                SqliteValue::Text("a".into()),
                SqliteValue::Text("bc".into())
            ]])?
        );
        assert_ne!(
            hash(&[vec![SqliteValue::Integer(1)]])?,
            hash(&[vec![SqliteValue::Text("1".into())]])?
        );
        assert_ne!(
            hash(&[vec![SqliteValue::Null]])?,
            hash(&[vec![SqliteValue::Text("".into())]])?
        );
        assert_ne!(
            hash(&[vec![SqliteValue::Integer(1), SqliteValue::Integer(2)]])?,
            hash(&[vec![SqliteValue::Integer(1)], vec![SqliteValue::Integer(2)]])?
        );
        Ok(())
    }
}
