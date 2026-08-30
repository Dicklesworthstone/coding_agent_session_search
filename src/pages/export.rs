use super::sqlite_artifact_paths;
#[cfg(test)]
use super::{
    sqlite_content_artifact_paths, sqlite_fixed_artifact_paths, sqlite_runtime_artifact_paths,
};
use crate::franken_sync::compat::{ConnectionExt, ParamValue, RowExt, Transaction, TransactionExt};
use crate::franken_sync::{Connection, Row as FrankenRow, params};
use crate::pages::summary::ExclusionSet;
use crate::ui::time_parser::parse_time_input;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use ring::rand::{SecureRandom, SystemRandom};
#[cfg(any(windows, test))]
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
#[cfg(any(windows, test))]
use std::io::Write;
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
    /// only then publish them through the platform's verified replacement
    /// protocol at the requested output path.
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
        let publish_guard = acquire_export_publish_guard(&output_path)?;
        #[cfg(windows)]
        let output_path = {
            recover_or_refuse_interrupted_export_publish(&output_path, &publish_guard)?;
            // Recovery can make a formerly absent destination name an alias
            // of the source (for example through a pre-existing hard link).
            // Re-run the source-identity proof against the restored entry
            // before opening either database.
            resolve_export_output_path(&self.source_db_path, &output_path)?
        };
        #[cfg(windows)]
        reject_non_regular_existing_publish_destination(&output_path)?;

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
        let builder_path =
            unpredictable_atomic_sidecar_path(&output_path, "builder", "pages_export.db")?;
        let temp_output_path =
            unpredictable_atomic_sidecar_path(&output_path, "tmp", "pages_export.db")?;
        let mut retain_temp_on_replace_error = false;
        let mut builder_owned = false;
        let mut candidate_owned = false;
        let result = (|| -> Result<(ExportStats, T)> {
            create_staged_export_file(&builder_path)?;
            builder_owned = true;
            // Deliberately NOT named `output_path`: shadowing the publish
            // destination here once routed the final install onto the builder
            // path (gh#418).
            let builder_db_path = builder_path.to_string_lossy().to_string();
            let dest =
                Connection::open(&builder_db_path).context("Failed to create output database")?;

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
                    // 019i2: the workspace is a path too. Under the
                    // obfuscating modes (hash/basename) it must not survive
                    // verbatim — a "hidden metadata" bundle previously kept
                    // every exact local workspace path here.
                    let transformed_workspace = self.transform_workspace(workspace);

                    tx.execute_compat(
                    "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, ended_at, message_count, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        *id,
                        agent.as_str(),
                        transformed_workspace.as_deref(),
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
                            // 019i2: snippet file paths follow the same path
                            // policy as source paths (relative strips the
                            // workspace; hash/basename obfuscate).
                            let fpath = fpath
                                .as_deref()
                                .map(|path| self.transform_path(path, workspace));
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
                // 019i2: record the realized path policy so verify/summary
                // surfaces can report what the bundle actually contains.
                tx.execute_compat(
                    "INSERT INTO export_meta (key, value) VALUES ('path_mode', ?1)",
                    params![self.filter.path_mode.as_meta_str()],
                )?;
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

            let verification =
                verifier(&temp_output_path).context("Staged Pages export verification failed")?;
            reject_existing_sqlite_sidecars(&temp_output_path, "verified staged database")
                .context("Staged Pages export verifier left an unbound SQLite sidecar")?;

            replace_file_from_temp(
                &temp_output_path,
                &output_path,
                &mut retain_temp_on_replace_error,
                &publish_guard,
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

    /// 019i2: workspace paths under the obfuscating modes. `Relative` and
    /// `Full` keep the workspace verbatim (relative source paths are only
    /// meaningful against it); `Basename` keeps the last component; `Hash`
    /// keeps a stable 16-hex digest so per-workspace grouping still joins.
    fn transform_workspace(&self, workspace: &Option<String>) -> Option<String> {
        let ws = workspace.as_deref()?;
        Some(match self.filter.path_mode {
            PathMode::Relative | PathMode::Full => ws.to_string(),
            PathMode::Basename => Path::new(ws)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| ws.to_string()),
            PathMode::Hash => hash_path16(ws),
        })
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
            PathMode::Hash => hash_path16(path),
        }
    }
}

/// Stable 16-hex-char SHA-256 prefix used by `PathMode::Hash` for every
/// exported path-bearing field (source, workspace, snippet).
fn hash_path16(path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    // sha2 ≥ 0.11 dropped `LowerHex` on the digest output;
    // `hex::encode` gives the same lowercase-hex string.
    hex::encode(hasher.finalize())[..16].to_string()
}

impl PathMode {
    /// Stable lowercase name recorded in `export_meta.path_mode`.
    pub fn as_meta_str(self) -> &'static str {
        match self {
            PathMode::Relative => "relative",
            PathMode::Basename => "basename",
            PathMode::Full => "full",
            PathMode::Hash => "hash",
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
        .with_context(|| {
            format!(
                "Failed to inspect source identity probe {}",
                first.display()
            )
        })?
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
        .with_context(|| {
            format!(
                "Failed to inspect source identity probe {}",
                first.display()
            )
        })?
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

fn table_columns_in_transaction(conn: &Transaction<'_>, table_name: &str) -> Result<Vec<String>> {
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
const EXPORT_PUBLISH_JOURNAL_FORMAT: &str = "cass-pages-export-publish-v1";
#[cfg(any(windows, test))]
const EXPORT_PUBLISH_JOURNAL_MAX_BYTES: u64 = 16 * 1024;
#[cfg(any(windows, test))]
const EXPORT_PUBLISH_BACKUP_SCAN_LIMIT: usize = 65_536;

struct ExportPublishGuard {
    final_path: PathBuf,
    lock_path: PathBuf,
    lock_identity: crate::franken_sync::FileIdentity,
    _lock_file: std::fs::File,
}

impl std::fmt::Debug for ExportPublishGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExportPublishGuard")
            .field("final_path", &self.final_path)
            .field("lock_path", &self.lock_path)
            .finish_non_exhaustive()
    }
}

struct ExportFileEvidence {
    identity: crate::franken_sync::FileIdentity,
    size_bytes: u64,
    sha256: String,
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportPublishJournal {
    format: String,
    backup_file_name: String,
    prior_size_bytes: u64,
    prior_sha256: String,
    candidate_size_bytes: u64,
    candidate_sha256: String,
}

fn export_publish_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("pages_export.db"));
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".pages-export-publish.lock");
    path.with_file_name(lock_name)
}

fn acquire_export_publish_guard(final_path: &Path) -> Result<ExportPublishGuard> {
    let lock_path = export_publish_lock_path(final_path);
    let lock_exists = match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            if metadata.nlink() != 1 {
                bail!(
                    "Pages export publish lock {} has {} hard links; refused to open it because exclusive pathname ownership is not provable",
                    lock_path.display(),
                    metadata.nlink()
                );
            }
            true
        }
        Ok(_) => {
            bail!(
                "Pages export publish lock is not a regular file; refused to open it: {}",
                lock_path.display()
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed inspecting Pages export publish lock {} before open",
                    lock_path.display()
                )
            });
        }
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    if lock_exists {
        options.create(false);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    let lock_file = options.open(&lock_path).with_context(|| {
        format!(
            "failed opening Pages export publish lock {}",
            lock_path.display()
        )
    })?;

    let lock_identity = crate::franken_sync::FileIdentity::from_file(&lock_file)
        .with_context(|| {
            format!(
                "failed identifying opened Pages export publish lock {}",
                lock_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for Pages export publish lock {}",
                lock_path.display()
            )
        })?;
    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            bail!(
                "another Pages export is already publishing to {}; lock contention at {}",
                final_path.display(),
                lock_path.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed acquiring Pages export publish lock {} for {}",
                    lock_path.display(),
                    final_path.display()
                )
            });
        }
    }

    let path_metadata = std::fs::symlink_metadata(&lock_path).with_context(|| {
        format!(
            "failed re-inspecting Pages export publish lock {} after acquisition",
            lock_path.display()
        )
    })?;
    if !path_metadata.file_type().is_file() {
        bail!(
            "Pages export publish lock {} changed to a non-regular entry during acquisition",
            lock_path.display()
        );
    }
    #[cfg(unix)]
    if path_metadata.nlink() != 1 {
        bail!(
            "Pages export publish lock {} has {} hard links after acquisition; exclusive pathname ownership is not provable",
            lock_path.display(),
            path_metadata.nlink()
        );
    }
    let path_probe = std::fs::File::open(&lock_path).with_context(|| {
        format!(
            "failed re-opening Pages export publish lock {} after acquisition",
            lock_path.display()
        )
    })?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)
        .with_context(|| {
            format!(
                "failed re-identifying Pages export publish lock {} after acquisition",
                lock_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable pathname identity for Pages export publish lock {}",
                lock_path.display()
            )
        })?;
    if path_identity != lock_identity {
        bail!(
            "Pages export publish lock {} changed identity during acquisition",
            lock_path.display()
        );
    }

    Ok(ExportPublishGuard {
        final_path: final_path.to_path_buf(),
        lock_path,
        lock_identity,
        _lock_file: lock_file,
    })
}

fn require_export_publish_guard(
    final_path: &Path,
    publish_guard: &ExportPublishGuard,
) -> Result<()> {
    if publish_guard.final_path != final_path {
        bail!(
            "Pages export publish guard for {} cannot authorize publication to {}",
            publish_guard.final_path.display(),
            final_path.display()
        );
    }
    let held_identity = crate::franken_sync::FileIdentity::from_file(&publish_guard._lock_file)
        .with_context(|| {
            format!(
                "failed re-identifying held Pages export publish lock {}",
                publish_guard.lock_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for held Pages export publish lock {}",
                publish_guard.lock_path.display()
            )
        })?;
    if held_identity != publish_guard.lock_identity {
        bail!(
            "held Pages export publish lock {} changed identity",
            publish_guard.lock_path.display()
        );
    }
    let path_metadata = std::fs::symlink_metadata(&publish_guard.lock_path).with_context(|| {
        format!(
            "failed re-inspecting Pages export publish lock {} before authorization",
            publish_guard.lock_path.display()
        )
    })?;
    if !path_metadata.file_type().is_file() {
        bail!(
            "Pages export publish lock {} is no longer a regular file; refused namespace mutation",
            publish_guard.lock_path.display()
        );
    }
    #[cfg(unix)]
    if path_metadata.nlink() != 1 {
        bail!(
            "Pages export publish lock {} has {} hard links before authorization; refused namespace mutation",
            publish_guard.lock_path.display(),
            path_metadata.nlink()
        );
    }
    let path_probe = std::fs::File::open(&publish_guard.lock_path).with_context(|| {
        format!(
            "failed re-opening Pages export publish lock {} before authorization",
            publish_guard.lock_path.display()
        )
    })?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)
        .with_context(|| {
            format!(
                "failed re-identifying Pages export publish lock {} before authorization",
                publish_guard.lock_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable pathname identity for Pages export publish lock {}",
                publish_guard.lock_path.display()
            )
        })?;
    if path_identity != publish_guard.lock_identity {
        bail!(
            "Pages export publish lock {} was replaced after acquisition; refused namespace mutation",
            publish_guard.lock_path.display()
        );
    }
    Ok(())
}

fn inspect_export_regular_file(path: &Path, label: &str) -> Result<ExportFileEvidence> {
    let path_metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed inspecting {label} {}", path.display()))?;
    if !path_metadata.file_type().is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed opening {label} {}", path.display()))?;
    let identity = crate::franken_sync::FileIdentity::from_file(&file)
        .with_context(|| format!("failed identifying {label} {}", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for {label} {}",
                path.display()
            )
        })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed inspecting opened {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("opened {label} is not a regular file: {}", path.display());
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed hashing {label} {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let hashed_metadata = file
        .metadata()
        .with_context(|| format!("failed re-inspecting opened {label} {}", path.display()))?;
    if hashed_metadata.len() != metadata.len() {
        bail!("{label} changed size while inspected: {}", path.display());
    }

    let final_path_metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed re-inspecting {label} {}", path.display()))?;
    if !final_path_metadata.file_type().is_file() {
        bail!(
            "{label} changed file type while inspected: {}",
            path.display()
        );
    }
    let path_probe = std::fs::File::open(path)
        .with_context(|| format!("failed re-opening {label} {}", path.display()))?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)
        .with_context(|| format!("failed re-identifying {label} {}", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for {label} {}",
                path.display()
            )
        })?;
    if path_identity != identity {
        bail!(
            "{label} changed identity while inspected: {}",
            path.display()
        );
    }

    Ok(ExportFileEvidence {
        identity,
        size_bytes: metadata.len(),
        sha256: hex::encode(hasher.finalize()),
    })
}

fn evidence_matches(
    actual: &ExportFileEvidence,
    expected_size_bytes: u64,
    expected_sha256: &str,
) -> bool {
    actual.size_bytes == expected_size_bytes && actual.sha256 == expected_sha256
}

fn sync_export_regular_file(path: &Path, expected: &ExportFileEvidence, label: &str) -> Result<()> {
    let before_sync = inspect_export_regular_file(path, label)?;
    if before_sync.identity != expected.identity
        || !evidence_matches(&before_sync, expected.size_bytes, &expected.sha256)
    {
        bail!(
            "{label} changed identity or content before sync: {}",
            path.display()
        );
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed opening {label} {} for sync", path.display()))?;
    let identity = crate::franken_sync::FileIdentity::from_file(&file)
        .with_context(|| format!("failed identifying {label} {} before sync", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for {label} {} before sync",
                path.display()
            )
        })?;
    if identity != expected.identity {
        bail!("{label} changed identity before sync: {}", path.display());
    }
    file.sync_all()
        .with_context(|| format!("failed syncing {label} {}", path.display()))?;
    let path_probe = std::fs::File::open(path)
        .with_context(|| format!("failed re-opening {label} {} after sync", path.display()))?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)
        .with_context(|| {
            format!(
                "failed re-identifying {label} {} after sync",
                path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for {label} {} after sync",
                path.display()
            )
        })?;
    if path_identity != expected.identity {
        bail!("{label} changed identity during sync: {}", path.display());
    }
    let after_sync = inspect_export_regular_file(path, label)?;
    if after_sync.identity != expected.identity
        || !evidence_matches(&after_sync, expected.size_bytes, &expected.sha256)
    {
        bail!(
            "{label} changed identity or content during sync: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn export_publish_recovery_journal_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("pages_export.db"));
    let mut journal_name = std::ffi::OsString::from(".");
    journal_name.push(file_name);
    journal_name.push(".pages-export-publish-in-progress.json");
    path.with_file_name(journal_name)
}

#[cfg(any(windows, test))]
fn legacy_export_publish_recovery_backup_path(path: &Path) -> PathBuf {
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
    Ok(path.with_file_name(format!(".{file_name}.{suffix}.{}", hex::encode(nonce))))
}

#[cfg(any(windows, test))]
fn unpredictable_export_publish_backup_path(path: &Path) -> Result<PathBuf> {
    unpredictable_atomic_sidecar_path(path, "publish-backup", "pages_export.db")
}

#[cfg(any(windows, test))]
fn export_publish_backup_prefix(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pages_export.db");
    format!(".{file_name}.publish-backup.")
}

#[cfg(any(windows, test))]
fn journal_backup_path(final_path: &Path, journal: &ExportPublishJournal) -> Result<PathBuf> {
    let backup_name_path = Path::new(&journal.backup_file_name);
    if backup_name_path.file_name() != Some(backup_name_path.as_os_str())
        || backup_name_path.components().count() != 1
    {
        bail!(
            "Pages export publish journal for {} contains a non-local backup name",
            final_path.display()
        );
    }
    let prefix = export_publish_backup_prefix(final_path);
    let Some(nonce) = journal.backup_file_name.strip_prefix(&prefix) else {
        bail!(
            "Pages export publish journal for {} contains an unexpected backup name {}",
            final_path.display(),
            journal.backup_file_name
        );
    };
    if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "Pages export publish journal for {} contains an invalid backup nonce",
            final_path.display()
        );
    }
    Ok(final_path.with_file_name(&journal.backup_file_name))
}

#[cfg(any(windows, test))]
fn validate_export_publish_journal(
    final_path: &Path,
    journal: &ExportPublishJournal,
) -> Result<PathBuf> {
    if journal.format != EXPORT_PUBLISH_JOURNAL_FORMAT {
        bail!(
            "Pages export publish journal for {} has unsupported format {}",
            final_path.display(),
            journal.format
        );
    }
    for (label, digest) in [
        ("prior", journal.prior_sha256.as_str()),
        ("candidate", journal.candidate_sha256.as_str()),
    ] {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!(
                "Pages export publish journal for {} has an invalid {label} SHA-256 digest",
                final_path.display()
            );
        }
    }
    journal_backup_path(final_path, journal)
}

#[cfg(any(windows, test))]
fn read_export_publish_journal(final_path: &Path) -> Result<Option<ExportPublishJournal>> {
    let journal_path = export_publish_recovery_journal_path(final_path);
    let metadata = match std::fs::symlink_metadata(&journal_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed inspecting Pages export publish journal {}",
                    journal_path.display()
                )
            });
        }
    };
    if !metadata.file_type().is_file() {
        bail!(
            "Pages export publish journal is not a regular file; preserved it without mutation: {}",
            journal_path.display()
        );
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        bail!(
            "Pages export publish journal {} has {} hard links; preserved it because exclusive pathname ownership is not provable",
            journal_path.display(),
            metadata.nlink()
        );
    }

    let file = std::fs::File::open(&journal_path).with_context(|| {
        format!(
            "failed opening Pages export publish journal {}",
            journal_path.display()
        )
    })?;
    let opened_identity = crate::franken_sync::FileIdentity::from_file(&file)
        .with_context(|| {
            format!(
                "failed identifying opened Pages export publish journal {}",
                journal_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for Pages export publish journal {}",
                journal_path.display()
            )
        })?;
    let opened_metadata = file.metadata().with_context(|| {
        format!(
            "failed inspecting opened Pages export publish journal {}",
            journal_path.display()
        )
    })?;
    if !opened_metadata.file_type().is_file() {
        bail!(
            "opened Pages export publish journal is not a regular file; preserved it without mutation: {}",
            journal_path.display()
        );
    }
    if opened_metadata.len() > EXPORT_PUBLISH_JOURNAL_MAX_BYTES {
        bail!(
            "Pages export publish journal exceeds the {}-byte bound; preserved it without mutation: {}",
            EXPORT_PUBLISH_JOURNAL_MAX_BYTES,
            journal_path.display()
        );
    }

    let mut bytes = Vec::new();
    file.take(EXPORT_PUBLISH_JOURNAL_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| {
            format!(
                "failed reading Pages export publish journal {}",
                journal_path.display()
            )
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > EXPORT_PUBLISH_JOURNAL_MAX_BYTES {
        bail!(
            "Pages export publish journal exceeds the {}-byte bound; preserved it without mutation: {}",
            EXPORT_PUBLISH_JOURNAL_MAX_BYTES,
            journal_path.display()
        );
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != opened_metadata.len() {
        bail!(
            "Pages export publish journal changed size while read; preserved it without mutation: {}",
            journal_path.display()
        );
    }
    let final_metadata = std::fs::symlink_metadata(&journal_path).with_context(|| {
        format!(
            "failed re-inspecting Pages export publish journal {}",
            journal_path.display()
        )
    })?;
    if !final_metadata.file_type().is_file() {
        bail!(
            "Pages export publish journal changed file type while read; preserved the current entry without mutation: {}",
            journal_path.display()
        );
    }
    #[cfg(unix)]
    if final_metadata.nlink() != 1 {
        bail!(
            "Pages export publish journal {} gained additional hard links while read; preserved it without mutation",
            journal_path.display()
        );
    }
    let path_probe = std::fs::File::open(&journal_path).with_context(|| {
        format!(
            "failed re-opening Pages export publish journal {}",
            journal_path.display()
        )
    })?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)
        .with_context(|| {
            format!(
                "failed re-identifying Pages export publish journal {}",
                journal_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for Pages export publish journal {}",
                journal_path.display()
            )
        })?;
    if path_identity != opened_identity {
        bail!(
            "Pages export publish journal changed identity while read; preserved the current entry without mutation: {}",
            journal_path.display()
        );
    }
    let journal: ExportPublishJournal = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Pages export publish journal is invalid and was preserved without mutation: {}",
            journal_path.display()
        )
    })?;
    validate_export_publish_journal(final_path, &journal)?;
    Ok(Some(journal))
}

#[cfg(any(windows, test))]
fn write_export_publish_journal(
    final_path: &Path,
    backup_path: &Path,
    prior: &ExportFileEvidence,
    candidate: &ExportFileEvidence,
    publish_guard: &ExportPublishGuard,
) -> Result<ExportPublishJournal> {
    require_export_publish_guard(final_path, publish_guard)?;
    let backup_file_name = backup_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Pages export publish backup has no UTF-8 file name: {}",
                backup_path.display()
            )
        })?
        .to_string();
    let journal = ExportPublishJournal {
        format: EXPORT_PUBLISH_JOURNAL_FORMAT.to_string(),
        backup_file_name,
        prior_size_bytes: prior.size_bytes,
        prior_sha256: prior.sha256.clone(),
        candidate_size_bytes: candidate.size_bytes,
        candidate_sha256: candidate.sha256.clone(),
    };
    validate_export_publish_journal(final_path, &journal)?;
    let bytes =
        serde_json::to_vec(&journal).context("failed serializing Pages export publish journal")?;
    let journal_path = export_publish_recovery_journal_path(final_path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&journal_path).with_context(|| {
        format!(
            "failed exclusively creating Pages export publish journal {}; preserved the live and staged generations",
            journal_path.display()
        )
    })?;
    let created_identity = crate::franken_sync::FileIdentity::from_file(&file)
        .with_context(|| {
            format!(
                "failed identifying newly created Pages export publish journal {}",
                journal_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for newly created Pages export publish journal {}",
                journal_path.display()
            )
        })?;
    file.write_all(&bytes).with_context(|| {
        format!(
            "failed writing Pages export publish journal {}; preserved it for inspection",
            journal_path.display()
        )
    })?;
    file.sync_all().with_context(|| {
        format!(
            "failed syncing Pages export publish journal {}; preserved it for inspection",
            journal_path.display()
        )
    })?;
    sync_parent_directory(&journal_path).with_context(|| {
        format!(
            "failed durably publishing Pages export journal {}; preserved it for recovery",
            journal_path.display()
        )
    })?;
    let path_probe = std::fs::File::open(&journal_path).with_context(|| {
        format!(
            "failed re-opening newly created Pages export publish journal {}",
            journal_path.display()
        )
    })?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)
        .with_context(|| {
            format!(
                "failed re-identifying newly created Pages export publish journal {}",
                journal_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for newly created Pages export publish journal {}",
                journal_path.display()
            )
        })?;
    if path_identity != created_identity {
        bail!(
            "Pages export publish journal {} changed identity while it was created; live and staged generations were preserved",
            journal_path.display()
        );
    }
    let observed_journal = read_export_publish_journal(final_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "newly created Pages export publish journal disappeared before publication: {}",
            journal_path.display()
        )
    })?;
    if observed_journal != journal {
        bail!(
            "Pages export publish journal {} changed content before publication; live and staged generations were preserved",
            journal_path.display()
        );
    }
    require_export_publish_guard(final_path, publish_guard)?;
    Ok(journal)
}

#[cfg(any(windows, test))]
fn remove_export_publish_journal(
    final_path: &Path,
    expected_journal: &ExportPublishJournal,
    publish_guard: &ExportPublishGuard,
) -> Result<()> {
    require_export_publish_guard(final_path, publish_guard)?;
    let journal_path = export_publish_recovery_journal_path(final_path);
    let current_journal = read_export_publish_journal(final_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "completed Pages export publish journal disappeared before validated cleanup: {}",
            journal_path.display()
        )
    })?;
    if current_journal != *expected_journal {
        bail!(
            "Pages export publish journal changed before cleanup; preserved the current entry without mutation: {}",
            journal_path.display()
        );
    }
    require_export_publish_guard(final_path, publish_guard)?;
    match std::fs::remove_file(&journal_path) {
        Ok(()) => sync_parent_directory(&journal_path).with_context(|| {
            format!(
                "removed completed Pages export publish journal {}, but could not durably sync its parent directory",
                journal_path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed removing completed Pages export publish journal {}",
                journal_path.display()
            )
        }),
    }
}

#[cfg(any(windows, test))]
fn first_unmarked_export_publish_backup(
    final_path: &Path,
    owned_backup_path: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let legacy_path = legacy_export_publish_recovery_backup_path(final_path);
    match std::fs::symlink_metadata(&legacy_path) {
        Ok(_) => return Ok(Some(legacy_path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed inspecting legacy Pages export recovery path {}",
                    legacy_path.display()
                )
            });
        }
    }

    let parent = final_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = export_publish_backup_prefix(final_path);
    let entries = std::fs::read_dir(parent).with_context(|| {
        format!(
            "failed scanning Pages export directory {} for unmarked recovery backups",
            parent.display()
        )
    })?;
    for (index, entry) in entries.enumerate() {
        if index >= EXPORT_PUBLISH_BACKUP_SCAN_LIMIT {
            bail!(
                "Pages export directory {} exceeds the {}-entry recovery scan bound",
                parent.display(),
                EXPORT_PUBLISH_BACKUP_SCAN_LIMIT
            );
        }
        let entry = entry.with_context(|| {
            format!(
                "failed reading Pages export directory entry in {}",
                parent.display()
            )
        })?;
        let entry_path = entry.path();
        if entry.file_name().to_string_lossy().starts_with(&prefix)
            && owned_backup_path != Some(entry_path.as_path())
        {
            return Ok(Some(entry_path));
        }
    }
    Ok(None)
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

/// gz8xg: fsqlite stamps two zero-content namespace identity records
/// (`-fsqlite-ns-gate` / `-fsqlite-ns-use`) beside every `VACUUM INTO`
/// target. Once the staged candidate has been renamed onto the destination
/// its main file is gone, so those two records would otherwise remain as
/// unpredictable-named litter in the output directory. Remove exactly that
/// pair, tolerating absence (older engines may not stamp them).
///
/// Runs only after the export is live and durably synced, so a failure here
/// is logged, never surfaced: cosmetic litter must not turn a successful
/// publish into an error.
fn cleanup_candidate_namespace_identity_records(temp_path: &Path) {
    if let Err(err) = cleanup_sqlite_sidecars(vec![
        super::sqlite_sidecar_path(temp_path, "-fsqlite-ns-gate"),
        super::sqlite_sidecar_path(temp_path, "-fsqlite-ns-use"),
    ]) {
        tracing::warn!(
            error = %err,
            candidate = %temp_path.display(),
            "published Pages export is live, but its staged candidate's fsqlite namespace identity records could not be removed"
        );
    }
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
        bail!(
            "staged Pages export is not a regular file: {}",
            path.display()
        );
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed opening staged export {} for chmod", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed inspecting staged export {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "staged Pages export is not a regular file: {}",
            path.display()
        );
    }
    if (path_metadata.dev(), path_metadata.ino()) != (metadata.dev(), metadata.ino()) {
        bail!(
            "staged Pages export {} changed identity before permission enforcement",
            path.display()
        );
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| {
            format!(
                "failed setting staged export {} to mode 0600",
                path.display()
            )
        })?;
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
    let evidence = inspect_export_regular_file(path, "staged Pages export")?;
    sync_export_regular_file(path, &evidence, "staged Pages export")
}

/// Refuse to publish one SQLite main file over an existing artifact family.
///
/// A WAL, shared-memory file, or rollback journal beside `final_path` may
/// contain state belonging to the prior main database generation. Replacing
/// only the main file while retaining any of those sidecars can therefore make
/// readers observe a mixed or corrupt generation. The exporter cannot safely
/// decide that an existing sidecar is stale, so preserve it and fail closed.
fn reject_existing_sqlite_sidecars(path: &Path, artifact_label: &str) -> Result<()> {
    // Namespace identity records are exempt: FrankenSQLite stamps them next
    // to every database it touches (VACUUM INTO targets included) and they
    // carry no database content, so they cannot make a main-file-only
    // artifact incomplete.
    for sidecar in super::sqlite_content_bearing_artifact_paths(path)? {
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

/// The backup fallback can recover only an existing regular database file.
/// Reject links and special filesystem entries before moving anything so a
/// crash can never strand an entry that the recovery path must not trust.
#[cfg(any(windows, test))]
fn reject_non_regular_existing_publish_destination(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            if metadata.nlink() != 1 {
                bail!(
                    "existing Pages export destination {} has {} hard links; refused recoverable replacement because exclusive pathname ownership is not provable",
                    path.display(),
                    metadata.nlink()
                );
            }
            Ok(())
        }
        Ok(_) => {
            bail!(
                "existing Pages export destination {} is not a regular file; refused backup replacement without mutation",
                path.display()
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed inspecting existing Pages export destination {}",
                path.display()
            )
        }),
    }
}

/// Recover only states authenticated by a file-synced journal. Its namespace
/// is also directory-synced where the platform exposes that operation. An
/// unmarked file at any reserved backup spelling is never treated as
/// cass-owned: it is preserved and reported for explicit operator resolution.
#[cfg(any(windows, test))]
fn recover_or_refuse_interrupted_export_publish(
    final_path: &Path,
    publish_guard: &ExportPublishGuard,
) -> Result<()> {
    require_export_publish_guard(final_path, publish_guard)?;
    let Some(journal) = read_export_publish_journal(final_path)? else {
        if let Some(unmarked_backup) = first_unmarked_export_publish_backup(final_path, None)? {
            bail!(
                "unmarked Pages export recovery artifact {} was preserved; refusing to infer ownership or modify live path {}",
                unmarked_backup.display(),
                final_path.display()
            );
        }
        return Ok(());
    };
    let backup_path = validate_export_publish_journal(final_path, &journal)?;
    if let Some(unmarked_backup) =
        first_unmarked_export_publish_backup(final_path, Some(&backup_path))?
    {
        bail!(
            "Pages export publish journal identifies {}, but additional unmarked recovery artifact {} also exists; preserved every generation",
            backup_path.display(),
            unmarked_backup.display()
        );
    }

    let backup_evidence = if replacement_path_entry_exists(&backup_path)? {
        reject_existing_sqlite_sidecars(&backup_path, "publish-recovery backup")?;
        let evidence = inspect_export_regular_file(&backup_path, "publish-recovery backup")?;
        if !evidence_matches(&evidence, journal.prior_size_bytes, &journal.prior_sha256) {
            bail!(
                "Pages export publish-recovery backup {} does not match the journaled prior generation; preserved it without mutation",
                backup_path.display()
            );
        }
        Some(evidence)
    } else {
        None
    };
    reject_existing_sqlite_sidecars(final_path, "interrupted publish destination")?;
    let final_evidence = if replacement_path_entry_exists(final_path)? {
        reject_non_regular_existing_publish_destination(final_path)?;
        Some(inspect_export_regular_file(
            final_path,
            "interrupted publish destination",
        )?)
    } else {
        None
    };

    match (final_evidence, backup_evidence) {
        (None, Some(prior)) => {
            if replacement_path_entry_exists(final_path)? {
                bail!(
                    "journaled Pages export backup {} was ready to restore, but live path {} was concurrently repopulated; preserved every entry and the journal",
                    backup_path.display(),
                    final_path.display()
                );
            }
            require_export_publish_guard(final_path, publish_guard)?;
            std::fs::rename(&backup_path, final_path).with_context(|| {
                format!(
                    "failed restoring journaled Pages export backup {} to missing live path {}; preserved the backup and journal",
                    backup_path.display(),
                    final_path.display()
                )
            })?;
            let restored = inspect_export_regular_file(final_path, "restored Pages export")?;
            if restored.identity != prior.identity
                || !evidence_matches(&restored, journal.prior_size_bytes, &journal.prior_sha256)
            {
                bail!(
                    "restored Pages export {} does not retain the journaled prior-generation identity; preserved the journal",
                    final_path.display()
                );
            }
            sync_export_regular_file(final_path, &restored, "restored Pages export")?;
            sync_parent_directory(final_path).with_context(|| {
                format!(
                    "the prior Pages export was restored at {}, but the recovery rename could not be durably synced; journal retained at {}",
                    final_path.display(),
                    export_publish_recovery_journal_path(final_path).display()
                )
            })?;
            #[cfg(windows)]
            bail!(
                "restored the prior Pages export at {}, but Windows std cannot prove durable deletion of the recovery namespace; journal retained at {} for explicit resolution",
                final_path.display(),
                export_publish_recovery_journal_path(final_path).display()
            );
            #[cfg(not(windows))]
            remove_export_publish_journal(final_path, &journal, publish_guard)
        }
        (Some(live), Some(_prior)) => {
            if !evidence_matches(
                &live,
                journal.candidate_size_bytes,
                &journal.candidate_sha256,
            ) {
                bail!(
                    "journaled Pages export recovery found live {} and prior {}; live bytes do not match the journaled candidate, so both were preserved",
                    final_path.display(),
                    backup_path.display()
                );
            }
            #[cfg(windows)]
            bail!(
                "new Pages export is live at {} and journaled prior generation remains at {}; Windows std cannot prove durable namespace cleanup, so backup and journal were preserved",
                final_path.display(),
                backup_path.display()
            );
            #[cfg(not(windows))]
            remove_prior_export_backup_after_publish(
                &backup_path,
                final_path,
                &journal,
                Some(_prior.identity),
                publish_guard,
            )
        }
        (Some(live), None) => {
            let live_is_prior =
                evidence_matches(&live, journal.prior_size_bytes, &journal.prior_sha256);
            let live_is_candidate = evidence_matches(
                &live,
                journal.candidate_size_bytes,
                &journal.candidate_sha256,
            );
            if !live_is_prior && !live_is_candidate {
                bail!(
                    "Pages export publish journal remains at {}, but live file {} matches neither journaled generation; preserved both",
                    export_publish_recovery_journal_path(final_path).display(),
                    final_path.display()
                );
            }
            #[cfg(windows)]
            bail!(
                "Pages export {} matches a journaled generation, but its backup namespace is absent and Windows std cannot prove the completed transition; journal retained at {}",
                final_path.display(),
                export_publish_recovery_journal_path(final_path).display()
            );
            #[cfg(not(windows))]
            remove_export_publish_journal(final_path, &journal, publish_guard)
        }
        (None, None) => bail!(
            "Pages export publish journal remains at {}, but both live {} and journaled backup {} are missing; preserved the journal",
            export_publish_recovery_journal_path(final_path).display(),
            final_path.display(),
            backup_path.display()
        ),
    }
}

#[cfg(any(windows, test))]
fn replace_file_from_temp_via_backup(
    temp_path: &Path,
    final_path: &Path,
    retain_temp_on_error: &mut bool,
    publish_guard: &ExportPublishGuard,
) -> Result<()> {
    *retain_temp_on_error = false;
    require_export_publish_guard(final_path, publish_guard)?;
    recover_or_refuse_interrupted_export_publish(final_path, publish_guard)?;
    reject_non_regular_existing_publish_destination(final_path)?;
    reject_existing_sqlite_sidecars(final_path, "destination before backup replacement")?;

    let prior = inspect_export_regular_file(final_path, "prior Pages export")?;
    let candidate = inspect_export_regular_file(temp_path, "staged Pages export")?;
    sync_export_regular_file(temp_path, &candidate, "staged Pages export")?;
    let backup_path = unpredictable_export_publish_backup_path(final_path)?;
    if replacement_path_entry_exists(&backup_path)? {
        bail!(
            "unpredictable Pages export backup path unexpectedly exists; preserved every generation: {}",
            backup_path.display()
        );
    }
    let journal =
        write_export_publish_journal(final_path, &backup_path, &prior, &candidate, publish_guard)?;
    if replacement_path_entry_exists(&backup_path)? {
        bail!(
            "journaled Pages export backup path {} appeared before the prior generation could be parked; preserved it, the live generation, staged candidate, and journal without mutation",
            backup_path.display()
        );
    }
    let live_probe = inspect_export_regular_file(final_path, "prior Pages export")?;
    if live_probe.identity != prior.identity
        || !evidence_matches(&live_probe, journal.prior_size_bytes, &journal.prior_sha256)
    {
        bail!(
            "prior Pages export {} changed identity or content after its publish journal was created; preserved the live and staged generations and journal without mutation",
            final_path.display()
        );
    }

    require_export_publish_guard(final_path, publish_guard)?;
    std::fs::rename(final_path, &backup_path).with_context(|| {
        format!(
            "failed parking prior Pages export {} at journaled backup {}; staged candidate and journal were preserved",
            final_path.display(),
            backup_path.display()
        )
    })?;
    let parked = inspect_export_regular_file(&backup_path, "parked prior Pages export")?;
    if parked.identity != prior.identity
        || !evidence_matches(&parked, journal.prior_size_bytes, &journal.prior_sha256)
    {
        *retain_temp_on_error = true;
        bail!(
            "parked Pages export {} changed identity or content; preserved it, the staged candidate {}, and the publish journal",
            backup_path.display(),
            temp_path.display()
        );
    }
    sync_export_regular_file(&backup_path, &parked, "parked prior Pages export")?;
    sync_parent_directory(&backup_path).with_context(|| {
        format!(
            "prior Pages export was parked at {} and staged candidate remains at {}, but the backup rename could not be durably synced; journal preserved",
            backup_path.display(),
            temp_path.display()
        )
    })?;

    require_export_publish_guard(final_path, publish_guard)?;
    match std::fs::rename(temp_path, final_path) {
        Ok(()) => {
            let published =
                inspect_export_regular_file(final_path, "published Pages export candidate")?;
            if published.identity != candidate.identity
                || !evidence_matches(
                    &published,
                    journal.candidate_size_bytes,
                    &journal.candidate_sha256,
                )
            {
                *retain_temp_on_error = true;
                bail!(
                    "published Pages export {} changed identity or content; prior generation and journal retained at {}",
                    final_path.display(),
                    backup_path.display()
                );
            }
            sync_export_regular_file(final_path, &published, "published Pages export candidate")?;
            sync_parent_directory(final_path).with_context(|| {
                format!(
                    "new Pages export is live at {} and prior generation remains at {}, but publication could not be durably synced; journal preserved",
                    final_path.display(),
                    backup_path.display()
                )
            })?;
            cleanup_candidate_namespace_identity_records(temp_path);
            remove_prior_export_backup_after_publish(
                &backup_path,
                final_path,
                &journal,
                Some(prior.identity),
                publish_guard,
            )
        }
        Err(publish_error) => {
            let restore_probe =
                inspect_export_regular_file(&backup_path, "parked prior Pages export")?;
            if restore_probe.identity != prior.identity
                || !evidence_matches(
                    &restore_probe,
                    journal.prior_size_bytes,
                    &journal.prior_sha256,
                )
            {
                *retain_temp_on_error = true;
                bail!(
                    "failed publishing staged Pages export {} at {}: {}; parked prior generation {} no longer matches its journal, so every artifact was preserved",
                    temp_path.display(),
                    final_path.display(),
                    publish_error,
                    backup_path.display()
                );
            }
            if replacement_path_entry_exists(final_path)? {
                *retain_temp_on_error = true;
                bail!(
                    "failed publishing staged Pages export {} at {}: {}; the live path was concurrently repopulated, so staged candidate {}, journaled prior generation {}, and current live entry were preserved",
                    temp_path.display(),
                    final_path.display(),
                    publish_error,
                    temp_path.display(),
                    backup_path.display()
                );
            }
            require_export_publish_guard(final_path, publish_guard)?;
            match std::fs::rename(&backup_path, final_path) {
                Ok(()) => {
                    let restored =
                        inspect_export_regular_file(final_path, "restored prior Pages export")?;
                    if restored.identity != prior.identity
                        || !evidence_matches(
                            &restored,
                            journal.prior_size_bytes,
                            &journal.prior_sha256,
                        )
                    {
                        *retain_temp_on_error = true;
                        bail!(
                            "failed publishing staged Pages export and restored path {} changed identity or content; staged candidate and journal preserved",
                            final_path.display()
                        );
                    }
                    sync_export_regular_file(final_path, &restored, "restored prior Pages export")?;
                    sync_parent_directory(final_path).with_context(|| {
                        format!(
                            "restored prior Pages export at {}, but could not durably sync the rollback; journal preserved",
                            final_path.display()
                        )
                    })?;
                    #[cfg(windows)]
                    return Err(publish_error).context(format!(
                        "restored prior Pages export at {}, but Windows std cannot prove durable journal cleanup; journal retained at {}",
                        final_path.display(),
                        export_publish_recovery_journal_path(final_path).display()
                    ));
                    #[cfg(not(windows))]
                    {
                        remove_export_publish_journal(final_path, &journal, publish_guard)?;
                        Err(publish_error).with_context(|| {
                            format!(
                                "failed publishing staged Pages export {} at {}; restored the prior generation",
                                temp_path.display(),
                                final_path.display()
                            )
                        })
                    }
                }
                Err(restore_error) => {
                    *retain_temp_on_error = true;
                    bail!(
                        "failed publishing staged Pages export {} at {}: {}; restore error: {}; staged candidate retained at {}; prior generation retained at {}; journal retained at {}",
                        temp_path.display(),
                        final_path.display(),
                        publish_error,
                        restore_error,
                        temp_path.display(),
                        backup_path.display(),
                        export_publish_recovery_journal_path(final_path).display()
                    );
                }
            }
        }
    }
}

#[cfg(any(windows, test))]
fn remove_prior_export_backup_after_publish(
    backup_path: &Path,
    final_path: &Path,
    journal: &ExportPublishJournal,
    expected_identity: Option<crate::franken_sync::FileIdentity>,
    publish_guard: &ExportPublishGuard,
) -> Result<()> {
    require_export_publish_guard(final_path, publish_guard)?;
    #[cfg(windows)]
    {
        let _ = (journal, expected_identity);
        bail!(
            "new Pages export is live at {}, but Windows std cannot prove durable namespace cleanup; prior generation {} and journal {} were preserved",
            final_path.display(),
            backup_path.display(),
            export_publish_recovery_journal_path(final_path).display()
        );
    }

    #[cfg(not(windows))]
    {
        let journaled_backup_path = validate_export_publish_journal(final_path, journal)?;
        if journaled_backup_path != backup_path {
            bail!(
                "new Pages export is live at {}, but backup {} is not the path owned by its publish journal {}; preserved it without mutation",
                final_path.display(),
                backup_path.display(),
                export_publish_recovery_journal_path(final_path).display()
            );
        }
        let current_journal = read_export_publish_journal(final_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "new Pages export is live at {}, but its publish journal disappeared before backup cleanup; prior generation {} was preserved",
                final_path.display(),
                backup_path.display()
            )
        })?;
        if current_journal != *journal {
            bail!(
                "new Pages export is live at {}, but its publish journal changed before backup cleanup; prior generation {} and the current journal were preserved",
                final_path.display(),
                backup_path.display()
            );
        }
        let cleanup_probe =
            inspect_export_regular_file(backup_path, "prior Pages export cleanup backup")?;
        #[cfg(unix)]
        {
            let cleanup_metadata = std::fs::symlink_metadata(backup_path).with_context(|| {
                format!(
                    "failed re-inspecting prior Pages export cleanup backup {}",
                    backup_path.display()
                )
            })?;
            if cleanup_metadata.nlink() != 1 {
                bail!(
                    "new Pages export is live at {}, but prior-generation backup {} has {} hard links; backup and journal were preserved",
                    final_path.display(),
                    backup_path.display(),
                    cleanup_metadata.nlink()
                );
            }
        }
        if expected_identity
            .map(|identity| identity != cleanup_probe.identity)
            .unwrap_or(false)
            || !evidence_matches(
                &cleanup_probe,
                journal.prior_size_bytes,
                &journal.prior_sha256,
            )
        {
            bail!(
                "new Pages export is live at {}, but prior-generation backup {} changed identity or content; backup and journal were preserved",
                final_path.display(),
                backup_path.display()
            );
        }
        require_export_publish_guard(final_path, publish_guard)?;
        std::fs::remove_file(backup_path).with_context(|| {
            format!(
                "new Pages export is live at {}, but the validated prior sensitive generation remains at {}",
                final_path.display(),
                backup_path.display()
            )
        })?;
        sync_parent_directory(backup_path).with_context(|| {
            format!(
                "new Pages export is live at {} and prior backup {} was removed, but backup deletion could not be durably synced; journal preserved",
                final_path.display(),
                backup_path.display()
            )
        })?;
        remove_export_publish_journal(final_path, journal, publish_guard)
    }
}

fn replace_file_from_temp(
    temp_path: &Path,
    final_path: &Path,
    retain_temp_on_error: &mut bool,
    publish_guard: &ExportPublishGuard,
) -> Result<()> {
    *retain_temp_on_error = false;
    require_export_publish_guard(final_path, publish_guard)?;
    let candidate = inspect_export_regular_file(temp_path, "staged Pages export")?;
    sync_export_regular_file(temp_path, &candidate, "staged Pages export")?;
    #[cfg(windows)]
    recover_or_refuse_interrupted_export_publish(final_path, publish_guard)?;
    #[cfg(windows)]
    reject_non_regular_existing_publish_destination(final_path)?;
    reject_existing_sqlite_sidecars(final_path, "destination")?;
    #[cfg(windows)]
    {
        if replacement_path_entry_exists(final_path)? {
            return replace_file_from_temp_via_backup(
                temp_path,
                final_path,
                retain_temp_on_error,
                publish_guard,
            );
        }
        require_export_publish_guard(final_path, publish_guard)?;
        std::fs::rename(temp_path, final_path).with_context(|| {
            format!(
                "failed renaming completed export {} into place at {}",
                temp_path.display(),
                final_path.display()
            )
        })?;
        let published = inspect_export_regular_file(final_path, "published Pages export")?;
        if published.identity != candidate.identity
            || !evidence_matches(&published, candidate.size_bytes, &candidate.sha256)
        {
            *retain_temp_on_error = true;
            bail!(
                "published Pages export {} changed identity or content after its staged rename",
                final_path.display()
            );
        }
        sync_export_regular_file(final_path, &published, "published Pages export")?;
        sync_parent_directory(final_path).with_context(|| {
            format!(
                "new Pages export is live at {}, but its first publication could not be durably synced",
                final_path.display()
            )
        })?;
        cleanup_candidate_namespace_identity_records(temp_path);
        bail!(
            "new Pages export is live at {}, but Windows std cannot prove durable directory-entry publication; no prior generation existed to retain",
            final_path.display()
        )
    }

    #[cfg(not(windows))]
    {
        require_export_publish_guard(final_path, publish_guard)?;
        std::fs::rename(temp_path, final_path).with_context(|| {
            format!(
                "failed renaming completed export {} into place at {}",
                temp_path.display(),
                final_path.display()
            )
        })?;
        let published = inspect_export_regular_file(final_path, "published Pages export")?;
        if published.identity != candidate.identity
            || !evidence_matches(&published, candidate.size_bytes, &candidate.sha256)
        {
            *retain_temp_on_error = true;
            bail!(
                "published Pages export {} changed identity or content after its staged rename",
                final_path.display()
            );
        }
        sync_export_regular_file(final_path, &published, "published Pages export")?;
        sync_parent_directory(final_path).with_context(|| {
            format!(
                "new Pages export is live at {}, but its publication could not be durably synced",
                final_path.display()
            )
        })?;
        cleanup_candidate_namespace_identity_records(temp_path);
        Ok(())
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
// `std` exposes no safe directory-handle sync on Windows. Callers use this as
// a sequencing point, then return a partial-success error while preserving the
// journal/recoverable generation instead of claiming namespace durability.
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

    fn write_test_publish_journal(
        final_path: &Path,
        backup_path: &Path,
        candidate_path: &Path,
        publish_guard: &ExportPublishGuard,
    ) -> Result<()> {
        let prior = inspect_export_regular_file(backup_path, "test prior generation")?;
        let candidate = inspect_export_regular_file(candidate_path, "test candidate generation")?;
        write_export_publish_journal(final_path, backup_path, &prior, &candidate, publish_guard)?;
        Ok(())
    }

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

    #[cfg(unix)]
    #[test]
    fn backup_replacement_rejects_existing_destination_symlink() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let target_path = temp_dir.path().join("target.db");
        std::fs::write(&target_path, b"target generation")?;
        symlink(&target_path, &final_path)?;

        let error = reject_non_regular_existing_publish_destination(&final_path)
            .expect_err("backup replacement must not move an untrusted link");

        assert!(format!("{error:#}").contains("not a regular file"));
        assert!(
            std::fs::symlink_metadata(&final_path)?
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target_path)?, b"target generation");

        Ok(())
    }

    #[test]
    fn publish_recovery_uses_stable_journal_and_unpredictable_backups() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let journal_path = export_publish_recovery_journal_path(&final_path);
        let first = unpredictable_export_publish_backup_path(&final_path)?;
        let second = unpredictable_export_publish_backup_path(&final_path)?;

        assert_eq!(
            journal_path.file_name().and_then(|name| name.to_str()),
            Some(".export.db.pages-export-publish-in-progress.json")
        );
        assert_ne!(first, second, "backup nonce must not be reused");
        for backup_path in [first, second] {
            let name = backup_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("backup name is not UTF-8"))?;
            assert!(name.starts_with(".export.db.publish-backup."));
            assert_eq!(
                name.rsplit_once('.').map(|(_, nonce)| nonce.len()),
                Some(32)
            );
        }
        Ok(())
    }

    #[test]
    fn interrupted_publish_refuses_unmarked_deterministic_backup() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = legacy_export_publish_recovery_backup_path(&final_path);
        std::fs::write(&backup_path, b"unowned bytes")?;
        let guard = acquire_export_publish_guard(&final_path)?;

        let error = recover_or_refuse_interrupted_export_publish(&final_path, &guard)
            .expect_err("an unmarked deterministic backup must never be adopted");

        assert!(format!("{error:#}").contains("unmarked"));
        assert_eq!(std::fs::read(&backup_path)?, b"unowned bytes");
        assert!(std::fs::symlink_metadata(&final_path).is_err());
        Ok(())
    }

    #[test]
    fn interrupted_publish_refuses_unmarked_random_backup() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        std::fs::write(&backup_path, b"unowned random backup")?;
        let guard = acquire_export_publish_guard(&final_path)?;

        let error = recover_or_refuse_interrupted_export_publish(&final_path, &guard)
            .expect_err("an unmarked random backup must never be adopted");

        assert!(format!("{error:#}").contains("unmarked"));
        assert_eq!(std::fs::read(&backup_path)?, b"unowned random backup");
        assert!(std::fs::symlink_metadata(&final_path).is_err());
        Ok(())
    }

    #[test]
    fn interrupted_publish_restores_only_journaled_prior_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        let candidate_path = temp_dir.path().join("candidate-evidence.db");
        std::fs::write(&backup_path, b"prior generation")?;
        std::fs::write(&candidate_path, b"candidate generation")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        write_test_publish_journal(&final_path, &backup_path, &candidate_path, &guard)?;

        let result = recover_or_refuse_interrupted_export_publish(&final_path, &guard);
        #[cfg(not(windows))]
        result?;
        #[cfg(windows)]
        {
            let error = result.expect_err("Windows recovery must retain its journal");
            assert!(format!("{error:#}").contains("journal retained"));
        }

        assert_eq!(std::fs::read(&final_path)?, b"prior generation");
        assert!(std::fs::symlink_metadata(&backup_path).is_err());
        #[cfg(not(windows))]
        assert!(
            std::fs::symlink_metadata(export_publish_recovery_journal_path(&final_path)).is_err()
        );
        #[cfg(windows)]
        assert!(export_publish_recovery_journal_path(&final_path).is_file());
        Ok(())
    }

    #[test]
    fn interrupted_publish_finalizes_journal_written_before_backup_move() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        let candidate_path = temp_dir.path().join("candidate-evidence.db");
        std::fs::write(&backup_path, b"prior generation")?;
        std::fs::write(&candidate_path, b"candidate generation")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        write_test_publish_journal(&final_path, &backup_path, &candidate_path, &guard)?;
        std::fs::rename(&backup_path, &final_path)?;

        let result = recover_or_refuse_interrupted_export_publish(&final_path, &guard);
        #[cfg(not(windows))]
        result?;
        #[cfg(windows)]
        {
            let error = result.expect_err("Windows must retain the unresolved journal");
            assert!(format!("{error:#}").contains("journal retained"));
        }

        assert_eq!(std::fs::read(&final_path)?, b"prior generation");
        assert!(std::fs::symlink_metadata(&backup_path).is_err());
        #[cfg(not(windows))]
        assert!(
            std::fs::symlink_metadata(export_publish_recovery_journal_path(&final_path)).is_err()
        );
        #[cfg(windows)]
        assert!(export_publish_recovery_journal_path(&final_path).is_file());
        Ok(())
    }

    #[test]
    fn interrupted_publish_finalizes_journaled_completed_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        std::fs::write(&final_path, b"candidate generation")?;
        std::fs::write(&backup_path, b"prior generation")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        write_test_publish_journal(&final_path, &backup_path, &final_path, &guard)?;

        let result = recover_or_refuse_interrupted_export_publish(&final_path, &guard);
        #[cfg(not(windows))]
        result?;
        #[cfg(windows)]
        {
            let error = result.expect_err("Windows finalization must preserve backup and journal");
            assert!(format!("{error:#}").contains("backup and journal were preserved"));
        }

        assert_eq!(std::fs::read(&final_path)?, b"candidate generation");
        #[cfg(not(windows))]
        {
            assert!(std::fs::symlink_metadata(&backup_path).is_err());
            assert!(
                std::fs::symlink_metadata(export_publish_recovery_journal_path(&final_path))
                    .is_err()
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(std::fs::read(&backup_path)?, b"prior generation");
            assert!(export_publish_recovery_journal_path(&final_path).is_file());
        }
        Ok(())
    }

    #[test]
    fn interrupted_publish_preserves_journaled_backup_after_content_drift() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        let candidate_path = temp_dir.path().join("candidate-evidence.db");
        std::fs::write(&backup_path, b"prior generation")?;
        std::fs::write(&candidate_path, b"candidate generation")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        write_test_publish_journal(&final_path, &backup_path, &candidate_path, &guard)?;
        std::fs::write(&backup_path, b"changed generation")?;

        let error = recover_or_refuse_interrupted_export_publish(&final_path, &guard)
            .expect_err("drifted recovery bytes must never be restored or removed");

        assert!(format!("{error:#}").contains("does not match"));
        assert_eq!(std::fs::read(&backup_path)?, b"changed generation");
        assert!(std::fs::symlink_metadata(&final_path).is_err());
        assert!(export_publish_recovery_journal_path(&final_path).is_file());
        Ok(())
    }

    #[test]
    fn interrupted_publish_preserves_backup_when_destination_companion_survives() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        let candidate_path = temp_dir.path().join("candidate-evidence.db");
        let destination_sidecar = sqlite_content_artifact_paths(&final_path)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("SQLite artifact family unexpectedly empty"))?;
        std::fs::write(&backup_path, b"prior main")?;
        std::fs::write(&candidate_path, b"candidate main")?;
        std::fs::write(&destination_sidecar, b"unbound destination companion")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        write_test_publish_journal(&final_path, &backup_path, &candidate_path, &guard)?;

        let error = recover_or_refuse_interrupted_export_publish(&final_path, &guard)
            .expect_err("recovery must not mix a parked main with destination companions");

        assert!(format!("{error:#}").contains(&destination_sidecar.display().to_string()));
        assert_eq!(std::fs::read(&backup_path)?, b"prior main");
        assert_eq!(
            std::fs::read(&destination_sidecar)?,
            b"unbound destination companion"
        );
        assert!(std::fs::symlink_metadata(&final_path).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn publish_lock_rejects_symlink_before_opening_target() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let lock_path = export_publish_lock_path(&final_path);
        let target_path = temp_dir.path().join("unowned-lock-target");
        std::fs::write(&target_path, b"unowned lock bytes")?;
        symlink(&target_path, &lock_path)?;

        let error = acquire_export_publish_guard(&final_path)
            .expect_err("publish lock acquisition must reject a pre-existing symlink");

        assert!(format!("{error:#}").contains("refused to open"));
        assert!(
            std::fs::symlink_metadata(&lock_path)?
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target_path)?, b"unowned lock bytes");
        Ok(())
    }

    #[test]
    fn publish_lock_rejects_special_entry_before_opening_it() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let lock_path = export_publish_lock_path(&final_path);
        std::fs::create_dir(&lock_path)?;

        let error = acquire_export_publish_guard(&final_path)
            .expect_err("publish lock acquisition must reject a non-file entry");

        assert!(format!("{error:#}").contains("refused to open"));
        assert!(lock_path.is_dir(), "unowned lock entry must be preserved");
        Ok(())
    }

    #[test]
    fn publish_lock_serializes_same_destination() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let first_guard = acquire_export_publish_guard(&final_path)?;

        let error = acquire_export_publish_guard(&final_path)
            .expect_err("a second publisher must not acquire the same destination lock");

        assert!(format!("{error:#}").contains("already publishing"));
        assert_eq!(first_guard.final_path, final_path);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replaced_publish_lock_invalidates_existing_guard() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let lock_path = export_publish_lock_path(&final_path);
        let parked_lock_path = temp_dir.path().join("parked-publish.lock");
        let first_guard = acquire_export_publish_guard(&final_path)?;
        std::fs::rename(&lock_path, &parked_lock_path)?;
        std::fs::write(&lock_path, b"replacement lock")?;
        let second_guard = acquire_export_publish_guard(&final_path)?;

        let error = require_export_publish_guard(&final_path, &first_guard)
            .expect_err("a guard for a replaced lock pathname must not authorize mutation");

        assert!(format!("{error:#}").contains("was replaced"));
        require_export_publish_guard(&final_path, &second_guard)?;
        assert!(
            parked_lock_path.is_file(),
            "original lock must be preserved"
        );
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
            assert!(
                name.starts_with(".export.db.tmp."),
                "unexpected name: {name}"
            );
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
            std::fs::symlink_metadata(&staged_path)?
                .file_type()
                .is_symlink(),
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
        let marker_bytes =
            format!(r#"{{"last_upgrade_version":1,"last_run_at":{now},"repairs_applied":[]}}"#);
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
        builder.execute_compat("VACUUM INTO ?1;", params![candidate_path_text.as_ref()])?;
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

        std::fs::write(&final_path, b"old export")?;
        std::fs::write(&temp_path, b"new export")?;

        let guard = acquire_export_publish_guard(&final_path)?;
        let mut retain_temp_on_error = false;
        let result = replace_file_from_temp_via_backup(
            &temp_path,
            &final_path,
            &mut retain_temp_on_error,
            &guard,
        );
        #[cfg(not(windows))]
        result?;
        #[cfg(windows)]
        {
            let error = result.expect_err(
                "Windows must preserve the last recoverable generation when namespace durability is unprovable",
            );
            assert!(format!("{error:#}").contains("cannot prove durable namespace cleanup"));
        }

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
        let journal_path = export_publish_recovery_journal_path(&final_path);
        #[cfg(not(windows))]
        assert!(
            std::fs::symlink_metadata(&journal_path).is_err()
                && first_unmarked_export_publish_backup(&final_path, None)?.is_none(),
            "successful replacement left a recovery artifact behind"
        );
        #[cfg(windows)]
        {
            let journal = read_export_publish_journal(&final_path)?
                .ok_or_else(|| anyhow::anyhow!("Windows publish did not retain its journal"))?;
            let backup_path = journal_backup_path(&final_path, &journal)?;
            assert!(
                backup_path.is_file(),
                "Windows publish lost its prior generation"
            );
            assert!(journal_path.is_file(), "Windows publish lost its journal");
        }

        Ok(())
    }

    #[test]
    fn completed_backup_publish_reports_retained_sensitive_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        std::fs::write(&final_path, b"new live generation")?;
        std::fs::write(&backup_path, b"prior generation")?;
        let prior = inspect_export_regular_file(&backup_path, "test prior generation")?;
        let candidate = inspect_export_regular_file(&final_path, "test live generation")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        let journal =
            write_export_publish_journal(&final_path, &backup_path, &prior, &candidate, &guard)?;
        std::fs::write(&backup_path, b"changed prior generation")?;

        let error = remove_prior_export_backup_after_publish(
            &backup_path,
            &final_path,
            &journal,
            Some(prior.identity),
            &guard,
        )
        .expect_err("a changed prior generation must not be silently removed");

        let message = format!("{error:#}");
        assert!(
            message.contains("new Pages export is live"),
            "partial-success state was not reported: {message}"
        );
        assert!(
            message.contains(&backup_path.display().to_string()),
            "retained backup path was not reported: {message}"
        );
        assert_eq!(std::fs::read(&final_path)?, b"new live generation");
        assert_eq!(std::fs::read(&backup_path)?, b"changed prior generation");
        assert!(
            export_publish_recovery_journal_path(&final_path).is_file(),
            "journal for retained prior generation must be preserved"
        );
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn completed_publish_preserves_backup_when_journal_changes_before_cleanup() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("export.db");
        let backup_path = unpredictable_export_publish_backup_path(&final_path)?;
        std::fs::write(&final_path, b"new live generation")?;
        std::fs::write(&backup_path, b"prior generation")?;
        let prior = inspect_export_regular_file(&backup_path, "test prior generation")?;
        let candidate = inspect_export_regular_file(&final_path, "test live generation")?;
        let guard = acquire_export_publish_guard(&final_path)?;
        let journal =
            write_export_publish_journal(&final_path, &backup_path, &prior, &candidate, &guard)?;
        let mut changed_journal = serde_json::to_value(&journal)?;
        changed_journal["candidate_sha256"] = serde_json::Value::String("0".repeat(64));
        std::fs::write(
            export_publish_recovery_journal_path(&final_path),
            serde_json::to_vec(&changed_journal)?,
        )?;
        let error = remove_prior_export_backup_after_publish(
            &backup_path,
            &final_path,
            &journal,
            Some(prior.identity),
            &guard,
        )
        .expect_err("changed journal must block prior-generation cleanup");

        assert!(format!("{error:#}").contains("journal changed"));
        assert_eq!(std::fs::read(&backup_path)?, b"prior generation");
        assert!(export_publish_recovery_journal_path(&final_path).is_file());
        Ok(())
    }

    #[test]
    fn test_replace_file_from_temp_overwrites_existing_file() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let final_path = temp_dir.path().join("export.db");
        let first_tmp = temp_dir.path().join("first.tmp");
        let second_tmp = temp_dir.path().join("second.tmp");
        let mut retain_temp_on_error = false;
        let guard = acquire_export_publish_guard(&final_path).expect("acquire publish guard");

        std::fs::write(&first_tmp, b"first").expect("write first temp");
        let first_result =
            replace_file_from_temp(&first_tmp, &final_path, &mut retain_temp_on_error, &guard);
        #[cfg(not(windows))]
        first_result.expect("initial replace");
        #[cfg(windows)]
        {
            let error = first_result
                .expect_err("Windows first publication must report namespace durability limits");
            assert!(format!("{error:#}").contains("cannot prove durable directory-entry"));
        }
        assert_eq!(
            std::fs::read(&final_path).expect("read first final"),
            b"first"
        );

        std::fs::write(&second_tmp, b"second").expect("write second temp");
        let second_result =
            replace_file_from_temp(&second_tmp, &final_path, &mut retain_temp_on_error, &guard);
        #[cfg(not(windows))]
        second_result.expect("overwrite replace");
        #[cfg(windows)]
        {
            let error =
                second_result.expect_err("Windows replacement must preserve recovery state");
            assert!(format!("{error:#}").contains("cannot prove durable namespace cleanup"));
        }
        assert_eq!(
            std::fs::read(&final_path).expect("read second final"),
            b"second"
        );
        assert!(!retain_temp_on_error);
    }

    #[test]
    fn replacement_rejects_existing_sqlite_sidecars_without_mutating_artifacts() -> Result<()> {
        // Namespace identity records are exempt from the rejection policy;
        // see `sqlite_content_bearing_artifact_paths`.
        let artifact_paths = sqlite_fixed_artifact_paths(Path::new("export.db"))
            .into_iter()
            .filter(|path| !crate::pages::is_fsqlite_namespace_identity_record(path));
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

            let guard = acquire_export_publish_guard(&final_path)?;
            let mut retain_temp_on_error = false;
            let error = replace_file_from_temp(
                &staged_path,
                &final_path,
                &mut retain_temp_on_error,
                &guard,
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

        let guard = acquire_export_publish_guard(&final_path)?;
        let mut retain_temp_on_error = false;
        let error =
            replace_file_from_temp(&staged_path, &final_path, &mut retain_temp_on_error, &guard)
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
