#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::pages::secret_scan::{
        SecretScanConfig, SecretScanFilters, SecretSeverity, scan_database,
    };
    use rusqlite::Connection;
    use std::path::Path;
    use tempfile::TempDir;

    fn setup_db(path: &Path, message_content: &str) -> Result<()> {
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
                metadata_json TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                content TEXT NOT NULL,
                extra_json TEXT
            );
            "#,
        )?;

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')", [])?;
        conn.execute(
            "INSERT INTO workspaces (id, path) VALUES (1, '/tmp/project')",
            [],
        )?;
        conn.execute(
            r#"INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, metadata_json)
             VALUES (1, 1, 1, 'Test Conversation', '/tmp/project/session.json', 1700000000000, '{"info":"none"}')"#,
            [],
        )?;
        conn.execute(
            r#"INSERT INTO messages (id, conversation_id, idx, content, extra_json)
             VALUES (1, 1, 0, ?1, '{"note":"none"}')"#,
            [message_content],
        )?;

        Ok(())
    }

    #[test]
    fn test_secret_scan_detects_openai_key() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let secret = "sk-TESTabcdefghijklmnopqrstuvwxyz012345";
        setup_db(&db_path, secret)?;

        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false)?;
        let report = scan_database(&db_path, &filters, &config, None, None)?;

        assert!(report.findings.iter().any(|f| f.kind == "openai_key"));
        Ok(())
    }

    #[test]
    fn test_secret_scan_allowlist_suppresses() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let secret = "sk-ALLOWLISTabcdefghijklmnopqrstuvwxyz012345";
        setup_db(&db_path, secret)?;

        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let allowlist = vec![r"sk-ALLOWLIST.*".to_string()];
        let config = SecretScanConfig::from_inputs_with_env(&allowlist, &[], false)?;
        let report = scan_database(&db_path, &filters, &config, None, None)?;

        assert!(!report.findings.iter().any(|f| f.kind == "openai_key"));
        Ok(())
    }

    #[test]
    fn test_secret_scan_entropy_detection() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let entropy_string = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        setup_db(&db_path, entropy_string)?;

        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false)?;
        let report = scan_database(&db_path, &filters, &config, None, None)?;

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "high_entropy_base64")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.severity == SecretSeverity::Medium)
        );
        Ok(())
    }
}
