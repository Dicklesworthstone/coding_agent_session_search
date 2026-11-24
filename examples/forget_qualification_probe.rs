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

fn token_summary(connection: &Connection, names: &[String], conversation_id: i64) -> Result<Value> {
    let id = column(names, "id")?;
    let tokens = column(names, "total_tokens")?;
    let mut hasher = table_hasher("token_usage", names)?;
    // The pinned engine cannot stream ORDER BY id through the conversation
    // index's implicit rowid suffix (see FORGET_TOKEN_USAGE_PAGE_SQL). First
    // obtain two scalar bounds without sorting; then page the rowid table.
    // The upper bound also prevents an empty tail page from scanning every
    // later conversation once this conversation's final ledger row is read.
    let bounds = connection.query_row_with_params(
        "SELECT MIN(id), MAX(id) FROM token_usage INDEXED BY idx_token_usage_conv WHERE conversation_id = ?1",
        &[SqliteValue::Integer(conversation_id)],
    )?;
    let mut next = bounds.get_typed::<Option<i64>>(0)?;
    let upper = bounds.get_typed::<Option<i64>>(1)?;
    ensure!(
        next.is_some() == upper.is_some(),
        "inconsistent ledger bounds"
    );
    let upper = upper.unwrap_or(i64::MIN);
    let mut count = 0_u64;
    let mut total_tokens = 0_i64;
    while let Some(lower) = next {
        let rows = connection.query_with_params(
            "SELECT * FROM token_usage NOT INDEXED WHERE conversation_id = ?1 AND id >= ?2 AND id <= ?3 ORDER BY id LIMIT ?4",
            &[SqliteValue::Integer(conversation_id), SqliteValue::Integer(lower), SqliteValue::Integer(upper), SqliteValue::Integer(PAGE_ROWS)],
        )?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            hash_row(&mut hasher, row.values())?;
            let value = row.get_typed::<Option<i64>>(tokens)?.unwrap_or(0);
            total_tokens = total_tokens
                .checked_add(value)
                .context("token total overflow")?;
            count = count.checked_add(1).context("token row count overflow")?;
        }
        next = rows
            .last()
            .context("empty token page")?
            .get_typed::<i64>(id)?
            .checked_add(1);
    }
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
    let mut conversations = Vec::new();
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
            let message = message_summary(connection, &message_columns, id)?;
            let tokens = token_summary(connection, &token_columns, id)?;
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
                "source_id": row.get_typed::<String>(provenance_column)?,
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
        "schema_version": 1,
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
        "tombstones": {
            "forgotten_sources": dump_tombstones(connection, "forgotten_sources")?,
            "forgotten_conversations": dump_tombstones(connection, "forgotten_conversations")?,
        },
        "pending": {"sql_marker_present": !pending_rows.is_empty(), "reader_pending": reader_pending,
                    "effective": !pending_rows.is_empty() || reader_pending},
        "analytics": analytics,
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
