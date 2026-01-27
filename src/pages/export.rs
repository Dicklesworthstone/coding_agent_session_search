use crate::ui::time_parser::parse_time_input;
use anyhow::{Context, Result, bail};
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
                tokenize='porter unicode61 remove_diacritics 2'
            )",
            [],
        )
        .context("Failed to create messages_fts table")?;

        tx.execute(
            r#"CREATE VIRTUAL TABLE messages_code_fts USING fts5(
                content,
                content='messages',
                content_rowid='id',
                tokenize="unicode61 tokenchars '-_./:@#$%\\'"
            )"#,
            [],
        )
        .context("Failed to create messages_code_fts table")?;

        // 4. Query Source
        let mut query = String::from(
            "SELECT c.id, a.slug as agent, w.path as workspace, c.title, c.source_path, c.started_at, c.ended_at, 
             (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) as message_count, 
             c.metadata_json 
             FROM conversations c
             JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(agents) = &self.filter.agents {
            if agents.is_empty() {
                query.push_str(" AND 1=0");
            } else {
                query.push_str(" AND a.slug IN (");
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
                query.push_str(" AND w.path IN (");
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
            query.push_str(" AND c.started_at >= ?");
            params.push(Box::new(since.timestamp_millis()));
        }

        if let Some(until) = self.filter.until {
            query.push_str(" AND c.started_at <= ?");
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
}
