use crate::ui::time_parser::parse_time_input;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        // 1. Open source DB
        let src = Connection::open_with_flags(
            &self.source_db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .context("Failed to open source database")?;
        src.busy_timeout(Duration::from_secs(5))?;

        // 2. Prepare output DB
        if self.output_path.exists() {
            std::fs::remove_file(&self.output_path)
                .context("Failed to remove existing output file")?;
        }
        let mut dest =
            Connection::open(&self.output_path).context("Failed to create output database")?;

        // Enable FTS5
        // Note: rusqlite bundled feature should handle this, but we need to ensure extension loading if not.
        // For now, assume compiled with fts5.

        let tx = dest.transaction()?;

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
            [],
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
                attachment_refs TEXT,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            )",
            [],
        )
        .context("Failed to create messages table")?;

        tx.execute(
            "CREATE TABLE export_meta (
                key TEXT PRIMARY KEY,
                value TEXT
            )",
            [],
        )
        .context("Failed to create export_meta table")?;

        tx.execute(
            "CREATE VIRTUAL TABLE messages_fts USING fts5(
                content,
                content='messages',
                content_rowid='id',
                tokenize='porter'
            )",
            [],
        )
        .context("Failed to create messages_fts table")?;

        tx.execute(
            r#"CREATE VIRTUAL TABLE messages_code_fts USING fts5(
                content,
                content='messages',
                content_rowid='id',
                tokenize="unicode61 tokenchars '_./'"
            )"#,
            [],
        )
        .context("Failed to create messages_code_fts table")?;

        // 4. Query Source
        let mut query = String::from(
            "SELECT id, agent, workspace, title, source_path, started_at, ended_at, message_count, metadata_json 
             FROM conversations WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(agents) = &self.filter.agents {
            if agents.is_empty() {
                query.push_str(" AND 1=0");
            } else {
                query.push_str(" AND agent IN (");
                for (i, agent) in agents.iter().enumerate() {
                    if i > 0 {
                        query.push_str(", ");
                    }
                    query.push('?');
                    params.push(Box::new(agent.clone()));
                }
                query.push(')');
            }
        }

        // Note: Workspace filtering in source DB might be string matching if paths aren't normalized consistently.
        // Assuming strict matching for now.
        if let Some(workspaces) = &self.filter.workspaces {
            if workspaces.is_empty() {
                query.push_str(" AND 1=0");
            } else {
                query.push_str(" AND workspace IN (");
                for (i, ws) in workspaces.iter().enumerate() {
                    if i > 0 {
                        query.push_str(", ");
                    }
                    query.push('?');
                    params.push(Box::new(ws.to_string_lossy().to_string()));
                }
                query.push(')');
            }
        }

        if let Some(since) = self.filter.since {
            query.push_str(" AND started_at >= ?");
            params.push(Box::new(since.timestamp_millis()));
        }

        if let Some(until) = self.filter.until {
            query.push_str(" AND started_at <= ?");
            params.push(Box::new(until.timestamp_millis()));
        }

        // Count total for progress
        let count_query = format!("SELECT COUNT(*) FROM ({})", query);
        let total_convs: usize = src.query_row(
            &count_query,
            rusqlite::params_from_iter(params.iter()),
            |row| row.get::<_, i64>(0).map(|v| v as usize),
        )?;

        // Execute Main Query
        let mut stmt = src.prepare(&query)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;

        let mut processed = 0;
        let mut msg_processed = 0;

        let mut msg_stmt = src.prepare(
            "SELECT role, content, created_at, idx 
             FROM messages 
             WHERE conversation_id = ? 
             ORDER BY idx ASC",
        )?;

        let mut insert_conv = tx.prepare(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, ended_at, message_count, metadata_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )?;

        let mut insert_msg = tx.prepare(
            "INSERT INTO messages (conversation_id, idx, role, content, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )?;

        let mut insert_fts =
            tx.prepare("INSERT INTO messages_fts (rowid, content) VALUES (?, ?)")?;

        let mut insert_code_fts =
            tx.prepare("INSERT INTO messages_code_fts (rowid, content) VALUES (?, ?)")?;

        while let Some(row) = rows.next()? {
            if let Some(r) = &running
                && !r.load(Ordering::Relaxed)
            {
                return Err(anyhow::anyhow!("Export cancelled"));
            }

            let id: i64 = row.get(0)?;
            let agent: String = row.get(1)?;
            let workspace: Option<String> = row.get(2)?;
            let title: Option<String> = row.get(3)?;
            let source_path: String = row.get(4)?;
            let started_at: Option<i64> = row.get(5)?;
            let ended_at: Option<i64> = row.get(6)?;
            let message_count: i64 = row.get(7)?;
            let metadata_json: Option<String> = row.get(8)?;

            // Transform Path
            let transformed_path = self.transform_path(&source_path, &workspace);

            insert_conv.execute(params![
                id,
                agent,
                workspace,
                title,
                transformed_path,
                started_at,
                ended_at,
                message_count,
                metadata_json
            ])?;

            // Fetch messages
            let mut msg_rows = msg_stmt.query(params![id])?;
            while let Some(msg_row) = msg_rows.next()? {
                let role: String = msg_row.get(0)?;
                let content: String = msg_row.get(1)?;
                let created_at: Option<i64> = msg_row.get(2)?;
                let idx: i64 = msg_row.get(3)?;

                let msg_id = insert_msg.insert(params![id, idx, role, content, created_at])?;

                // Populate FTS
                insert_fts.execute(params![msg_id, content])?;
                insert_code_fts.execute(params![msg_id, content])?;

                msg_processed += 1;
            }

            processed += 1;
            progress(processed, total_convs);
        }

        // Metadata
        tx.execute(
            "INSERT INTO export_meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        tx.execute(
            "INSERT INTO export_meta (key, value) VALUES ('exported_at', ?)",
            params![Utc::now().to_rfc3339()],
        )?;

        drop(insert_conv);
        drop(insert_msg);
        drop(insert_fts);
        drop(insert_code_fts);
        // drop(msg_stmt); // Removed: Let Rust handle drop order
        // drop(stmt);     // Removed: Let Rust handle drop order

        tx.commit()?;

        Ok(ExportStats {
            conversations_processed: processed,
            messages_processed: msg_processed,
        })
    }

    fn transform_path(&self, path: &str, workspace: &Option<String>) -> String {
        match self.filter.path_mode {
            PathMode::Relative => {
                if let Some(ws) = workspace
                    && let Some(stripped) = path.strip_prefix(ws)
                {
                    return stripped.trim_start_matches(['/', '\\']).to_string();
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
                format!("{:x}", hasher.finalize())[..16].to_string()
            }
        }
    }
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

    let db_path = db_path.unwrap_or_else(|| {
        directories::ProjectDirs::from("com", "dicklesworthstone", "coding-agent-search")
            .map(|dirs| dirs.data_dir().join("agent_search.db"))
            .expect("Could not determine data directory")
    });

    let since_dt = since
        .as_deref()
        .and_then(|s| parse_time_input(s).and_then(DateTime::from_timestamp_millis));
    let until_dt = until
        .as_deref()
        .and_then(|s| parse_time_input(s).and_then(DateTime::from_timestamp_millis));

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
