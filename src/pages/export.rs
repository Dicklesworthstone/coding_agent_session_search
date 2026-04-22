use crate::ui::time_parser::parse_time_input;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt, TransactionExt};
use frankensqlite::{Connection, Row as FrankenRow, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct ExportFilter {
    pub agents: Option<Vec<String>>,
    pub workspaces: Option<Vec<PathBuf>>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub path_mode: PathMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PathMode {
    Relative,
    Basename,
    Full,
    Hash,
}

pub struct ExportEngine {
    source_db_path: PathBuf,
    output_path: PathBuf,
    filter: ExportFilter,
}

pub struct ExportStats {
    pub conversations_processed: usize,
    pub messages_processed: usize,
}

type SnippetExportRow = (
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    String,
);

impl ExportEngine {
    pub fn new(source_db_path: &Path, output_path: &Path, filter: ExportFilter) -> Self {
        Self {
            source_db_path: source_db_path.to_path_buf(),
            output_path: output_path.to_path_buf(),
            filter,
        }
    }

    pub fn execute<F>(&self, progress: F, running: Option<Arc<AtomicBool>>) -> Result<ExportStats>
    where
        F: Fn(usize, usize),
    {
        let src_canon = std::fs::canonicalize(&self.source_db_path)
            .unwrap_or_else(|_| self.source_db_path.clone());
        let out_canon =
            std::fs::canonicalize(&self.output_path).unwrap_or_else(|_| self.output_path.clone());
        if src_canon == out_canon {
            bail!("output path must be different from source database path");
        }

        if self.output_path.exists() && self.output_path.is_dir() {
            bail!(
                "output path points to a directory, expected a file: {}",
                self.output_path.display()
            );
        }

        if let Some(parent) = self.output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create export output directory {}",
                    parent.display()
                )
            })?;
        }

        // 1. Open source DB
        let src = super::open_existing_sqlite_db(&self.source_db_path)
            .context("Failed to open source database")?;

        // 2. Build the export into a unique temp database, then atomically
        // replace the final output only after a successful commit.
        let temp_output_path = unique_atomic_temp_path(&self.output_path);
        let mut replace_attempted = false;
        let result = (|| -> Result<ExportStats> {
            let output_path = temp_output_path.to_string_lossy().to_string();
            let dest =
                Connection::open(&output_path).context("Failed to create output database")?;

            let (processed, msg_processed) = {
                let mut tx = dest.transaction()?;

                // 3. Create Schema (Split into individual statements)
                tx.execute(
                    "CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent TEXT NOT NULL,
                workspace TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                message_count INTEGER,
                metadata_json TEXT
            )",
                )
                .context("Failed to create conversations table")?;

                tx.execute(
                    "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER,
                updated_at INTEGER,
                model TEXT,
                attachment_refs TEXT,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            )",
                )
                .context("Failed to create messages table")?;

                tx.execute(
                    "CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT,
                FOREIGN KEY (message_id) REFERENCES messages(id)
            )",
                )
                .context("Failed to create snippets table")?;

                tx.execute(
                    "CREATE TABLE export_meta (
                key TEXT PRIMARY KEY,
                value TEXT
            )",
                )
                .context("Failed to create export_meta table")?;

                tx.execute(
                    "CREATE VIRTUAL TABLE messages_fts USING fts5(
                content,
                tokenize='porter unicode61 remove_diacritics 2'
            )",
                )
                .context("Failed to create messages_fts table")?;

                tx.execute(
                    r#"CREATE VIRTUAL TABLE messages_code_fts USING fts5(
                content,
                tokenize="unicode61 tokenchars '-_./:@#$%\\'"
            )"#,
                )
                .context("Failed to create messages_code_fts table")?;

                // 4. Query Source.  LEFT JOIN + COALESCE on agents so the
                // export path includes legacy NULL-agent conversations
                // (otherwise the exported archive silently omits them).
                // Agent filter becomes an EXISTS guard against the agents
                // table so it works correctly without the joined column.
                let mut query = String::from(
                "SELECT c.id, COALESCE(a.slug, 'unknown') as agent, w.path as workspace, c.title, c.source_path, c.started_at, c.ended_at,
             (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) as message_count,
             c.metadata_json
             FROM conversations c
             LEFT JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             WHERE 1=1"
            );
                let mut params: Vec<ParamValue> = Vec::new();

                if let Some(agents) = &self.filter.agents {
                    if agents.is_empty() {
                        query.push_str(" AND 1=0");
                    } else {
                        query.push_str(" AND EXISTS (SELECT 1 FROM agents a2 WHERE a2.id = c.agent_id AND a2.slug IN (");
                        for (i, agent) in agents.iter().enumerate() {
                            if i > 0 {
                                query.push_str(", ");
                            }
                            query.push('?');
                            params.push(ParamValue::from(agent.clone()));
                        }
                        query.push_str("))");
                    }
                }

                // Note: Workspace filtering in source DB might be string matching if paths aren't normalized consistently.
                // Assuming strict matching for now.
                if let Some(workspaces) = &self.filter.workspaces {
                    if workspaces.is_empty() {
                        query.push_str(" AND 1=0");
                    } else {
                        query.push_str(" AND w.path IN (");
                        for (i, ws) in workspaces.iter().enumerate() {
                            if i > 0 {
                                query.push_str(", ");
                            }
                            query.push('?');
                            params.push(ParamValue::from(ws.to_string_lossy().to_string()));
                        }
                        query.push(')');
                    }
                }

                if let Some(since) = self.filter.since {
                    query.push_str(" AND c.started_at >= ?");
                    params.push(ParamValue::from(since.timestamp_millis()));
                }

                if let Some(until) = self.filter.until {
                    query.push_str(" AND c.started_at <= ?");
                    params.push(ParamValue::from(until.timestamp_millis()));
                }

                // Count total for progress
                let count_query = format!("SELECT COUNT(*) FROM ({})", query);
                let total_convs: usize =
                    src.query_row_map(&count_query, &params, |row: &FrankenRow| {
                        row.get_typed::<i64>(0).map(|v| v as usize)
                    })?;

                // Execute Main Query - collect all conversation rows
                type ConversationExportRow = (
                    i64,
                    String,
                    Option<String>,
                    Option<String>,
                    String,
                    Option<i64>,
                    Option<i64>,
                    i64,
                    Option<String>,
                );
                let conv_rows: Vec<ConversationExportRow> =
                    src.query_map_collect(&query, &params, |row: &FrankenRow| {
                        Ok((
                            row.get_typed::<i64>(0)?,
                            row.get_typed::<String>(1)?,
                            row.get_typed::<Option<String>>(2)?,
                            row.get_typed::<Option<String>>(3)?,
                            row.get_typed::<String>(4)?,
                            row.get_typed::<Option<i64>>(5)?,
                            row.get_typed::<Option<i64>>(6)?,
                            row.get_typed::<i64>(7)?,
                            row.get_typed::<Option<String>>(8)?,
                        ))
                    })?;

                let mut processed = 0;
                let mut msg_processed = 0;
                let message_cols = table_columns(&src, "messages")?;
                let has_snippets_table = table_exists(&src, "snippets");
                let msg_query = build_message_export_query(&message_cols);

                for (
                    id,
                    agent,
                    workspace,
                    title,
                    source_path,
                    started_at,
                    ended_at,
                    message_count,
                    metadata_json,
                ) in &conv_rows
                {
                    if let Some(r) = &running
                        && !r.load(Ordering::Relaxed)
                    {
                        return Err(anyhow::anyhow!("Export cancelled"));
                    }

                    // Transform Path
                    let transformed_path = self.transform_path(source_path, workspace);

                    tx.execute_compat(
                    "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, ended_at, message_count, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        *id,
                        agent.as_str(),
                        workspace.as_deref(),
                        title.as_deref(),
                        transformed_path.as_str(),
                        *started_at,
                        *ended_at,
                        *message_count,
                        metadata_json.as_deref()
                    ],
                )?;

                    // Fetch messages for this conversation
                    let msg_rows: Vec<MessageExportRow> = src.query_map_collect(
                        &msg_query,
                        frankensqlite::params![*id],
                        |row: &FrankenRow| {
                            Ok((
                                row.get_typed::<i64>(0)?,
                                row.get_typed::<String>(1)?,
                                row.get_typed::<String>(2)?,
                                row.get_typed::<Option<i64>>(3)?,
                                row.get_typed::<i64>(4)?,
                                row.get_typed::<Option<i64>>(5)?,
                                row.get_typed::<Option<String>>(6)?,
                                row.get_typed::<Option<String>>(7)?,
                                row.get_typed::<Option<String>>(8)?,
                            ))
                        },
                    )?;

                    for (
                        source_message_id,
                        role,
                        content,
                        created_at,
                        idx,
                        updated_at,
                        model,
                        attachment_refs,
                        extra_json,
                    ) in &msg_rows
                    {
                        let resolved_model = normalize_optional_text(model.clone())
                            .or_else(|| derive_message_model(extra_json.as_deref()));
                        let resolved_attachment_refs =
                            normalize_optional_text(attachment_refs.clone())
                                .or_else(|| derive_attachment_refs(extra_json.as_deref()));

                        tx.execute_compat(
                            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at, updated_at, model, attachment_refs)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            params![
                                *source_message_id,
                                *id,
                                *idx,
                                role.as_str(),
                                content.as_str(),
                                *created_at,
                                *updated_at,
                                resolved_model.as_deref(),
                                resolved_attachment_refs.as_deref()
                            ],
                        )?;

                        // Populate FTS
                        tx.execute_compat(
                            "INSERT INTO messages_fts (rowid, content) VALUES (?1, ?2)",
                            params![*source_message_id, content.as_str()],
                        )?;
                        tx.execute_compat(
                            "INSERT INTO messages_code_fts (rowid, content) VALUES (?1, ?2)",
                            params![*source_message_id, content.as_str()],
                        )?;

                        // 5. Migrate Snippets for this message (bd-4x92)
                        let snip_rows: Vec<SnippetExportRow> = if has_snippets_table {
                            src.query_map_collect(
                                "SELECT file_path, start_line, end_line, language, snippet_text FROM snippets WHERE message_id = ?1",
                                params![*source_message_id],
                                |row: &FrankenRow| {
                                    Ok((
                                        row.get_typed::<Option<String>>(0)?,
                                        row.get_typed::<Option<i64>>(1)?,
                                        row.get_typed::<Option<i64>>(2)?,
                                        row.get_typed::<Option<String>>(3)?,
                                        row.get_typed::<String>(4)?,
                                    ))
                                },
                            )?
                        } else {
                            Vec::new()
                        };

                        for (fpath, start, end, lang, stext) in snip_rows {
                            tx.execute_compat(
                                "INSERT INTO snippets (message_id, file_path, start_line, end_line, language, snippet_text)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                params![*source_message_id, fpath, start, end, lang, stext.as_str()],
                            )?;
                        }

                        msg_processed += 1;
                    }

                    processed += 1;
                    progress(processed, total_convs);
                }

                // Metadata
                tx.execute("INSERT INTO export_meta (key, value) VALUES ('schema_version', '1')")?;
                let exported_at = Utc::now().to_rfc3339();
                tx.execute_compat(
                    "INSERT INTO export_meta (key, value) VALUES ('exported_at', ?1)",
                    params![exported_at.as_str()],
                )?;

                tx.commit()?;
                (processed, msg_processed)
            };
            drop(dest);

            replace_attempted = true;
            replace_file_from_temp(&temp_output_path, &self.output_path)
                .context("Failed to install completed export database")?;

            Ok(ExportStats {
                conversations_processed: processed,
                messages_processed: msg_processed,
            })
        })();

        if result.is_err() && !replace_attempted {
            cleanup_sqlite_temp_artifacts(&temp_output_path);
        }

        result
    }

    fn transform_path(&self, path: &str, workspace: &Option<String>) -> String {
        match self.filter.path_mode {
            PathMode::Relative => {
                if let Some(ws) = workspace {
                    let ws_path = Path::new(ws);
                    let path_obj = Path::new(path);
                    if let Ok(stripped) = path_obj.strip_prefix(ws_path) {
                        return stripped
                            .to_string_lossy()
                            .trim_start_matches(['/', '\\'])
                            .to_string();
                    }
                }
                path.to_string()
            }
            PathMode::Basename => Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string()),
            PathMode::Full => path.to_string(),
            PathMode::Hash => {
                let mut hasher = Sha256::new();
                hasher.update(path.as_bytes());
                // sha2 ≥ 0.11 dropped `LowerHex` on the digest output;
                // `hex::encode` gives the same lowercase-hex string.
                hex::encode(hasher.finalize())[..16].to_string()
            }
        }
    }
}

type MessageExportRow = (
    i64,
    String,
    String,
    Option<i64>,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn table_columns(conn: &Connection, table_name: &str) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table_name})");
    conn.query_map_collect(&pragma, params![], |row: &FrankenRow| {
        row.get_typed::<String>(1)
    })
    .context("Failed to inspect source table schema")
}

fn table_exists(conn: &Connection, table_name: &str) -> bool {
    if !table_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return false;
    }

    table_columns(conn, table_name)
        .map(|columns| !columns.is_empty())
        .unwrap_or(false)
}

fn build_message_export_query(columns: &[String]) -> String {
    let has_updated_at = columns.iter().any(|col| col == "updated_at");
    let has_model = columns.iter().any(|col| col == "model");
    let has_attachment_refs = columns.iter().any(|col| col == "attachment_refs");
    let has_extra_json = columns.iter().any(|col| col == "extra_json");

    format!(
        "SELECT id, role, content, created_at, idx, {}, {}, {}, {}
         FROM messages
         WHERE conversation_id = ?1
         ORDER BY idx ASC",
        if has_updated_at {
            "updated_at"
        } else {
            "NULL AS updated_at"
        },
        if has_model { "model" } else { "NULL AS model" },
        if has_attachment_refs {
            "attachment_refs"
        } else {
            "NULL AS attachment_refs"
        },
        if has_extra_json {
            "extra_json"
        } else {
            "NULL AS extra_json"
        }
    )
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn derive_message_model(extra_json: Option<&str>) -> Option<String> {
    let value = parse_message_extra_json(extra_json)?;

    [
        value.pointer("/model"),
        value.pointer("/cass/model"),
        value.pointer("/model_id"),
        value.pointer("/message/model"),
        value.pointer("/message/model_id"),
        value.pointer("/metadata/model"),
    ]
    .into_iter()
    .flatten()
    .find_map(|candidate| candidate.as_str())
    .map(str::trim)
    .filter(|candidate| !candidate.is_empty())
    .map(ToOwned::to_owned)
}

fn derive_attachment_refs(extra_json: Option<&str>) -> Option<String> {
    let value = parse_message_extra_json(extra_json)?;

    [
        value.pointer("/attachment_refs"),
        value.pointer("/attachments"),
        value.pointer("/cass/attachment_refs"),
        value.pointer("/cass/attachments"),
        value.pointer("/attachmentRefs"),
        value.pointer("/message/attachment_refs"),
        value.pointer("/message/attachments"),
        value.pointer("/metadata/attachment_refs"),
        value.pointer("/metadata/attachments"),
    ]
    .into_iter()
    .flatten()
    .find_map(|candidate| {
        if candidate.is_null() {
            None
        } else {
            serde_json::to_string(candidate).ok()
        }
    })
}

fn parse_message_extra_json(extra_json: Option<&str>) -> Option<Value> {
    serde_json::from_str(extra_json?).ok()
}

fn unique_atomic_temp_path(path: &Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "tmp", "pages_export.db")
}

#[cfg(windows)]
fn unique_replace_backup_path(path: &Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "bak", "pages_export.db")
}

fn unique_atomic_sidecar_path(path: &Path, suffix: &str, fallback_name: &str) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

fn cleanup_sqlite_temp_artifacts(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar_path(path, "-wal"));
    let _ = std::fs::remove_file(sidecar_path(path, "-shm"));
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "pages_export.db".to_string());
    path.with_file_name(format!("{file_name}{suffix}"))
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path)?;
                Ok(())
            }
            Err(first_err)
                if final_path.exists()
                    && matches!(
                        first_err.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
            {
                let backup_path = unique_replace_backup_path(final_path);
                std::fs::rename(final_path, &backup_path).with_context(|| {
                    let _ = std::fs::remove_file(temp_path);
                    format!(
                        "failed preparing backup {} before replacing {} after initial rename error: {}",
                        backup_path.display(),
                        final_path.display(),
                        first_err
                    )
                })?;

                match std::fs::rename(temp_path, final_path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&backup_path);
                        sync_parent_directory(final_path)?;
                        Ok(())
                    }
                    Err(second_err) => match std::fs::rename(&backup_path, final_path) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(temp_path);
                            sync_parent_directory(final_path)?;
                            bail!(
                                "failed replacing {} with {}: first error: {}; second error: {}; restored original file",
                                final_path.display(),
                                temp_path.display(),
                                first_err,
                                second_err
                            );
                        }
                        Err(restore_err) => {
                            bail!(
                                "failed replacing {} with {}: first error: {}; second error: {}; restore error: {}; temp file retained at {}",
                                final_path.display(),
                                temp_path.display(),
                                first_err,
                                second_err,
                                restore_err,
                                temp_path.display()
                            );
                        }
                    },
                }
            }
            Err(rename_err) => Err(rename_err).with_context(|| {
                format!(
                    "failed renaming completed export {} into place at {}",
                    temp_path.display(),
                    final_path.display()
                )
            }),
        }
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, final_path).with_context(|| {
            format!(
                "failed renaming completed export {} into place at {}",
                temp_path.display(),
                final_path.display()
            )
        })?;
        sync_parent_directory(final_path)
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)
        .with_context(|| format!("failed opening parent directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed syncing parent directory {}", parent.display()))
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run_pages_export(
    db_path: Option<PathBuf>,
    output_path: PathBuf,
    agents: Option<Vec<String>>,
    workspaces: Option<Vec<String>>,
    since: Option<String>,
    until: Option<String>,
    path_mode: PathMode,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        println!("Dry run: would export to {:?}", output_path);
        return Ok(());
    }

    let db_path = db_path.unwrap_or_else(crate::default_db_path);

    let since_dt = parse_export_time_arg("--since", since.as_deref())?;
    let until_dt = parse_export_time_arg("--until", until.as_deref())?;

    if let (Some(since_dt), Some(until_dt)) = (since_dt, until_dt)
        && since_dt > until_dt
    {
        bail!(
            "Invalid time range: --since ({}) is after --until ({})",
            since_dt.to_rfc3339(),
            until_dt.to_rfc3339()
        );
    }

    let workspaces_path = workspaces.map(|ws| ws.into_iter().map(PathBuf::from).collect());

    let filter = ExportFilter {
        agents,
        workspaces: workspaces_path,
        since: since_dt,
        until: until_dt,
        path_mode,
    };

    let engine = ExportEngine::new(&db_path, &output_path, filter);

    println!("Exporting to {:?}...", output_path);
    let stats = engine.execute(
        |current, total| {
            if total > 0 && current % 100 == 0 {
                use std::io::Write;
                print!("\rProcessed {}/{} conversations...", current, total);
                std::io::stdout().flush().ok();
            }
        },
        None,
    )?;
    println!(
        "\rExport complete! Processed {} conversations, {} messages.",
        stats.conversations_processed, stats.messages_processed
    );

    Ok(())
}

fn parse_export_time_arg(
    flag_name: &str,
    raw_value: Option<&str>,
) -> Result<Option<DateTime<Utc>>> {
    let Some(raw_value) = raw_value else {
        return Ok(None);
    };

    let timestamp = parse_time_input(raw_value)
        .ok_or_else(|| anyhow::anyhow!("Invalid {flag_name} value: {raw_value}"))?;
    let parsed = DateTime::from_timestamp_millis(timestamp)
        .ok_or_else(|| anyhow::anyhow!("{flag_name} value is out of range: {raw_value}"))?;
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone};
    use std::path::Path;
    use tempfile::TempDir;

    // ==================== ExportFilter tests ====================

    #[test]
    fn test_export_filter_default_values() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        assert!(filter.agents.is_none());
        assert!(filter.workspaces.is_none());
        assert!(filter.since.is_none());
        assert!(filter.until.is_none());
        assert_eq!(filter.path_mode, PathMode::Full);
    }

    #[test]
    fn test_export_filter_with_agents() {
        let filter = ExportFilter {
            agents: Some(vec!["claude".to_string(), "codex".to_string()]),
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let agents = filter.agents.as_ref().unwrap();
        assert_eq!(agents.len(), 2);
        assert!(agents.contains(&"claude".to_string()));
        assert!(agents.contains(&"codex".to_string()));
    }

    #[test]
    fn test_export_filter_with_workspaces() {
        let filter = ExportFilter {
            agents: None,
            workspaces: Some(vec![
                PathBuf::from("/home/user/project1"),
                PathBuf::from("/home/user/project2"),
            ]),
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };

        let workspaces = filter.workspaces.as_ref().unwrap();
        assert_eq!(workspaces.len(), 2);
    }

    #[test]
    fn test_export_filter_with_time_range() {
        let since = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2025, 12, 31, 23, 59, 59).unwrap();

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: Some(since),
            until: Some(until),
            path_mode: PathMode::Hash,
        };

        assert_eq!(filter.since.unwrap().year(), 2025);
        assert_eq!(filter.until.unwrap().month(), 12);
    }

    #[test]
    fn test_export_filter_clone() {
        let filter = ExportFilter {
            agents: Some(vec!["gemini".to_string()]),
            workspaces: Some(vec![PathBuf::from("/tmp/test")]),
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        let cloned = filter.clone();
        assert_eq!(cloned.agents, filter.agents);
        assert_eq!(cloned.workspaces, filter.workspaces);
        assert_eq!(cloned.path_mode, filter.path_mode);
    }

    // ==================== PathMode tests ====================

    #[test]
    fn test_path_mode_equality() {
        assert_eq!(PathMode::Relative, PathMode::Relative);
        assert_eq!(PathMode::Basename, PathMode::Basename);
        assert_eq!(PathMode::Full, PathMode::Full);
        assert_eq!(PathMode::Hash, PathMode::Hash);
    }

    #[test]
    fn test_path_mode_inequality() {
        assert_ne!(PathMode::Relative, PathMode::Full);
        assert_ne!(PathMode::Basename, PathMode::Hash);
        assert_ne!(PathMode::Full, PathMode::Relative);
    }

    #[test]
    fn test_path_mode_clone() {
        let mode = PathMode::Hash;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }

    #[test]
    fn test_path_mode_copy() {
        let mode = PathMode::Relative;
        let copied: PathMode = mode;
        assert_eq!(copied, PathMode::Relative);
    }

    #[test]
    fn test_path_mode_debug() {
        let debug_str = format!("{:?}", PathMode::Full);
        assert!(debug_str.contains("Full"));
    }

    // ==================== ExportEngine::new() tests ====================

    #[test]
    fn test_export_engine_new_stores_paths() {
        let source = Path::new("/tmp/source.db");
        let output = Path::new("/tmp/output.db");
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        let engine = ExportEngine::new(source, output, filter);

        assert_eq!(engine.source_db_path, PathBuf::from("/tmp/source.db"));
        assert_eq!(engine.output_path, PathBuf::from("/tmp/output.db"));
    }

    #[test]
    fn test_export_engine_new_with_relative_paths() {
        let source = Path::new("relative/source.db");
        let output = Path::new("relative/output.db");
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };

        let engine = ExportEngine::new(source, output, filter);

        assert_eq!(engine.source_db_path, PathBuf::from("relative/source.db"));
        assert_eq!(engine.output_path, PathBuf::from("relative/output.db"));
    }

    // ==================== ExportEngine::transform_path() tests ====================

    #[test]
    fn test_transform_path_full_mode() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/home/user/project/file.rs", &None);
        assert_eq!(result, "/home/user/project/file.rs");
    }

    #[test]
    fn test_transform_path_full_mode_with_workspace() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let workspace = Some("/home/user/project".to_string());
        let result = engine.transform_path("/home/user/project/src/main.rs", &workspace);
        // Full mode ignores workspace
        assert_eq!(result, "/home/user/project/src/main.rs");
    }

    #[test]
    fn test_transform_path_basename_mode() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/home/user/project/src/main.rs", &None);
        assert_eq!(result, "main.rs");
    }

    #[test]
    fn test_transform_path_basename_mode_nested() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/very/deep/nested/path/to/file.txt", &None);
        assert_eq!(result, "file.txt");
    }

    #[test]
    fn test_transform_path_basename_mode_no_extension() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/usr/bin/cargo", &None);
        assert_eq!(result, "cargo");
    }

    #[test]
    fn test_transform_path_relative_mode_with_workspace() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let workspace = Some("/home/user/project".to_string());
        let result = engine.transform_path("/home/user/project/src/main.rs", &workspace);
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn test_transform_path_relative_mode_without_workspace() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/home/user/project/src/main.rs", &None);
        // Without workspace, returns full path
        assert_eq!(result, "/home/user/project/src/main.rs");
    }

    #[test]
    fn test_transform_path_relative_mode_path_not_under_workspace() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let workspace = Some("/home/user/project".to_string());
        let result = engine.transform_path("/other/path/file.rs", &workspace);
        // Path not under workspace, returns full path
        assert_eq!(result, "/other/path/file.rs");
    }

    #[test]
    fn test_transform_path_relative_mode_strips_leading_slash() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let workspace = Some("/home/user".to_string());
        let result = engine.transform_path("/home/user/file.rs", &workspace);
        assert_eq!(result, "file.rs");
    }

    #[test]
    fn test_transform_path_hash_mode() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Hash,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/home/user/project/file.rs", &None);
        // Hash should be 16 hex characters
        assert_eq!(result.len(), 16);
        assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_transform_path_hash_mode_deterministic() {
        let filter1 = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Hash,
        };
        let engine1 = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter1);

        let filter2 = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Hash,
        };
        let engine2 = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter2);

        let path = "/home/user/project/file.rs";
        let result1 = engine1.transform_path(path, &None);
        let result2 = engine2.transform_path(path, &None);

        assert_eq!(result1, result2);
    }

    #[test]
    fn test_transform_path_hash_mode_different_paths_different_hashes() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Hash,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result1 = engine.transform_path("/path/one/file.rs", &None);
        let result2 = engine.transform_path("/path/two/file.rs", &None);

        assert_ne!(result1, result2);
    }

    // ==================== ExportStats tests ====================

    #[test]
    fn test_export_stats_default_values() {
        let stats = ExportStats {
            conversations_processed: 0,
            messages_processed: 0,
        };

        assert_eq!(stats.conversations_processed, 0);
        assert_eq!(stats.messages_processed, 0);
    }

    #[test]
    fn test_export_stats_with_values() {
        let stats = ExportStats {
            conversations_processed: 100,
            messages_processed: 5000,
        };

        assert_eq!(stats.conversations_processed, 100);
        assert_eq!(stats.messages_processed, 5000);
    }

    // ==================== Edge case tests ====================

    #[test]
    fn test_transform_path_empty_path() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("", &None);
        assert_eq!(result, "");
    }

    #[test]
    fn test_transform_path_basename_empty_returns_original() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        // Empty path has no file_name
        let result = engine.transform_path("", &None);
        assert_eq!(result, "");
    }

    #[test]
    fn test_transform_path_with_special_characters() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/path/to/file with spaces.rs", &None);
        assert_eq!(result, "file with spaces.rs");
    }

    #[test]
    fn test_transform_path_hash_with_unicode() {
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Hash,
        };
        let engine = ExportEngine::new(Path::new("/tmp/s.db"), Path::new("/tmp/o.db"), filter);

        let result = engine.transform_path("/path/to/файл.rs", &None);
        // Should still produce valid 16-char hex hash
        assert_eq!(result.len(), 16);
        assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_export_filter_empty_agents_list() {
        let filter = ExportFilter {
            agents: Some(vec![]),
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        assert!(filter.agents.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_export_filter_empty_workspaces_list() {
        let filter = ExportFilter {
            agents: None,
            workspaces: Some(vec![]),
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        assert!(filter.workspaces.as_ref().unwrap().is_empty());
    }

    // ==================== Integration-style tests (with real temp files) ====================

    #[test]
    fn test_export_engine_new_with_tempdir() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let source = temp_dir.path().join("source.db");
        let output = temp_dir.path().join("output.db");

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        let engine = ExportEngine::new(&source, &output, filter);

        assert!(engine.source_db_path.starts_with(temp_dir.path()));
        assert!(engine.output_path.starts_with(temp_dir.path()));
    }

    #[test]
    fn test_replace_file_from_temp_overwrites_existing_file() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let final_path = temp_dir.path().join("export.db");
        let first_tmp = temp_dir.path().join("first.tmp");
        let second_tmp = temp_dir.path().join("second.tmp");

        std::fs::write(&first_tmp, b"first").expect("write first temp");
        replace_file_from_temp(&first_tmp, &final_path).expect("initial replace");
        assert_eq!(
            std::fs::read(&final_path).expect("read first final"),
            b"first"
        );

        std::fs::write(&second_tmp, b"second").expect("write second temp");
        replace_file_from_temp(&second_tmp, &final_path).expect("overwrite replace");
        assert_eq!(
            std::fs::read(&final_path).expect("read second final"),
            b"second"
        );
    }
}
