use super::sqlite_artifact_paths;
#[cfg(test)]
use super::{
    sqlite_content_artifact_paths, sqlite_fixed_artifact_paths, sqlite_runtime_artifact_paths,
};
use crate::franken_sync::compat::{
    ConnectionExt, ParamValue, RowExt, Transaction, TransactionExt,
};
use crate::franken_sync::{Connection, Row as FrankenRow, params};
use crate::pages::summary::ExclusionSet;
use crate::ui::time_parser::parse_time_input;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
    exclusions: ExclusionSet,
}

#[derive(Debug)]
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
            exclusions: ExclusionSet::new(),
        }
    }

    /// Apply wizard review exclusions to the rows eligible for this export.
    ///
    /// Direct and config-driven exports do not call this method, so their
    /// existing positive-filter behavior remains unchanged.
    pub fn with_exclusions(mut self, exclusions: ExclusionSet) -> Self {
        self.exclusions = exclusions;
        self
    }

    pub fn execute<F>(&self, progress: F, running: Option<Arc<AtomicBool>>) -> Result<ExportStats>
    where
        F: Fn(usize, usize),
    {
        self.execute_verified(progress, running, |_| Ok(()))
            .map(|(stats, ())| stats)
    }

    /// Build the export in a private sidecar, verify those exact bytes, and
    /// only then atomically publish them at the requested output path.
    ///
    /// The verifier is deliberately invoked after the destination transaction
    /// is committed and closed but before `replace_file_from_temp`. A failed
    /// verifier therefore leaves any prior output untouched and prevents an
    /// unapproved generation from becoming visible.
    pub fn execute_verified<F, V, T>(
        &self,
        progress: F,
        running: Option<Arc<AtomicBool>>,
        verifier: V,
    ) -> Result<(ExportStats, T)>
    where
        F: Fn(usize, usize),
        V: FnOnce(&Path) -> Result<T>,
    {
        let output_path = resolve_export_output_path(&self.source_db_path, &self.output_path)?;
        #[cfg(windows)]
        recover_or_refuse_interrupted_export_publish(&output_path)?;

        if output_path.exists() && output_path.is_dir() {
            bail!(
                "output path points to a directory, expected a file: {}",
                output_path.display()
            );
        }

        // 1. Open source DB
        let src = super::open_existing_sqlite_db(&self.source_db_path)
            .context("Failed to open source database")?;

        // 2. Build into a private writer database, then ask FrankenSQLite to
        // produce a separate, self-contained candidate with VACUUM INTO. A
        // brand-new on-disk connection permanently retains its bootstrap WAL;
        // the engine's bounded image contract therefore explicitly requires
        // VACUUM INTO rather than publishing an in-place writer database.
        let builder_path = unpredictable_atomic_sidecar_path(
            &output_path,
            "builder",
            "pages_export.db",
        )?;
        let temp_output_path =
            unpredictable_atomic_sidecar_path(&output_path, "tmp", "pages_export.db")?;
        let mut retain_temp_on_replace_error = false;
        let mut builder_owned = false;
        let mut candidate_owned = false;
        let result = (|| -> Result<(ExportStats, T)> {
            create_staged_export_file(&builder_path)?;
            builder_owned = true;
            let output_path = builder_path.to_string_lossy().to_string();
            let dest =
                Connection::open(&output_path).context("Failed to create output database")?;

            dest.execute_batch(
                // Pages exports are encrypted/copied as one portable SQLite file.
                // WAL would allow committed schema/data to remain in a sidecar
                // that is not part of the encrypted payload.
                "PRAGMA journal_mode = 'delete';
                 PRAGMA synchronous = NORMAL;
                 PRAGMA busy_timeout = 5000;
                 PRAGMA foreign_keys = ON;",
            )
            .context("Failed to set destination database PRAGMAs")?;

            // Every source row that contributes to one export must come from
            // one SQLite generation. In particular, conversation counts and
            // the later per-conversation message/snippet reads must not straddle
            // concurrent indexing commits.
            let mut src_tx = src
                .transaction()
                .context("Failed to start source database read snapshot")?;
            let mut tx = match dest
                .transaction()
                .context("Failed to start destination export transaction")
            {
                Ok(tx) => tx,
                Err(destination_error) => {
                    return match src_tx
                        .rollback()
                        .context("Failed to close source database read snapshot")
                    {
                        Ok(()) => Err(destination_error),
                        Err(rollback_error) => Err(destination_error.context(format!(
                            "source read-snapshot rollback also failed: {rollback_error:#}"
                        ))),
                    };
                }
            };

            let export_result = (|| -> Result<(usize, usize)> {
                let message_cols = table_columns_in_transaction(&src_tx, "messages")?;
                let has_snippets_table = table_exists_in_transaction(&src_tx, "snippets")?;
                let msg_query = build_message_export_query(&message_cols);

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
                let mut from_where = String::from(
                    " FROM conversations c
             LEFT JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             WHERE 1=1",
                );
                let mut params: Vec<ParamValue> = Vec::new();

                if let Some(agents) = &self.filter.agents {
                    if agents.is_empty() {
                        from_where.push_str(" AND 1=0");
                    } else {
                        from_where.push_str(" AND EXISTS (SELECT 1 FROM agents a2 WHERE a2.id = c.agent_id AND a2.slug IN (");
                        for (i, agent) in agents.iter().enumerate() {
                            if i > 0 {
                                from_where.push_str(", ");
                            }
                            from_where.push('?');
                            params.push(ParamValue::from(agent.clone()));
                        }
                        from_where.push_str("))");
                    }
                }

                // Note: Workspace filtering in source DB might be string matching if paths aren't normalized consistently.
                // Assuming strict matching for now.
                if let Some(workspaces) = &self.filter.workspaces {
                    if workspaces.is_empty() {
                        from_where.push_str(" AND 1=0");
                    } else {
                        from_where.push_str(" AND w.path IN (");
                        for (i, ws) in workspaces.iter().enumerate() {
                            if i > 0 {
                                from_where.push_str(", ");
                            }
                            from_where.push('?');
                            params.push(ParamValue::from(ws.to_string_lossy().to_string()));
                        }
                        from_where.push(')');
                    }
                }

                if let Some(since) = self.filter.since {
                    from_where.push_str(" AND c.started_at >= ?");
                    params.push(ParamValue::from(since.timestamp_millis()));
                }

                if let Some(until) = self.filter.until {
                    from_where.push_str(" AND c.started_at <= ?");
                    params.push(ParamValue::from(until.timestamp_millis()));
                }

                let query = format!(
                    "SELECT c.id, COALESCE(a.slug, 'unknown') as agent, w.path as workspace, c.title, c.source_path, c.started_at, c.ended_at,
             (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) as message_count,
             c.metadata_json
             {from_where}
             ORDER BY c.id"
                );

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
                let mut conv_rows: Vec<ConversationExportRow> =
                    src_tx.query_map_collect(&query, &params, |row: &FrankenRow| {
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
                conv_rows.retain(|(id, _, workspace, title, _, _, _, _, _)| {
                    !self.exclusions.should_exclude(
                        workspace.as_deref(),
                        *id,
                        title.as_deref().unwrap_or(""),
                    )
                });
                let total_convs = conv_rows.len();

                let mut processed = 0;
                let mut msg_processed = 0;

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
                    let msg_rows: Vec<MessageExportRow> = src_tx.query_map_collect(
                        &msg_query,
                        crate::franken_sync::params![*id],
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
                            src_tx.query_map_collect(
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

                Ok((processed, msg_processed))
            })();
            let export_result = match export_result {
                Ok(stats) => match tx
                    .commit()
                    .context("Failed to commit completed destination export transaction")
                {
                    Ok(()) => Ok(stats),
                    Err(commit_error) => match tx
                        .rollback()
                        .context("Failed to roll back destination after commit failure")
                    {
                        Ok(()) => Err(commit_error),
                        Err(rollback_error) => Err(commit_error.context(format!(
                            "destination rollback also failed: {rollback_error:#}"
                        ))),
                    },
                },
                Err(export_error) => match tx
                    .rollback()
                    .context("Failed to roll back incomplete destination export transaction")
                {
                    Ok(()) => Err(export_error),
                    Err(rollback_error) => Err(export_error.context(format!(
                        "destination rollback also failed: {rollback_error:#}"
                    ))),
                },
            };
            // The transaction commits/rolls back through `&mut self`, so the
            // binding still borrows `dest` until it is dropped — and
            // `dest.close()` below moves the connection. End the borrow here.
            drop(tx);
            let source_rollback_result = src_tx
                .rollback()
                .context("Failed to close source database read snapshot");
            let (processed, msg_processed) = match (export_result, source_rollback_result) {
                (Ok(stats), Ok(())) => stats,
                (Err(export_error), Ok(())) => return Err(export_error),
                (Ok(_), Err(rollback_error)) => return Err(rollback_error),
                (Err(export_error), Err(rollback_error)) => {
                    return Err(export_error.context(format!(
                        "source read-snapshot rollback also failed: {rollback_error:#}"
                    )));
                }
            };

            let candidate_path = temp_output_path.to_string_lossy();
            dest.execute_compat("VACUUM INTO ?1;", params![candidate_path.as_ref()])
                .context("Failed to materialize self-contained Pages export candidate")?;
            candidate_owned = true;
            dest.close()
                .context("Failed to close and checkpoint Pages export builder")?;
            enforce_private_candidate_permissions(&temp_output_path)?;
            // Cleanup may remove the main path before reporting a companion
            // error. Relinquish pathname ownership before it starts so an
            // error path never retries against a possible replacement entry.
            builder_owned = false;
            cleanup_sqlite_temp_artifacts(&builder_path)
                .context("Failed to remove closed Pages export builder artifacts")?;
            finalize_staged_sqlite_sidecars(&temp_output_path)
                .context("Failed to finalize staged Pages export as one SQLite main file")?;

            let verification = verifier(&temp_output_path)
                .context("Staged Pages export verification failed")?;
            reject_existing_sqlite_sidecars(&temp_output_path, "verified staged database")
                .context("Staged Pages export verifier left an unbound SQLite sidecar")?;

            replace_file_from_temp(
                &temp_output_path,
                &output_path,
                &mut retain_temp_on_replace_error,
            )
            .context("Failed to install completed export database")?;
            candidate_owned = false;

            Ok((
                ExportStats {
                    conversations_processed: processed,
                    messages_processed: msg_processed,
                },
                verification,
            ))
        })();

        let result = if builder_owned {
            match cleanup_sqlite_temp_artifacts(&builder_path) {
                Ok(()) => result,
                Err(cleanup_error) => match result {
                    Ok(_) => Err(cleanup_error.context(
                        "completed Pages export was not published because its private builder could not be removed",
                    )),
                    Err(export_error) => Err(export_error.context(format!(
                        "failed to remove private Pages export builder artifacts: {cleanup_error:#}"
                    ))),
                },
            }
        } else {
            result
        };

        match result {
            // Only the catastrophic Windows backup/restore failure retains the
            // owned candidate for recovery. Every ordinary rejection after a
            // successful VACUUM reservation removes that exact artifact family;
            // a pre-reservation path collision is preserved rather than guessed
            // to belong to this export.
            Err(export_error) if candidate_owned && !retain_temp_on_replace_error => {
                match cleanup_sqlite_temp_artifacts(&temp_output_path) {
                    Ok(()) => Err(export_error),
                    Err(cleanup_error) => Err(export_error.context(format!(
                        "failed to remove rejected staged export artifacts: {cleanup_error:#}"
                    ))),
                }
            }
            other => other,
        }
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

/// Resolve the destination entry only after its parent exists, then prove it
/// does not name the source database.
///
/// Canonicalizing a not-yet-created output path and falling back to its raw
/// spelling is unsafe: creating a missing parent can make a path containing
/// `..` start resolving to an existing source file. Resolve the parent first
/// and use that stable directory spelling for staging and publication so the
/// alias check and the eventual rename address the same entry.
fn resolve_export_output_path(source_db_path: &Path, output_path: &Path) -> Result<PathBuf> {
    let source_canonical = std::fs::canonicalize(source_db_path).with_context(|| {
        format!(
            "Failed to resolve source database path {}",
            source_db_path.display()
        )
    })?;
    let output_name = output_path.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "export output path has no file name: {}",
            output_path.display()
        )
    })?;
    let output_parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_parent).with_context(|| {
        format!(
            "Failed to create export output directory {}",
            output_parent.display()
        )
    })?;
    let resolved_parent = std::fs::canonicalize(output_parent).with_context(|| {
        format!(
            "Failed to resolve export output directory {}",
            output_parent.display()
        )
    })?;
    let resolved_output = resolved_parent.join(output_name);

    match std::fs::canonicalize(&resolved_output) {
        Ok(output_canonical) if output_canonical == source_canonical => {
            bail!("output path must be different from source database path");
        }
        Ok(_) if existing_regular_files_share_identity(&source_canonical, &resolved_output)? => {
            bail!(
                "output path must not refer to the same filesystem object as the source database"
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to resolve export output path {}",
                    resolved_output.display()
                )
            });
        }
    }

    Ok(resolved_output)
}

fn existing_regular_files_share_identity(first: &Path, second: &Path) -> Result<bool> {
    if !std::fs::metadata(first)
        .with_context(|| format!("Failed to inspect source identity probe {}", first.display()))?
        .is_file()
    {
        return Ok(false);
    }
    if !std::fs::metadata(second)
        .with_context(|| {
            format!(
                "Failed to inspect export output identity probe {}",
                second.display()
            )
        })?
        .is_file()
    {
        return Ok(false);
    }

    let first_file = std::fs::File::open(first)
        .with_context(|| format!("Failed to open source identity probe {}", first.display()))?;
    if !first_file
        .metadata()
        .with_context(|| format!("Failed to inspect source identity probe {}", first.display()))?
        .is_file()
    {
        return Ok(false);
    }
    let second_file = std::fs::File::open(second).with_context(|| {
        format!(
            "Failed to open export output identity probe {}",
            second.display()
        )
    })?;
    if !second_file
        .metadata()
        .with_context(|| {
            format!(
                "Failed to inspect export output identity probe {}",
                second.display()
            )
        })?
        .is_file()
    {
        return Ok(false);
    }

    let first_identity = crate::franken_sync::FileIdentity::from_file(&first_file)
        .context("Failed to identify source database filesystem object")?;
    let second_identity = crate::franken_sync::FileIdentity::from_file(&second_file)
        .context("Failed to identify export output filesystem object")?;
    Ok(first_identity.is_some() && first_identity == second_identity)
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

fn table_columns_in_transaction(
    conn: &Transaction<'_>,
    table_name: &str,
) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table_name})");
    conn.query_map_collect(&pragma, params![], |row: &FrankenRow| {
        row.get_typed::<String>(1)
    })
    .context("Failed to inspect source table schema")
}

fn table_exists_in_transaction(conn: &Transaction<'_>, table_name: &str) -> Result<bool> {
    if !table_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!("invalid SQLite table name: {table_name}");
    }

    table_columns_in_transaction(conn, table_name)
        .map(|columns| !columns.is_empty())
        .with_context(|| format!("Failed to inspect source table {table_name}"))
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
    let value: Value = serde_json::from_str(extra_json?).ok()?;

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
    let value: Value = serde_json::from_str(extra_json?).ok()?;

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

#[cfg(any(windows, test))]
fn export_publish_recovery_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("pages_export.db"));
    let mut backup_name = std::ffi::OsString::from(".");
    backup_name.push(file_name);
    backup_name.push(".pages-export-publish-in-progress.bak");
    path.with_file_name(backup_name)
}

fn unpredictable_atomic_sidecar_path(
    path: &Path,
    suffix: &str,
    fallback_name: &str,
) -> Result<PathBuf> {
    let mut nonce = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("failed to obtain randomness for Pages export staging"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);
    Ok(path.with_file_name(format!(
        ".{file_name}.{suffix}.{}",
        hex::encode(nonce)
    )))
}

fn cleanup_sqlite_sidecars(artifacts: Vec<PathBuf>) -> Result<()> {
    let mut first_error = None;
    for artifact in artifacts {
        match std::fs::remove_file(&artifact) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(anyhow::Error::new(err).context(format!(
                        "failed removing staged SQLite artifact {}",
                        artifact.display()
                    )));
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn finalize_staged_sqlite_sidecars(path: &Path) -> Result<()> {
    // The publishable path is a VACUUM INTO image and is never opened through
    // a read-write FrankenSQLite connection. It therefore owns no companion
    // artifacts at all. Even namespace or lock files at this random pathname
    // are unexpected entries, not builder residue, so preserve and reject
    // them. Only the separately owned, explicitly closed builder is cleaned.
    reject_existing_sqlite_sidecars(path, "staged VACUUM candidate")
}

fn cleanup_sqlite_temp_artifacts(path: &Path) -> Result<()> {
    // Resolve the complete exact family before removing the main path. If the
    // bounded directory scan cannot prove the dynamic WAL-segment set, fail
    // without mutation rather than losing the namespace anchor first.
    let sidecars = sqlite_artifact_paths(path)?;
    let main_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ensure_no_unbound_sqlite_sidecars(path, sidecars);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed inspecting staged SQLite artifact {} before cleanup",
                    path.display()
                )
            });
        }
    };
    if !main_metadata.file_type().is_file() {
        bail!(
            "staged SQLite artifact {} is no longer a regular file; preserved every companion",
            path.display()
        );
    }
    #[cfg(unix)]
    if main_metadata.nlink() != 1 {
        bail!(
            "staged SQLite artifact {} has {} hard links; preserved every companion because exclusive pathname ownership is no longer provable",
            path.display(),
            main_metadata.nlink()
        );
    }

    match std::fs::remove_file(path) {
        Ok(()) => cleanup_sqlite_sidecars(sidecars),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_no_unbound_sqlite_sidecars(path, sidecars)
        }
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "failed removing staged SQLite artifact {}; preserved every companion",
            path.display()
        ))),
    }
}

fn ensure_no_unbound_sqlite_sidecars(path: &Path, sidecars: Vec<PathBuf>) -> Result<()> {
    for sidecar in sidecars {
        match std::fs::symlink_metadata(&sidecar) {
            Ok(_) => {
                bail!(
                    "staged SQLite main file {} disappeared before cleanup while companion {} still exists; preserved the companion because pathname ownership is no longer provable",
                    path.display(),
                    sidecar.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed inspecting staged SQLite companion {} after main-file disappearance",
                        sidecar.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn create_staged_export_file(path: &Path) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .with_context(|| format!("failed securely creating staged export {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn enforce_private_candidate_permissions(path: &Path) -> Result<()> {
    let path_metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed inspecting staged export {}", path.display()))?;
    if !path_metadata.file_type().is_file() {
        bail!("staged Pages export is not a regular file: {}", path.display());
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed opening staged export {} for chmod", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed inspecting staged export {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("staged Pages export is not a regular file: {}", path.display());
    }
    if (path_metadata.dev(), path_metadata.ino()) != (metadata.dev(), metadata.ino()) {
        bail!(
            "staged Pages export {} changed identity before permission enforcement",
            path.display()
        );
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed setting staged export {} to mode 0600", path.display()))?;
    file.sync_all().with_context(|| {
        format!(
            "failed syncing staged export {} after setting mode 0600",
            path.display()
        )
    })?;
    let mode = file
        .metadata()
        .with_context(|| format!("failed verifying staged export mode for {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "staged Pages export {} retained non-owner permission bits after chmod: {mode:o}",
            path.display()
        );
    }
    let final_path_metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed re-inspecting staged export {} after setting mode 0600",
            path.display()
        )
    })?;
    if !final_path_metadata.file_type().is_file()
        || (final_path_metadata.dev(), final_path_metadata.ino())
            != (metadata.dev(), metadata.ino())
    {
        bail!(
            "staged Pages export {} changed identity during permission enforcement",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_private_candidate_permissions(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed inspecting staged export {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("staged Pages export is not a regular file: {}", path.display());
    }
    Ok(())
}

/// Refuse to publish one SQLite main file over an existing artifact family.
///
/// A WAL, shared-memory file, or rollback journal beside `final_path` may
/// contain state belonging to the prior main database generation. Replacing
/// only the main file while retaining any of those sidecars can therefore make
/// readers observe a mixed or corrupt generation. The exporter cannot safely
/// decide that an existing sidecar is stale, so preserve it and fail closed.
fn reject_existing_sqlite_sidecars(path: &Path, artifact_label: &str) -> Result<()> {
    for sidecar in sqlite_artifact_paths(path)? {
        match std::fs::symlink_metadata(&sidecar) {
            Ok(_) => {
                bail!(
                    "refusing main-file-only Pages export publication because {artifact_label} {} has SQLite sidecar {}; close every process using that artifact and preserve or move the complete SQLite family before retrying",
                    path.display(),
                    sidecar.display()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed inspecting SQLite sidecar {} for {artifact_label} {}",
                        sidecar.display(),
                        path.display()
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn replacement_path_entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => {
            Err(err).with_context(|| format!("failed inspecting export path {}", path.display()))
        }
    }
}

#[cfg(any(windows, test))]
fn replace_file_from_temp_via_backup(
    temp_path: &Path,
    final_path: &Path,
    first_err: &std::io::Error,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    *retain_temp_on_error = false;
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
            remove_prior_export_backup_after_publish(&backup_path, final_path)?;
            sync_parent_directory(final_path).with_context(|| {
                format!(
                    "new Pages export is live at {}, but its replacement could not be durably synced",
                    final_path.display()
                )
            })?;
            Ok(())
        }
        Err(second_err) => match std::fs::rename(&backup_path, final_path) {
            Ok(()) => {
                let _ = std::fs::remove_file(temp_path);
                let replacement_error = anyhow::anyhow!(
                    "failed replacing {} with {}: first error: {}; second error: {}; restored original file",
                    final_path.display(),
                    temp_path.display(),
                    first_err,
                    second_err
                );
                match sync_parent_directory(final_path) {
                    Ok(()) => Err(replacement_error),
                    Err(sync_error) => Err(replacement_error.context(format!(
                        "the prior Pages export was restored at {}, but the restoration could not be durably synced: {sync_error:#}",
                        final_path.display()
                    ))),
                }
            }
            Err(restore_err) => {
                *retain_temp_on_error = true;
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

#[cfg(any(windows, test))]
fn remove_prior_export_backup_after_publish(backup_path: &Path, final_path: &Path) -> Result<()> {
    match std::fs::remove_file(backup_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "new Pages export is live at {}, but the prior sensitive generation remains at {}",
            final_path.display(),
            backup_path.display()
        ))),
    }
}

fn replace_file_from_temp(
    temp_path: &Path,
    final_path: &Path,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    *retain_temp_on_error = false;
    reject_existing_sqlite_sidecars(final_path, "destination")?;
    #[cfg(windows)]
    {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path).with_context(|| {
                    format!(
                        "new Pages export is live at {}, but its first publication could not be durably synced",
                        final_path.display()
                    )
                })?;
                Ok(())
            }
            Err(first_err)
                if matches!(
                    first_err.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                if replacement_path_entry_exists(final_path)? {
                    replace_file_from_temp_via_backup(
                        temp_path,
                        final_path,
                        &first_err,
                        retain_temp_on_error,
                    )
                } else {
                    Err(first_err).with_context(|| {
                        format!(
                            "failed renaming completed export {} into place at {}",
                            temp_path.display(),
                            final_path.display()
                        )
                    })
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
        sync_parent_directory(final_path).with_context(|| {
            format!(
                "new Pages export is live at {}, but its publication could not be durably synced",
                final_path.display()
            )
        })
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

    println!("Exporting to {:?}...", output_path);
    let (stats, ()) = export_pages_database_verified(
        db_path,
        output_path,
        agents,
        workspaces,
        since,
        until,
        path_mode,
        |current, total| {
            if total > 0 && current % 100 == 0 {
                use std::io::Write;
                print!("\rProcessed {}/{} conversations...", current, total);
                std::io::stdout().flush().ok();
            }
        },
        |_| Ok(()),
    )?;
    println!(
        "\rExport complete! Processed {} conversations, {} messages.",
        stats.conversations_processed, stats.messages_processed
    );

    Ok(())
}

/// Export a filtered Pages database and verify its private staged generation
/// before the final output path is replaced.
#[allow(clippy::too_many_arguments)]
pub fn export_pages_database_verified<F, V, T>(
    db_path: Option<PathBuf>,
    output_path: PathBuf,
    agents: Option<Vec<String>>,
    workspaces: Option<Vec<String>>,
    since: Option<String>,
    until: Option<String>,
    path_mode: PathMode,
    progress: F,
    verifier: V,
) -> Result<(ExportStats, T)>
where
    F: Fn(usize, usize),
    V: FnOnce(&Path) -> Result<T>,
{
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
    engine.execute_verified(progress, None, verifier)
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
    fn output_resolution_rejects_alias_created_by_missing_parent() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let missing_parent = temp_dir.path().join("created-during-export");
        let output_path = missing_parent.join("..").join("source.db");
        std::fs::write(&source_path, b"source generation")?;

        assert!(
            std::fs::canonicalize(&output_path).is_err(),
            "the regression requires the raw output alias to be unresolved before its parent exists"
        );
        let error = resolve_export_output_path(&source_path, &output_path)
            .expect_err("creating the parent must expose and reject the source alias");

        assert!(
            format!("{error:#}").contains("output path must be different"),
            "unexpected alias rejection: {error:#}"
        );
        assert!(
            missing_parent.is_dir(),
            "the test must cross the state transition that used to make the alias dangerous"
        );
        assert_eq!(
            std::fs::read(&source_path)?,
            b"source generation",
            "alias rejection must preserve the source database"
        );
        Ok(())
    }

    #[test]
    fn output_resolution_returns_entry_under_resolved_created_parent() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("new-parent").join("export.db");
        std::fs::write(&source_path, b"source generation")?;

        let resolved = resolve_export_output_path(&source_path, &output_path)?;

        assert_eq!(
            resolved,
            std::fs::canonicalize(temp_dir.path().join("new-parent"))?.join("export.db")
        );
        assert_ne!(resolved, std::fs::canonicalize(source_path)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn output_resolution_rejects_existing_hard_link_to_source() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");
        std::fs::write(&source_path, b"source generation")?;
        std::fs::hard_link(&source_path, &output_path)?;

        let error = resolve_export_output_path(&source_path, &output_path)
            .expect_err("an existing output with the source identity must be rejected");

        assert!(
            format!("{error:#}").contains("same filesystem object"),
            "unexpected filesystem-identity rejection: {error:#}"
        );
        assert_eq!(std::fs::read(source_path)?, b"source generation");
        assert_eq!(std::fs::read(output_path)?, b"source generation");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replacement_path_entry_exists_detects_dangling_symlink() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new()?;
        let link_path = temp_dir.path().join("export.db");
        let missing_target = temp_dir.path().join("missing-export.db");

        symlink(&missing_target, &link_path)?;

        if link_path.exists() {
            return Err(anyhow::anyhow!(
                "Path::exists stopped following the missing target"
            ));
        }
        if !replacement_path_entry_exists(&link_path)? {
            return Err(anyhow::anyhow!(
                "replacement path helper missed a dangling symlink entry"
            ));
        }

        Ok(())
    }

    #[test]
    fn unique_replace_backup_path_is_not_reused() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let first = unique_replace_backup_path(&final_path);
        let second = unique_replace_backup_path(&final_path);

        if first == second {
            return Err(anyhow::anyhow!(
                "export replacement backup path was reused: {}",
                first.display()
            ));
        }

        Ok(())
    }

    #[test]
    fn vacuum_candidate_path_uses_fresh_unpredictable_names() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let first = unpredictable_atomic_sidecar_path(&final_path, "tmp", "pages_export.db")?;
        let second = unpredictable_atomic_sidecar_path(&final_path, "tmp", "pages_export.db")?;

        assert_ne!(first, second, "candidate nonce was reused");
        for path in [first, second] {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("candidate name is not UTF-8"))?;
            assert!(name.starts_with(".export.db.tmp."), "unexpected name: {name}");
            assert_eq!(
                name.rsplit_once('.').map(|(_, nonce)| nonce.len()),
                Some(32),
                "candidate nonce must carry 128 bits"
            );
        }
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn staged_export_file_is_exclusive_and_owner_only() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        create_staged_export_file(&staged_path)?;

        let mode = std::fs::metadata(&staged_path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(anyhow::anyhow!("staged export mode was {mode:o}"));
        }
        if create_staged_export_file(&staged_path).is_ok() {
            return Err(anyhow::anyhow!(
                "exclusive staging unexpectedly reused an existing path"
            ));
        }
        Ok(())
    }

    #[test]
    fn rejected_export_cleanup_removes_every_sqlite_artifact() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        let wal_segment = temp_dir.path().join("export.tmp.db-wal-seg-not-an-epoch");
        let artifacts = std::iter::once(staged_path.clone())
            .chain(sqlite_fixed_artifact_paths(&staged_path))
            .chain(std::iter::once(wal_segment))
            .collect::<Vec<_>>();
        for artifact in &artifacts {
            std::fs::write(artifact, b"staged bytes")?;
        }

        cleanup_sqlite_temp_artifacts(&staged_path)?;

        for artifact in artifacts {
            if std::fs::symlink_metadata(&artifact).is_ok() {
                return Err(anyhow::anyhow!(
                    "rejected staged SQLite artifact survived cleanup: {}",
                    artifact.display()
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn rejected_export_cleanup_preserves_sidecars_when_main_removal_fails() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        let sidecar_path = sqlite_content_artifact_paths(&staged_path)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("SQLite artifact family unexpectedly empty"))?;
        std::fs::create_dir(&staged_path)?;
        std::fs::write(&sidecar_path, b"recovery bytes")?;

        let error = cleanup_sqlite_temp_artifacts(&staged_path)
            .expect_err("a non-file main path must make cleanup fail closed");

        assert!(
            format!("{error:#}").contains("preserved every companion"),
            "unexpected main-removal error: {error:#}"
        );
        assert_eq!(
            std::fs::read(&sidecar_path)?,
            b"recovery bytes",
            "failed main removal must not destroy a recoverable companion"
        );
        Ok(())
    }

    #[test]
    fn rejected_export_cleanup_preserves_sidecars_after_main_identity_loss() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        let sidecar_path = sqlite_content_artifact_paths(&staged_path)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("SQLite artifact family unexpectedly empty"))?;
        std::fs::write(&sidecar_path, b"unbound recovery bytes")?;

        let error = cleanup_sqlite_temp_artifacts(&staged_path)
            .expect_err("a surviving companion without its main anchor must be preserved");

        assert!(
            format!("{error:#}").contains("pathname ownership is no longer provable"),
            "unexpected missing-main cleanup error: {error:#}"
        );
        assert_eq!(std::fs::read(&sidecar_path)?, b"unbound recovery bytes");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejected_export_cleanup_preserves_replacement_symlink_and_sidecars() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        let replacement_target = temp_dir.path().join("replacement-target.db");
        let sidecar_path = sqlite_content_artifact_paths(&staged_path)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("SQLite artifact family unexpectedly empty"))?;
        std::fs::write(&replacement_target, b"unowned replacement")?;
        symlink(&replacement_target, &staged_path)?;
        std::fs::write(&sidecar_path, b"unowned sidecar")?;

        let error = cleanup_sqlite_temp_artifacts(&staged_path)
            .expect_err("cleanup must not unlink a replacement symlink");

        assert!(
            format!("{error:#}").contains("no longer a regular file"),
            "unexpected replacement-entry error: {error:#}"
        );
        assert_eq!(std::fs::read(&replacement_target)?, b"unowned replacement");
        assert!(
            std::fs::symlink_metadata(&staged_path)?.file_type().is_symlink(),
            "replacement symlink must be preserved"
        );
        assert_eq!(std::fs::read(&sidecar_path)?, b"unowned sidecar");
        Ok(())
    }

    #[test]
    fn staged_finalization_rejects_content_sidecars_without_mutating_them() -> Result<()> {
        let content_paths = sqlite_content_artifact_paths(Path::new("export.tmp.db"));
        for relative_path in content_paths {
            let temp_dir = TempDir::new()?;
            let staged_path = temp_dir.path().join("export.tmp.db");
            let sentinel_path = temp_dir.path().join(relative_path);
            std::fs::write(&staged_path, b"staged main")?;
            std::fs::write(&sentinel_path, b"content-bearing sentinel")?;

            let error = finalize_staged_sqlite_sidecars(&staged_path)
                .expect_err("a content-bearing staged sidecar must block publication");
            let message = format!("{error:#}");
            if !message.contains(&sentinel_path.display().to_string()) {
                return Err(anyhow::anyhow!(
                    "staged sidecar rejection omitted {}: {message}",
                    sentinel_path.display()
                ));
            }
            if std::fs::read(&staged_path)? != b"staged main" {
                return Err(anyhow::anyhow!(
                    "staged sidecar rejection mutated the main file for {}",
                    sentinel_path.display()
                ));
            }
            if std::fs::read(&sentinel_path)? != b"content-bearing sentinel" {
                return Err(anyhow::anyhow!(
                    "staged sidecar rejection mutated the sidecar {}",
                    sentinel_path.display()
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn staged_finalization_rejects_parallel_wal_segments_without_mutation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        let segment_path = temp_dir.path().join("export.tmp.db-wal-seg-not-an-epoch");
        std::fs::write(&staged_path, b"staged main")?;
        std::fs::write(&segment_path, b"parallel WAL segment")?;

        let error = finalize_staged_sqlite_sidecars(&staged_path)
            .expect_err("a parallel WAL segment must block publication");
        assert!(
            format!("{error:#}").contains(&segment_path.display().to_string()),
            "WAL-segment rejection omitted exact artifact path"
        );
        assert_eq!(std::fs::read(&staged_path)?, b"staged main");
        assert_eq!(std::fs::read(&segment_path)?, b"parallel WAL segment");
        Ok(())
    }

    #[test]
    fn staged_vacuum_candidate_rejects_even_a_valid_marker_at_birth() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        let marker_path = crate::pages::sqlite_migration_marker_path(&staged_path);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        std::fs::write(&staged_path, b"staged main")?;
        let marker_bytes = format!(
            r#"{{"last_upgrade_version":1,"last_run_at":{now},"repairs_applied":[]}}"#
        );
        std::fs::write(&marker_path, marker_bytes.as_bytes())?;

        let error = finalize_staged_sqlite_sidecars(&staged_path)
            .expect_err("a VACUUM candidate must never carry a migration marker");
        assert!(
            format!("{error:#}").contains(&marker_path.display().to_string()),
            "candidate-marker rejection omitted exact marker path"
        );
        assert_eq!(std::fs::read(&staged_path)?, b"staged main");
        assert_eq!(std::fs::read(&marker_path)?, marker_bytes.as_bytes());
        Ok(())
    }

    #[test]
    fn staged_finalization_rejects_runtime_sidecars_without_mutation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let staged_path = temp_dir.path().join("export.tmp.db");
        std::fs::write(&staged_path, b"staged main")?;
        let runtime_sidecars = sqlite_runtime_artifact_paths(&staged_path);
        for sidecar in &runtime_sidecars {
            std::fs::write(sidecar, b"unowned runtime sentinel")?;
        }

        let error = finalize_staged_sqlite_sidecars(&staged_path)
            .expect_err("a VACUUM candidate must not consume runtime sidecars");
        assert!(
            runtime_sidecars
                .iter()
                .any(|sidecar| format!("{error:#}").contains(&sidecar.display().to_string())),
            "runtime-sidecar rejection omitted the exact conflicting path"
        );

        if std::fs::read(&staged_path)? != b"staged main" {
            return Err(anyhow::anyhow!(
                "staged finalization mutated the SQLite main file"
            ));
        }
        for sidecar in runtime_sidecars {
            if std::fs::read(&sidecar)? != b"unowned runtime sentinel" {
                return Err(anyhow::anyhow!(
                    "staged finalization mutated runtime sidecar {}",
                    sidecar.display()
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn vacuum_into_detaches_candidate_from_expected_builder_wal() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let builder_path = temp_dir.path().join("builder.db");
        let candidate_path = temp_dir.path().join("candidate.db");
        create_staged_export_file(&builder_path)?;

        let builder = Connection::open(builder_path.to_string_lossy().as_ref())?;
        builder.execute_batch(
            "PRAGMA journal_mode = 'delete';
             CREATE TABLE exported (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO exported VALUES (1, 'candidate row');",
        )?;
        let candidate_path_text = candidate_path.to_string_lossy();
        builder.execute_compat(
            "VACUUM INTO ?1;",
            params![candidate_path_text.as_ref()],
        )?;
        builder.close()?;
        enforce_private_candidate_permissions(&candidate_path)?;

        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&candidate_path)?.permissions().mode() & 0o077,
            0,
            "VACUUM candidate must be private before verification"
        );

        let builder_wal = sqlite_content_artifact_paths(&builder_path)
            .into_iter()
            .find(|path| path.as_os_str().to_string_lossy().ends_with("-wal"))
            .ok_or_else(|| anyhow::anyhow!("shared artifact family omitted builder WAL"))?;
        assert!(
            builder_wal.exists(),
            "test requires pinned FrankenSQLite's retained bootstrap WAL"
        );
        reject_existing_sqlite_sidecars(&candidate_path, "VACUUM INTO candidate")?;
        assert!(
            std::fs::metadata(&candidate_path)?.len() > 0,
            "VACUUM INTO candidate must contain a database image"
        );

        cleanup_sqlite_temp_artifacts(&builder_path)?;
        assert!(
            !builder_wal.exists(),
            "closed private builder WAL survived exact-family cleanup"
        );

        let candidate = crate::pages::open_existing_sqlite_db(&candidate_path)?;
        let row = candidate.query_row("SELECT COUNT(*) FROM exported")?;
        assert_eq!(row.get_typed::<i64>(0)?, 1);
        candidate.close()?;
        Ok(())
    }

    #[test]
    fn replace_file_from_temp_via_backup_overwrites_existing_file() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let temp_path = temp_dir.path().join("export.tmp");
        let first_err = std::io::Error::from(std::io::ErrorKind::AlreadyExists);

        std::fs::write(&final_path, b"old export")?;
        std::fs::write(&temp_path, b"new export")?;

        let mut retain_temp_on_error = false;
        replace_file_from_temp_via_backup(
            &temp_path,
            &final_path,
            &first_err,
            &mut retain_temp_on_error,
        )?;

        if !matches!(
            std::fs::read(&final_path)?.as_slice().cmp(b"new export"),
            std::cmp::Ordering::Equal
        ) {
            return Err(anyhow::anyhow!(
                "backup replacement did not publish temp bytes"
            ));
        }
        if temp_path.exists() {
            return Err(anyhow::anyhow!("export temp path was not consumed"));
        }
        if retain_temp_on_error {
            return Err(anyhow::anyhow!(
                "successful replacement incorrectly requested temp retention"
            ));
        }

        Ok(())
    }

    #[test]
    fn completed_backup_publish_reports_retained_sensitive_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = temp_dir.path().join("export.db.backup");
        std::fs::write(&final_path, b"new live generation")?;
        std::fs::create_dir(&backup_path)?;

        let error = remove_prior_export_backup_after_publish(&backup_path, &final_path)
            .expect_err("an undeletable prior generation must not be silently ignored");

        let message = format!("{error:#}");
        assert!(
            message.contains("new Pages export is live"),
            "partial-success state was not reported: {message}"
        );
        assert!(
            message.contains(&backup_path.display().to_string()),
            "retained backup path was not reported: {message}"
        );
        assert_eq!(std::fs::read(final_path)?, b"new live generation");
        assert!(backup_path.is_dir(), "failed cleanup target must be preserved");
        Ok(())
    }

    #[test]
    fn test_replace_file_from_temp_overwrites_existing_file() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let final_path = temp_dir.path().join("export.db");
        let first_tmp = temp_dir.path().join("first.tmp");
        let second_tmp = temp_dir.path().join("second.tmp");
        let mut retain_temp_on_error = false;

        std::fs::write(&first_tmp, b"first").expect("write first temp");
        replace_file_from_temp(&first_tmp, &final_path, &mut retain_temp_on_error)
            .expect("initial replace");
        assert_eq!(
            std::fs::read(&final_path).expect("read first final"),
            b"first"
        );

        std::fs::write(&second_tmp, b"second").expect("write second temp");
        replace_file_from_temp(&second_tmp, &final_path, &mut retain_temp_on_error)
            .expect("overwrite replace");
        assert_eq!(
            std::fs::read(&final_path).expect("read second final"),
            b"second"
        );
        assert!(!retain_temp_on_error);
    }

    #[test]
    fn replacement_rejects_existing_sqlite_sidecars_without_mutating_artifacts() -> Result<()> {
        let artifact_paths = sqlite_fixed_artifact_paths(Path::new("export.db"));
        for relative_path in artifact_paths {
            let temp_dir = TempDir::new()?;
            let final_path = temp_dir.path().join("export.db");
            let staged_path = temp_dir.path().join("export.tmp.db");
            let sentinel_path = temp_dir.path().join(relative_path);
            let artifact_label = sentinel_path.display().to_string();
            let old_generation = format!("old main for {artifact_label}");
            let new_generation = format!("new main for {artifact_label}");
            let sentinel = format!("sentinel sidecar for {artifact_label}");

            std::fs::write(&final_path, old_generation.as_bytes())?;
            std::fs::write(&staged_path, new_generation.as_bytes())?;
            std::fs::write(&sentinel_path, sentinel.as_bytes())?;

            let mut retain_temp_on_error = false;
            let error = replace_file_from_temp(
                &staged_path,
                &final_path,
                &mut retain_temp_on_error,
            )
            .expect_err("an existing SQLite sidecar must block main-file replacement");

            let message = format!("{error:#}");
            if !message.contains(&sentinel_path.display().to_string()) {
                return Err(anyhow::anyhow!(
                    "sidecar rejection did not identify {}: {message}",
                    sentinel_path.display()
                ));
            }
            if std::fs::read(&final_path)? != old_generation.as_bytes() {
                return Err(anyhow::anyhow!(
                    "sidecar rejection mutated the prior main database for {artifact_label}"
                ));
            }
            if std::fs::read(&staged_path)? != new_generation.as_bytes() {
                return Err(anyhow::anyhow!(
                    "sidecar rejection consumed the staged database for {artifact_label}"
                ));
            }
            if std::fs::read(&sentinel_path)? != sentinel.as_bytes() {
                return Err(anyhow::anyhow!(
                    "sidecar rejection mutated the sentinel artifact for {artifact_label}"
                ));
            }
            if retain_temp_on_error {
                return Err(anyhow::anyhow!(
                    "preflight sidecar rejection incorrectly marked a catastrophic replacement failure"
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn replacement_rejects_existing_parallel_wal_segment_without_mutation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let staged_path = temp_dir.path().join("export.tmp.db");
        let segment_path = temp_dir.path().join("export.db-wal-seg-42");
        std::fs::write(&final_path, b"old main")?;
        std::fs::write(&staged_path, b"new main")?;
        std::fs::write(&segment_path, b"old WAL segment")?;

        let mut retain_temp_on_error = false;
        let error = replace_file_from_temp(
            &staged_path,
            &final_path,
            &mut retain_temp_on_error,
        )
        .expect_err("an existing WAL segment must block main-file replacement");

        assert!(
            format!("{error:#}").contains(&segment_path.display().to_string()),
            "replacement refusal omitted exact WAL segment path"
        );
        assert_eq!(std::fs::read(&final_path)?, b"old main");
        assert_eq!(std::fs::read(&staged_path)?, b"new main");
        assert_eq!(std::fs::read(&segment_path)?, b"old WAL segment");
        assert!(!retain_temp_on_error);
        Ok(())
    }
}
