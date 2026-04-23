//! FTS5 Integration Tests for Pages Export
//!
//! Tests the dual FTS5 index strategy:
//! - messages_fts: Porter stemmer for natural language
//! - messages_code_fts: unicode61 with tokenchars for code identifiers

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
    use coding_agent_search::pages::fts::{
        Fts5SearchMode, detect_search_mode, escape_fts5_query, format_fts5_query,
        validate_fts5_query,
    };
    use frankensqlite::compat::{ConnectionExt, RowExt};
    use frankensqlite::{Connection as FrankenConnection, params as fparams};
    use std::path::Path;
    use tempfile::TempDir;

    fn open_franken_db(path: &Path) -> Result<FrankenConnection> {
        let path_str = path.to_string_lossy();
        Ok(FrankenConnection::open(path_str.as_ref())?)
    }

    /// Set up a source database with test data for FTS5 testing
    fn setup_fts_source_db(path: &Path) -> Result<()> {
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
        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'claude')")?;
        conn.execute("INSERT INTO workspaces (id, path) VALUES (1, '/home/user/project')")?;

        // Insert test conversations
        conn.execute(
            "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, message_count)
             VALUES (1, 1, 1, 'FTS Test', '/path/1.json', 1600000000000, 5)"
        )?;

        // Insert messages with various content types for FTS testing

        // Message 1: Natural language with stemming test ("running" should match "run")
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (1, 1, 0, 'user', 'I am running the tests and they keep running forever', 1600000000000)"
        )?;

        // Message 2: Code identifier with underscore (snake_case)
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (2, 1, 1, 'assistant', 'You should call my_function and get_user_by_id to fix the issue', 1600000001000)"
        )?;

        // Message 3: File path / filename
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (3, 1, 2, 'user', 'The error is in AuthController.ts at line 42', 1600000002000)"
        )?;

        // Message 4: More content for BM25 ranking tests
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (4, 1, 3, 'assistant', 'Error error error - this message has many errors', 1600000003000)"
        )?;

        // Message 5: Single mention for ranking comparison
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (5, 1, 4, 'user', 'I found one error in the code', 1600000004000)",
        )?;

        Ok(())
    }

    /// Create an exported database with FTS5 indexes
    fn create_export_db(temp_dir: &TempDir) -> Result<(FrankenConnection, std::path::PathBuf)> {
        let source_path = temp_dir.path().join("source.db");
        let output_path = temp_dir.path().join("export.db");

        setup_fts_source_db(&source_path)?;

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode: PathMode::Relative,
        };

        let engine = ExportEngine::new(&source_path, &output_path, filter);
        engine.execute(|_, _| {}, None)?;

        let conn = FrankenConnection::open(output_path.to_string_lossy().into_owned())?;
        Ok((conn, output_path))
    }

    fn create_runtime_fts_db(temp_dir: &TempDir) -> Result<FrankenConnection> {
        let db_path = temp_dir.path().join("runtime_fts.db");
        let conn = open_franken_db(&db_path)?;

        conn.execute_batch(
            r#"
            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent TEXT NOT NULL,
                workspace TEXT,
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
                attachment_refs TEXT,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );

            CREATE VIRTUAL TABLE messages_fts USING fts5(
                content,
                tokenize='porter unicode61 remove_diacritics 2'
            );

            CREATE VIRTUAL TABLE messages_code_fts USING fts5(
                content,
                tokenize="unicode61 tokenchars '-_./:@#$%\\'"
            );
            "#,
        )?;

        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (1, 'claude', '/home/user/project', 'FTS Test', '/path/1.json', 1600000000000, 5)"
        )?;
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (1, 1, 0, 'user', 'I am running the tests and they keep running forever', 1600000000000)"
        )?;
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (2, 1, 1, 'assistant', 'You should call my_function and get_user_by_id to fix the issue', 1600000001000)"
        )?;
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (3, 1, 2, 'user', 'The error is in AuthController.ts at line 42', 1600000002000)"
        )?;
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (4, 1, 3, 'assistant', 'Error error error - this message has many errors', 1600000003000)"
        )?;
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content, created_at)
             VALUES (5, 1, 4, 'user', 'I found one error in the code', 1600000004000)",
        )?;
        conn.execute("INSERT INTO messages_fts(rowid, content) SELECT id, content FROM messages")?;
        conn.execute(
            "INSERT INTO messages_code_fts(rowid, content) SELECT id, content FROM messages",
        )?;

        Ok(conn)
    }

    // ============================================
    // Porter Stemmer Tests (messages_fts)
    // ============================================

    #[test]
    fn test_fts5_porter_stemming_run_matches_running() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Search for "run" should match content with "running" due to porter stemmer
        let results: Vec<String> = conn.query_map_collect(
            r#"
                SELECT snippet(messages_fts, 0, '[', ']', '...', 20) as snippet
                FROM messages_fts
                WHERE messages_fts MATCH '"run"'
            "#,
            &[],
            |row| row.get_typed(0),
        )?;

        assert!(
            !results.is_empty(),
            "Porter stemmer should match 'run' to 'running'"
        );

        // Verify the match is from message 1 which contains "running"
        let found_running = results.iter().any(|s| s.to_lowercase().contains("running"));
        assert!(
            found_running,
            "Should have matched the message containing 'running'"
        );

        Ok(())
    }

    #[test]
    fn test_fts5_porter_stemming_bidirectional() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Search for "running" should also work
        let count: i64 = conn.query_row_map(
            r#"SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '"running"'"#,
            &[],
            |row| row.get_typed(0),
        )?;

        assert!(count > 0, "Should find messages containing 'running'");

        Ok(())
    }

    // ============================================
    // Code Tokenizer Tests (messages_code_fts)
    // ============================================

    #[test]
    fn test_fts5_code_underscore_token() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Search for snake_case identifier in code FTS
        let count: i64 = conn.query_row_map(
            r#"SELECT COUNT(*) FROM messages_code_fts WHERE messages_code_fts MATCH '"my_function"'"#,
            &[],
            |row| row.get_typed(0),
        )?;

        assert!(
            count > 0,
            "Code FTS should match 'my_function' as single token"
        );

        // Also test get_user_by_id
        let count2: i64 = conn.query_row_map(
            r#"SELECT COUNT(*) FROM messages_code_fts WHERE messages_code_fts MATCH '"get_user_by_id"'"#,
            &[],
            |row| row.get_typed(0),
        )?;

        assert!(
            count2 > 0,
            "Code FTS should match 'get_user_by_id' as single token"
        );

        Ok(())
    }

    #[test]
    fn test_fts5_code_filename_with_extension() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Search for filename with extension
        let count: i64 = conn.query_row_map(
            r#"SELECT COUNT(*) FROM messages_code_fts WHERE messages_code_fts MATCH '"AuthController.ts"'"#,
            &[],
            |row| row.get_typed(0),
        )?;

        assert!(
            count > 0,
            "Code FTS should match 'AuthController.ts' as single token"
        );

        Ok(())
    }

    // ============================================
    // Empty Query Tests
    // ============================================

    #[test]
    fn test_fts5_empty_query_returns_empty() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let _conn = create_runtime_fts_db(&temp_dir)?;

        // Empty escaped query should return no results (not an error)
        let escaped = escape_fts5_query("");
        assert!(
            escaped.is_empty(),
            "Empty query should produce empty escaped string"
        );

        // Validate should return None for empty
        assert!(validate_fts5_query("").is_none());
        assert!(validate_fts5_query("   ").is_none());

        Ok(())
    }

    // ============================================
    // Special Character Escaping Tests
    // ============================================

    #[test]
    fn test_fts5_escape_prevents_injection() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Attempt queries with special characters that could break FTS5
        let dangerous_queries = vec![
            r#"foo"bar"#, // Embedded quotes
            "foo*",       // Wildcard
            "foo+bar",    // OR operator
            "foo-bar",    // NOT operator
            "foo:bar",    // Column prefix
            "(foo)",      // Grouping
            "foo^2",      // Boost
            "foo~2",      // Fuzzy
        ];

        for query in dangerous_queries {
            let escaped = escape_fts5_query(query);

            // Escaped query should be safe to execute
            let result = conn.query_row_map(
                &format!(
                    r#"SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '{}'"#,
                    escaped
                ),
                &[],
                |row| row.get_typed::<i64>(0),
            );

            // Should not error, may return 0 results
            assert!(
                result.is_ok(),
                "Query with '{}' should not cause FTS5 error",
                query
            );
        }

        Ok(())
    }

    #[test]
    fn test_fts5_escape_quotes_doubled() {
        // Verify that internal quotes are properly doubled
        let escaped = escape_fts5_query(r#"say "hello""#);
        assert!(
            escaped.contains(r#""""#),
            "Internal quotes should be doubled"
        );
    }

    // ============================================
    // BM25 Ranking Tests
    // ============================================

    #[test]
    fn test_fts5_bm25_ranking_more_matches_higher() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Search for "error" - message 4 has many, message 5 has one
        let results: Vec<(i64, f64)> = conn.query_map_collect(
            r#"
                SELECT m.id, bm25(messages_fts) as score
                FROM messages_fts
                JOIN messages m ON messages_fts.rowid = m.id
                WHERE messages_fts MATCH '"error"'
                ORDER BY score
            "#,
            &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )?;

        assert!(
            results.len() >= 2,
            "Should find at least 2 messages with 'error'"
        );

        // BM25 returns negative scores (lower = better match)
        // Message 4 (many errors) should rank higher (more negative score)
        // Find message 4 and 5 in results
        let msg4_score = results.iter().find(|(id, _)| *id == 4).map(|(_, s)| *s);
        let msg5_score = results.iter().find(|(id, _)| *id == 5).map(|(_, s)| *s);

        if let (Some(s4), Some(s5)) = (msg4_score, msg5_score) {
            // Note: BM25 scores are negative, more negative = better match
            assert!(
                s4 < s5,
                "Message with more 'error' occurrences should have lower (better) BM25 score"
            );
        }

        Ok(())
    }

    // ============================================
    // Query Mode Detection Tests
    // ============================================

    #[test]
    fn test_detect_search_mode_natural_language() {
        assert_eq!(
            detect_search_mode("hello world"),
            Fts5SearchMode::NaturalLanguage
        );
        assert_eq!(
            detect_search_mode("error handling"),
            Fts5SearchMode::NaturalLanguage
        );
        assert_eq!(
            detect_search_mode("the quick brown fox"),
            Fts5SearchMode::NaturalLanguage
        );
    }

    #[test]
    fn test_detect_search_mode_code_patterns() {
        // Underscore (snake_case)
        assert_eq!(detect_search_mode("my_function"), Fts5SearchMode::Code);

        // Dot (file extension)
        assert_eq!(detect_search_mode("main.rs"), Fts5SearchMode::Code);

        // camelCase
        assert_eq!(detect_search_mode("getUserById"), Fts5SearchMode::Code);

        // Path separator
        assert_eq!(detect_search_mode("src/lib.rs"), Fts5SearchMode::Code);
    }

    #[test]
    fn test_format_fts5_query_routing() {
        // Natural language routes to messages_fts
        let (table, query) = format_fts5_query("error handling", Fts5SearchMode::Auto);
        assert_eq!(table, "messages_fts");
        assert_eq!(query, r#""error" "handling""#);

        // Code query routes to messages_code_fts
        let (table, query) = format_fts5_query("my_function", Fts5SearchMode::Auto);
        assert_eq!(table, "messages_code_fts");
        assert_eq!(query, r#""my_function""#);

        // Explicit override
        let (table, _) = format_fts5_query("hello", Fts5SearchMode::Code);
        assert_eq!(table, "messages_code_fts");
    }

    // ============================================
    // FTS5 Index Population Tests
    // ============================================

    #[test]
    fn test_fts5_both_indexes_populated() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let (conn, _) = create_export_db(&temp_dir)?;

        // Count messages in both FTS tables
        let porter_count: i64 =
            conn.query_row_map("SELECT COUNT(*) FROM messages_fts", &[], |row| {
                row.get_typed(0)
            })?;

        let code_count: i64 =
            conn.query_row_map("SELECT COUNT(*) FROM messages_code_fts", &[], |row| {
                row.get_typed(0)
            })?;

        assert_eq!(porter_count, 5, "messages_fts should have 5 entries");
        assert_eq!(code_count, 5, "messages_code_fts should have 5 entries");

        Ok(())
    }

    #[test]
    fn test_fts5_snippet_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Test snippet generation with highlighting
        let snippet: String = conn.query_row_map(
            r#"
            SELECT snippet(messages_fts, 0, '<mark>', '</mark>', '...', 32)
            FROM messages_fts
            WHERE messages_fts MATCH '"error"'
            LIMIT 1
            "#,
            &[],
            |row| row.get_typed(0),
        )?;

        // Snippet should contain the matched term with highlighting
        assert!(
            snippet.contains("<mark>") && snippet.contains("</mark>"),
            "Snippet should contain highlight markers"
        );

        Ok(())
    }

    // ============================================
    // Integration with Search SQL Builder
    // ============================================

    #[test]
    fn test_build_fts5_search_sql_works() -> Result<()> {
        use coding_agent_search::pages::fts::build_fts5_search_sql;

        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Build and execute the generated SQL
        let sql = build_fts5_search_sql("messages_fts", 64, false);

        let results: Vec<(i64, String)> =
            conn.query_map_collect(&sql, fparams!["\"error\"", 10_i64, 0_i64], |row| {
                Ok((row.get_typed(0)?, row.get_typed(3)?))
            })?;

        assert!(!results.is_empty(), "Search SQL should return results");

        Ok(())
    }

    #[test]
    fn test_build_fts5_search_sql_with_agent_filter() -> Result<()> {
        use coding_agent_search::pages::fts::build_fts5_search_sql;

        let temp_dir = TempDir::new()?;
        let conn = create_runtime_fts_db(&temp_dir)?;

        // Build SQL with agent filter
        let sql = build_fts5_search_sql("messages_fts", 64, true);

        let results: Vec<i64> = conn.query_map_collect(
            &sql,
            fparams!["\"error\"", "claude", 10_i64, 0_i64],
            |row| row.get_typed(0),
        )?;

        // Should find results with agent="claude"
        assert!(
            !results.is_empty(),
            "Search with agent filter should return results"
        );

        // Try with non-existent agent
        let no_results: Vec<i64> = conn.query_map_collect(
            &sql,
            fparams!["\"error\"", "nonexistent", 10_i64, 0_i64],
            |row| row.get_typed(0),
        )?;

        assert!(
            no_results.is_empty(),
            "Search with non-existent agent should return no results"
        );

        Ok(())
    }
}
