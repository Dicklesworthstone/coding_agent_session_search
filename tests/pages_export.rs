#[cfg(test)]
mod tests {
    use anyhow::{Result, anyhow};
    use coding_agent_search::pages::export::{
        ExportEngine, ExportFilter, PathMode, run_pages_export,
    };
    use frankensqlite::compat::{ConnectionExt, RowExt};
    use frankensqlite::{Connection as FrankenConnection, Row as FrankenRow, params as fparams};
    use rusqlite::Connection;
    use std::path::Path;
    use tempfile::TempDir;

    fn setup_source_db(path: &Path) -> Result<()> {
        let conn = Connection::open(path)?;

        conn.execute_batch(
            r#"
            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );

            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL
            );

            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                message_count INTEGER,
                metadata_json TEXT,
                FOREIGN KEY (agent_id) REFERENCES agents(id),
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id)
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER,
                updated_at INTEGER,
                model TEXT,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );
            "#,
        )?;

        // Agents + workspaces
        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'claude')", [])?;
        conn.execute("INSERT INTO agents (id, slug) VALUES (2, 'codex')", [])?;
        conn.execute(
            "INSERT INTO workspaces (id, path) VALUES (1, '/home/user/proj1')",
            [],
        )?;
        conn.execute(
            "INSERT INTO workspaces (id, path) VALUES (2, '/home/user/proj2')",
            [],
        )?;

        // Insert test data
        conn.execute(
            "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, message_count)
             VALUES (1, 1, 1, 'Test Conv 1', '/home/user/proj1/.claude/1.json', 1600000000000, 2)",
            [],
        )?;
        conn.execute(
            "INSERT INTO messages (conversation_id, idx, role, content, created_at)
             VALUES (1, 0, 'user', 'hello', 1600000000000)",
            [],
        )?;
        conn.execute(
            "INSERT INTO messages (conversation_id, idx, role, content, created_at)
             VALUES (1, 1, 'assistant', 'world', 1600000005000)",
            [],
        )?;

        conn.execute(
            "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, message_count)
             VALUES (2, 2, 2, 'Test Conv 2', '/home/user/proj2/.codex/session.json', 1700000000000, 1)",
            [],
        )?;
        conn.execute(
            "INSERT INTO messages (conversation_id, idx, role, content, created_at)
             VALUES (2, 0, 'user', 'rust code', 1700000000000)",
            [],
        )?;

        Ok(())
    }

    fn open_franken_db(path: &Path) -> Result<FrankenConnection> {
        let path_str = path.to_string_lossy();
        Ok(FrankenConnection::open(path_str.as_ref())?)
    }

    fn setup_franken_source_db(path: &Path) -> Result<()> {
        let conn = open_franken_db(path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );

            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL
            );

            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                message_count INTEGER,
                metadata_json TEXT
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER,
                updated_at INTEGER,
                model TEXT,
                attachment_refs TEXT
            );
            "#,
        )?;

        conn.execute_compat(
            "INSERT INTO agents (id, slug) VALUES (?1, ?2)",
            fparams![1_i64, "codex"],
        )?;
        conn.execute_compat(
            "INSERT INTO workspaces (id, path) VALUES (?1, ?2)",
            fparams![1_i64, "/home/user/franken"],
        )?;
        conn.execute_compat(
            "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, message_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            fparams![
                1_i64,
                1_i64,
                1_i64,
                "Frankensqlite Export",
                "/home/user/franken/.codex/session.jsonl",
                1_700_000_000_000_i64,
                2_i64
            ],
        )?;
        conn.execute_compat(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at, updated_at, model, attachment_refs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            fparams![
                10_i64,
                1_i64,
                0_i64,
                "user",
                "please verify frankensqlite pages export",
                1_700_000_000_000_i64,
                1_700_000_000_100_i64,
                "gpt-5",
                "[\"artifact-a\"]"
            ],
        )?;
        conn.execute_compat(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at, updated_at, model, attachment_refs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            fparams![
                11_i64,
                1_i64,
                1_i64,
                "assistant",
                "frankensqlite export payload is queryable",
                1_700_000_000_500_i64,
                1_700_000_000_600_i64,
                "gpt-5",
                "[\"artifact-b\"]"
            ],
        )?;

        Ok(())
    }

    #[test]
    fn test_export_engine_basic() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        assert_eq!(stats.conversations_processed, 2);
        assert_eq!(stats.messages_processed, 3);

        // Verify output DB
        let conn = Connection::open(&output_path)?;

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;
        assert_eq!(count, 2);

        let fts_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'messages_fts'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(fts_exists, 1);

        // Verify Path Transformation (Relative)
        let path: String = conn.query_row(
            "SELECT source_path FROM conversations WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(path, ".claude/1.json"); // Stripped workspace prefix

        Ok(())
    }

    #[test]
    fn test_export_filter_agent() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: Some(vec!["claude".to_string()]),
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        assert_eq!(stats.conversations_processed, 1);

        let conn = Connection::open(&output_path)?;
        let agent: String = conn.query_row("SELECT agent FROM conversations", [], |r| r.get(0))?;
        assert_eq!(agent, "claude");

        Ok(())
    }

    #[test]
    fn test_export_engine_frankensqlite_source_and_output_are_queryable() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source-franken.db");
        let output_path = temp_dir.path().join("export-franken.db");

        setup_franken_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: Some(vec!["codex".to_string()]),
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        assert_eq!(stats.conversations_processed, 1);
        assert_eq!(stats.messages_processed, 2);

        let output_conn = open_franken_db(&output_path)?;
        let exported_messages: i64 = output_conn.query_row_map(
            "SELECT COUNT(*) FROM messages",
            &[],
            |row: &FrankenRow| row.get_typed(0),
        )?;
        assert_eq!(exported_messages, 2);

        let assistant_content: String = output_conn.query_row_map(
            "SELECT content FROM messages WHERE role = 'assistant'",
            &[],
            |row: &FrankenRow| row.get_typed(0),
        )?;
        assert_eq!(
            assistant_content,
            "frankensqlite export payload is queryable"
        );

        let fts_hits: i64 = output_conn.query_row_map(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'frankensqlite'",
            &[],
            |row: &FrankenRow| row.get_typed(0),
        )?;
        assert_eq!(fts_hits, 2);

        Ok(())
    }

    #[test]
    fn test_export_path_mode_hash() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Hash,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(|_, _| {}, None)?;

        let conn = Connection::open(&output_path)?;
        let path: String = conn.query_row(
            "SELECT source_path FROM conversations WHERE id=1",
            [],
            |r| r.get(0),
        )?;

        assert_eq!(path.len(), 16); // 16 chars hex
        assert_ne!(path, "/home/user/proj1/.claude/1.json");

        Ok(())
    }

    #[test]
    fn test_export_filter_multiple_agents() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        // Filter for both agents
        let filter = ExportFilter {
            agents: Some(vec!["claude".to_string(), "codex".to_string()]),
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        // Should get both conversations
        assert_eq!(stats.conversations_processed, 2);
        assert_eq!(stats.messages_processed, 3);

        Ok(())
    }

    #[test]
    fn test_export_filter_time_range() -> Result<()> {
        use chrono::{TimeZone, Utc};

        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        // Filter for conversations after first one's start time
        // Conv 1: started_at = 1600000000000 (Sep 2020)
        // Conv 2: started_at = 1700000000000 (Nov 2023)
        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: Some(
                Utc.timestamp_millis_opt(1_650_000_000_000)
                    .single()
                    .ok_or_else(|| anyhow!("invalid fixed pages export timestamp"))?,
            ), // ~Apr 2022
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        // Should only get conv 2 (codex)
        assert_eq!(stats.conversations_processed, 1);

        let conn = Connection::open(&output_path)?;
        let agent: String = conn.query_row("SELECT agent FROM conversations", [], |r| r.get(0))?;
        assert_eq!(agent, "codex");

        Ok(())
    }

    #[test]
    fn test_export_preserves_message_identity_and_optional_metadata() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let source_conn = Connection::open(&source_path)?;
        source_conn.execute("ALTER TABLE messages ADD COLUMN attachment_refs TEXT", [])?;

        let source_message_id: i64 = source_conn.query_row(
            "SELECT id FROM messages WHERE conversation_id = 1 AND idx = 0",
            [],
            |row| row.get(0),
        )?;
        source_conn.execute(
            "UPDATE messages SET updated_at = ?, model = ?, attachment_refs = ? WHERE id = ?",
            rusqlite::params![
                1_600_000_123_000_i64,
                "claude-opus-4-6",
                "[\"blob-a\",\"blob-b\"]",
                source_message_id
            ],
        )?;
        drop(source_conn);

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(|_, _| {}, None)?;

        let output_conn = Connection::open(&output_path)?;
        let exported = output_conn.query_row(
            "SELECT id, updated_at, model, attachment_refs FROM messages WHERE conversation_id = 1 AND idx = 0",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?;

        assert_eq!(exported.0, source_message_id);
        assert_eq!(exported.1, Some(1_600_000_123_000_i64));
        assert_eq!(exported.2.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(exported.3.as_deref(), Some("[\"blob-a\",\"blob-b\"]"));

        Ok(())
    }

    #[test]
    fn test_export_derives_model_from_extra_json_when_column_missing() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        let conn = Connection::open(&source_path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE agents (id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
            CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT NOT NULL);
            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                message_count INTEGER,
                metadata_json TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER,
                extra_json TEXT
            );
            "#,
        )?;
        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'claude')", [])?;
        conn.execute(
            "INSERT INTO workspaces (id, path) VALUES (1, '/home/user/proj1')",
            [],
        )?;
        conn.execute(
            "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, message_count)
             VALUES (1, 1, 1, 'Extra JSON model', '/home/user/proj1/.claude/extra.jsonl', 1600000000000, 1)",
            [],
        )?;
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at, extra_json)
             VALUES (101, 1, 0, 'assistant', 'hello', 1600000000000, ?1)",
            rusqlite::params![
                r#"{"message":{"model":"claude-sonnet-4"},"attachments":["blob-z"]}"#
            ],
        )?;
        drop(conn);

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(|_, _| {}, None)?;

        let output_conn = Connection::open(&output_path)?;
        let exported = output_conn.query_row(
            "SELECT id, model, attachment_refs FROM messages WHERE conversation_id = 1 AND idx = 0",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )?;

        assert_eq!(exported.0, 101);
        assert_eq!(exported.1.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(exported.2.as_deref(), Some("[\"blob-z\"]"));

        Ok(())
    }

    #[test]
    fn test_export_filter_workspace() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: Some(vec![std::path::PathBuf::from("/home/user/proj1")]),
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        // Should only get conv 1 (claude in proj1)
        assert_eq!(stats.conversations_processed, 1);

        let conn = Connection::open(&output_path)?;
        let agent: String = conn.query_row("SELECT agent FROM conversations", [], |r| r.get(0))?;
        assert_eq!(agent, "claude");

        Ok(())
    }

    #[test]
    fn test_export_empty_result() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        // Filter for non-existent agent
        let filter = ExportFilter {
            agents: Some(vec!["nonexistent".to_string()]),
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        // Should get empty result
        assert_eq!(stats.conversations_processed, 0);
        assert_eq!(stats.messages_processed, 0);

        // Output DB should still exist with schema
        let conn = Connection::open(&output_path)?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;
        assert_eq!(count, 0);

        // Schema should exist (FTS table)
        let fts_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'messages_fts'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(fts_exists, 1);

        Ok(())
    }

    #[test]
    fn test_export_path_mode_basename() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Basename,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(|_, _| {}, None)?;

        let conn = Connection::open(&output_path)?;
        let path: String = conn.query_row(
            "SELECT source_path FROM conversations WHERE id=1",
            [],
            |r| r.get(0),
        )?;

        // Should be just the filename
        assert_eq!(path, "1.json");

        Ok(())
    }

    #[test]
    fn test_export_path_mode_full() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Full,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(|_, _| {}, None)?;

        let conn = Connection::open(&output_path)?;
        let path: String = conn.query_row(
            "SELECT source_path FROM conversations WHERE id=1",
            [],
            |r| r.get(0),
        )?;

        // Should be full path unchanged
        assert_eq!(path, "/home/user/proj1/.claude/1.json");

        Ok(())
    }

    #[test]
    fn test_export_progress_callback() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let callback_count = Arc::new(AtomicUsize::new(0));
        let callback_count_clone = callback_count.clone();

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(
            move |current, total| {
                callback_count_clone.fetch_add(1, Ordering::SeqCst);
                assert!(current <= total);
            },
            None,
        )?;

        // Should have been called for each conversation (2)
        assert_eq!(callback_count.load(Ordering::SeqCst), 2);

        Ok(())
    }

    #[test]
    fn test_export_engine_creates_missing_output_parent_directories() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("nested/site/export.db");

        setup_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        let stats = engine.execute(|_, _| {}, None)?;

        assert_eq!(stats.conversations_processed, 2);
        assert!(output_path.exists(), "export db should be created");

        let conn = Connection::open(&output_path)?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;
        assert_eq!(count, 2);

        Ok(())
    }

    #[test]
    fn test_run_pages_export_rejects_invalid_since() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let err = run_pages_export(
            Some(source_path),
            output_path,
            None,
            None,
            Some("not-a-time".to_string()),
            None,
            PathMode::Relative,
            false,
        )
        .expect_err("invalid --since should fail");

        assert!(err.to_string().contains("Invalid --since value"));
        Ok(())
    }

    #[test]
    fn test_run_pages_export_rejects_reversed_time_range() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_source_db(&source_path)?;

        let err = run_pages_export(
            Some(source_path),
            output_path,
            None,
            None,
            Some("2025-01-02".to_string()),
            Some("2025-01-01".to_string()),
            PathMode::Relative,
            false,
        )
        .expect_err("reversed time range should fail");

        assert!(err.to_string().contains("Invalid time range"));
        Ok(())
    }
}
