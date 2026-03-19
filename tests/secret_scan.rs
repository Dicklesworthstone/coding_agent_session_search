#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::pages::secret_scan::{
        SecretScanConfig, SecretScanFilters, SecretScanReport, SecretSeverity, scan_database,
    };
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn severity_rank(s: SecretSeverity) -> u8 {
        match s {
            SecretSeverity::Critical => 0,
            SecretSeverity::High => 1,
            SecretSeverity::Medium => 2,
            SecretSeverity::Low => 3,
        }
    }

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

    /// Extended setup: populate DB with custom title, metadata, and multiple messages.
    fn setup_db_full(
        path: &Path,
        agent_slug: &str,
        workspace_path: &str,
        title: &str,
        metadata_json: &str,
        started_at: i64,
        messages: &[(i64, &str, Option<&str>)], // (idx, content, extra_json)
    ) -> Result<()> {
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

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, ?1)", [agent_slug])?;
        conn.execute(
            "INSERT INTO workspaces (id, path) VALUES (1, ?1)",
            [workspace_path],
        )?;
        conn.execute(
            r#"INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, metadata_json)
             VALUES (1, 1, 1, ?1, '/test/session.json', ?2, ?3)"#,
            rusqlite::params![title, started_at, metadata_json],
        )?;

        for (i, (idx, content, extra)) in messages.iter().enumerate() {
            conn.execute(
                r#"INSERT INTO messages (id, conversation_id, idx, content, extra_json)
                 VALUES (?1, 1, ?2, ?3, ?4)"#,
                rusqlite::params![i as i64 + 1, idx, content, extra.unwrap_or("null"),],
            )?;
        }

        Ok(())
    }

    fn no_filters() -> SecretScanFilters {
        SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: None,
            until_ts: None,
        }
    }

    fn default_config() -> SecretScanConfig {
        SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap()
    }

    fn scan(db_path: &Path) -> Result<SecretScanReport> {
        scan_database(db_path, &no_filters(), &default_config(), None, None)
    }

    // =========================================================================
    // Original tests
    // =========================================================================

    #[test]
    fn test_secret_scan_detects_openai_key() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let secret = "sk-TESTabcdefghijklmnopqrstuvwxyz012345";
        setup_db(&db_path, secret)?;

        let report = scan(&db_path)?;
        assert!(report.findings.iter().any(|f| f.kind == "openai_key"));
        Ok(())
    }

    #[test]
    fn test_secret_scan_allowlist_suppresses() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let secret = "sk-ALLOWLISTabcdefghijklmnopqrstuvwxyz012345";
        setup_db(&db_path, secret)?;

        let allowlist = vec![r"sk-ALLOWLIST.*".to_string()];
        let config = SecretScanConfig::from_inputs_with_env(&allowlist, &[], false)?;
        let report = scan_database(&db_path, &no_filters(), &config, None, None)?;

        assert!(!report.findings.iter().any(|f| f.kind == "openai_key"));
        Ok(())
    }

    #[test]
    fn test_secret_scan_entropy_detection() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let entropy_string = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        setup_db(&db_path, entropy_string)?;

        let report = scan(&db_path)?;
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

    // =========================================================================
    // Built-in pattern detection tests (br-ig84)
    // =========================================================================

    #[test]
    fn detects_aws_access_key_id() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "credentials: AKIAIOSFODNN7EXAMPLE")?;

        let report = scan(&db_path)?;
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "aws_access_key_id"),
            "should detect AWS access key ID pattern"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "aws_access_key_id")
            .unwrap();
        assert_eq!(finding.severity, SecretSeverity::High);
        Ok(())
    }

    #[test]
    fn detects_aws_secret_key() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "aws_secret_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "aws_secret_key"),
            "should detect AWS secret key pattern"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "aws_secret_key")
            .unwrap();
        assert_eq!(finding.severity, SecretSeverity::Critical);
        Ok(())
    }

    #[test]
    fn detects_github_pat() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij")?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "github_pat"),
            "should detect GitHub PAT"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "github_pat")
            .unwrap();
        assert_eq!(finding.severity, SecretSeverity::High);
        Ok(())
    }

    #[test]
    fn detects_anthropic_key() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh")?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "anthropic_key"),
            "should detect Anthropic API key"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "anthropic_key")
            .unwrap();
        assert_eq!(finding.severity, SecretSeverity::High);
        Ok(())
    }

    #[test]
    fn anthropic_key_is_not_reported_as_openai_key() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh")?;

        let report = scan(&db_path)?;
        assert!(
            !report.findings.iter().any(|f| f.kind == "openai_key"),
            "Anthropic keys should not also be classified as OpenAI keys"
        );
        Ok(())
    }

    #[test]
    fn detects_jwt_token() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "auth: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "jwt"),
            "should detect JWT"
        );
        let finding = report.findings.iter().find(|f| f.kind == "jwt").unwrap();
        assert_eq!(finding.severity, SecretSeverity::Medium);
        Ok(())
    }

    #[test]
    fn detects_private_key_header() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "private_key"),
            "should detect private key header"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "private_key")
            .unwrap();
        assert_eq!(finding.severity, SecretSeverity::Critical);
        Ok(())
    }

    #[test]
    fn detects_encrypted_private_key_header() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIFHjBABgkqhkiG9w0BBQMwDgQIc...",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "private_key"),
            "should detect encrypted private key header"
        );
        Ok(())
    }

    #[test]
    fn detects_database_url() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "db=postgres://admin:secret123@db.example.com:5432/production",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "database_url"),
            "should detect database URL"
        );
        Ok(())
    }

    #[test]
    fn detects_generic_api_key() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "api_key=abcdefgh12345678")?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "generic_api_key"),
            "should detect generic API key"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "generic_api_key")
            .unwrap();
        assert_eq!(finding.severity, SecretSeverity::Low);
        Ok(())
    }

    // =========================================================================
    // Scanning location tests (br-ig84)
    // =========================================================================

    #[test]
    fn detects_secret_in_conversation_title() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "claude",
            "/tmp/proj",
            "Debug sk-TESTabcdefghijklmnopqrstuvwxyz012345 issue",
            "{}",
            1700000000000,
            &[(0, "safe content only", None)],
        )?;

        let report = scan(&db_path)?;
        let title_finding = report.findings.iter().find(|f| {
            f.kind == "openai_key"
                && f.location
                    == coding_agent_search::pages::secret_scan::SecretLocation::ConversationTitle
        });
        assert!(title_finding.is_some(), "should detect secret in title");
        Ok(())
    }

    #[test]
    fn detects_secret_in_metadata_json() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "claude",
            "/tmp/proj",
            "Clean title",
            r#"{"token":"sk-TESTabcdefghijklmnopqrstuvwxyz012345"}"#,
            1700000000000,
            &[(0, "safe content", None)],
        )?;

        let report = scan(&db_path)?;
        let meta_finding = report.findings.iter().find(|f| {
            f.kind == "openai_key"
                && f.location
                    == coding_agent_search::pages::secret_scan::SecretLocation::ConversationMetadata
        });
        assert!(meta_finding.is_some(), "should detect secret in metadata");
        Ok(())
    }

    #[test]
    fn detects_secret_in_message_extra_json() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "Clean title",
            "{}",
            1700000000000,
            &[(0, "safe content", Some(r#"{"key":"AKIAIOSFODNN7EXAMPLE"}"#))],
        )?;

        let report = scan(&db_path)?;
        let extra_finding = report.findings.iter().find(|f| {
            f.kind == "aws_access_key_id"
                && f.location
                    == coding_agent_search::pages::secret_scan::SecretLocation::MessageMetadata
        });
        assert!(
            extra_finding.is_some(),
            "should detect secret in message extra_json"
        );
        Ok(())
    }

    // =========================================================================
    // Filter tests (br-ig84)
    // =========================================================================

    #[test]
    fn agent_filter_limits_scan_to_matching_agent() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "title",
            "{}",
            1700000000000,
            &[(0, "sk-TESTabcdefghijklmnopqrstuvwxyz012345", None)],
        )?;

        // Filter to "claude" agent — should NOT find the "codex" secret
        let filters = SecretScanFilters {
            agents: Some(vec!["claude".to_string()]),
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let report = scan_database(&db_path, &filters, &default_config(), None, None)?;
        assert_eq!(
            report.findings.len(),
            0,
            "wrong agent filter should produce no findings"
        );
        Ok(())
    }

    #[test]
    fn workspace_filter_limits_scan() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/project-a",
            "title",
            "{}",
            1700000000000,
            &[(0, "sk-TESTabcdefghijklmnopqrstuvwxyz012345", None)],
        )?;

        // Filter to different workspace — should NOT find secrets
        let filters = SecretScanFilters {
            agents: None,
            workspaces: Some(vec![PathBuf::from("/tmp/project-b")]),
            since_ts: None,
            until_ts: None,
        };
        let report = scan_database(&db_path, &filters, &default_config(), None, None)?;
        assert_eq!(
            report.findings.len(),
            0,
            "wrong workspace filter should produce no findings"
        );
        Ok(())
    }

    #[test]
    fn time_range_filter_excludes_old_conversations() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "title",
            "{}",
            1000000000000, // old timestamp
            &[(0, "sk-TESTabcdefghijklmnopqrstuvwxyz012345", None)],
        )?;

        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: Some(1700000000000), // newer than conversation
            until_ts: None,
        };
        let report = scan_database(&db_path, &filters, &default_config(), None, None)?;
        assert_eq!(
            report.findings.len(),
            0,
            "time filter should exclude old conversations"
        );
        Ok(())
    }

    // =========================================================================
    // Edge cases and robustness tests (br-ig84)
    // =========================================================================

    #[test]
    fn empty_database_returns_empty_report() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");

        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE agents (id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
            CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT NOT NULL);
            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY, agent_id INTEGER NOT NULL,
                workspace_id INTEGER, title TEXT, source_path TEXT NOT NULL,
                started_at INTEGER, metadata_json TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL, content TEXT NOT NULL, extra_json TEXT
            );
            "#,
        )?;
        drop(conn);

        let report = scan(&db_path)?;
        assert_eq!(report.findings.len(), 0);
        assert_eq!(report.summary.total, 0);
        assert!(!report.summary.has_critical);
        assert!(!report.summary.truncated);
        Ok(())
    }

    #[test]
    fn safe_content_produces_no_findings() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "This is perfectly safe content about Rust programming.",
        )?;

        let report = scan(&db_path)?;
        assert_eq!(
            report.findings.len(),
            0,
            "safe content should have no findings"
        );
        Ok(())
    }

    #[test]
    fn multiple_secrets_in_multiple_messages() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "Clean title",
            "{}",
            1700000000000,
            &[
                (0, "found key AKIAIOSFODNN7EXAMPLE in env", None),
                (
                    1,
                    "using sk-TESTabcdefghijklmnopqrstuvwxyz012345 for API",
                    None,
                ),
                (2, "connect postgres://admin:pass@host:5432/db", None),
            ],
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.len() >= 3,
            "should find multiple secrets: {}",
            report.findings.len()
        );

        let kinds: Vec<&str> = report.findings.iter().map(|f| f.kind.as_str()).collect();
        assert!(kinds.contains(&"aws_access_key_id"), "should find AWS key");
        assert!(kinds.contains(&"openai_key"), "should find OpenAI key");
        assert!(kinds.contains(&"database_url"), "should find DB URL");
        Ok(())
    }

    #[test]
    fn findings_sorted_by_severity_then_kind() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        // Include secrets of different severities
        let content = concat!(
            "aws_secret_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY ",
            "sk-TESTabcdefghijklmnopqrstuvwxyz012345 ",
            "api_key=my_generic_token_value_here",
        );
        setup_db(&db_path, content)?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.len() >= 2,
            "should find multiple severities"
        );

        // Verify sorted: Critical first, then High, Medium, Low
        for i in 1..report.findings.len() {
            let prev = severity_rank(report.findings[i - 1].severity);
            let curr = severity_rank(report.findings[i].severity);
            assert!(
                prev <= curr,
                "findings not sorted: {} before {} (indices {}, {})",
                report.findings[i - 1].kind,
                report.findings[i].kind,
                i - 1,
                i,
            );
        }
        Ok(())
    }

    #[test]
    fn summary_counts_match_findings() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "sk-TESTabcdefghijklmnopqrstuvwxyz012345 and api_key=my_token_value_here",
        )?;

        let report = scan(&db_path)?;
        assert_eq!(report.summary.total, report.findings.len());

        let total_by_sev: usize = report.summary.by_severity.values().sum();
        assert_eq!(
            total_by_sev,
            report.findings.len(),
            "by_severity sum should match total"
        );
        Ok(())
    }

    #[test]
    fn has_critical_flag_set_when_critical_found() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAI...")?;

        let report = scan(&db_path)?;
        assert!(report.summary.has_critical, "should flag critical severity");
        Ok(())
    }

    #[test]
    fn has_critical_flag_false_when_no_critical() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        // api_key is Low severity only
        setup_db(&db_path, "api_key=my_generic_token_value_here")?;

        let report = scan(&db_path)?;
        assert!(
            !report.summary.has_critical,
            "no critical findings -> has_critical should be false"
        );
        Ok(())
    }

    #[test]
    fn denylist_via_database_scan_always_critical() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "internal-secret-XYZZY-token")?;

        let denylist = vec!["internal-secret-.*-token".to_string()];
        let config = SecretScanConfig::from_inputs_with_env(&[], &denylist, false)?;
        let report = scan_database(&db_path, &no_filters(), &config, None, None)?;

        assert!(!report.findings.is_empty(), "denylist pattern should match");
        let finding = &report.findings[0];
        assert_eq!(finding.severity, SecretSeverity::Critical);
        assert_eq!(finding.kind, "denylist");
        Ok(())
    }

    #[test]
    fn redaction_does_not_leak_full_secret() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let full_secret = "sk-TESTabcdefghijklmnopqrstuvwxyz012345";
        setup_db(&db_path, full_secret)?;

        let report = scan(&db_path)?;
        for finding in &report.findings {
            assert!(
                !finding.match_redacted.contains(full_secret),
                "match_redacted should not contain full secret: {}",
                finding.match_redacted,
            );
            assert!(
                !finding.context.contains(full_secret),
                "context should not contain full secret: {}",
                finding.context,
            );
        }
        Ok(())
    }

    #[test]
    fn finding_includes_agent_and_source_path() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db_full(
            &db_path,
            "gemini",
            "/home/user/myproject",
            "title",
            "{}",
            1700000000000,
            &[(0, "sk-TESTabcdefghijklmnopqrstuvwxyz012345", None)],
        )?;

        let report = scan(&db_path)?;
        assert!(!report.findings.is_empty());
        let finding = &report.findings[0];
        assert_eq!(finding.agent.as_deref(), Some("gemini"));
        assert_eq!(finding.workspace.as_deref(), Some("/home/user/myproject"));
        assert!(finding.source_path.is_some());
        assert!(finding.conversation_id.is_some());
        Ok(())
    }

    #[test]
    fn nonexistent_database_returns_error() {
        let result = scan_database(
            Path::new("/nonexistent/path/scan.db"),
            &no_filters(),
            &default_config(),
            None,
            None,
        );
        assert!(result.is_err(), "nonexistent DB should return error");
    }

    #[test]
    fn hex_entropy_detection_for_long_hex_strings() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        // 64-char hex string (looks like a SHA-256 hash or secret)
        setup_db(
            &db_path,
            "key: a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "high_entropy_hex"),
            "should detect high-entropy hex string"
        );
        Ok(())
    }

    #[test]
    fn openssh_private_key_detected() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEA...",
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "private_key"),
            "should detect OPENSSH private key header"
        );
        Ok(())
    }

    #[test]
    fn ec_private_key_detected() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "-----BEGIN EC PRIVATE KEY-----\nMHQCAQEE...")?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "private_key"),
            "should detect EC private key header"
        );
        Ok(())
    }

    #[test]
    fn mysql_connection_url_detected() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "mysql://root:password@localhost:3306/mydb")?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "database_url"),
            "should detect MySQL connection URL"
        );
        Ok(())
    }

    #[test]
    fn mongodb_connection_url_detected() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "mongodb://admin:secret@cluster.mongodb.net/prod")?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "database_url"),
            "should detect MongoDB connection URL"
        );
        Ok(())
    }
}
