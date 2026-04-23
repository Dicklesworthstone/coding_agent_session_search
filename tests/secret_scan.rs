#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::pages::secret_scan::{
        SecretScanConfig, SecretScanFilters, SecretScanReport, SecretSeverity, scan_database,
    };
    use frankensqlite::Connection as FrankenConnection;
    use frankensqlite::compat::ConnectionExt;
    use frankensqlite::params as fparams;
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

    fn open_db(path: &Path) -> Result<FrankenConnection> {
        let path_str = path.to_string_lossy();
        Ok(FrankenConnection::open(path_str.as_ref())?)
    }

    fn setup_db(path: &Path, message_content: &str) -> Result<()> {
        let conn = open_db(path)?;
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

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")?;
        conn.execute("INSERT INTO workspaces (id, path) VALUES (1, '/tmp/project')")?;
        conn.execute(
            r#"INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, metadata_json)
             VALUES (1, 1, 1, 'Test Conversation', '/tmp/project/session.json', 1700000000000, '{"info":"none"}')"#,
        )?;
        conn.execute_compat(
            r#"INSERT INTO messages (id, conversation_id, idx, content, extra_json)
             VALUES (1, 1, 0, ?1, '{"note":"none"}')"#,
            fparams![message_content],
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
        let conn = open_db(path)?;
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

        conn.execute_compat(
            "INSERT INTO agents (id, slug) VALUES (1, ?1)",
            fparams![agent_slug],
        )?;
        conn.execute_compat(
            "INSERT INTO workspaces (id, path) VALUES (1, ?1)",
            fparams![workspace_path],
        )?;
        conn.execute_compat(
            r#"INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, metadata_json)
             VALUES (1, 1, 1, ?1, '/test/session.json', ?2, ?3)"#,
            fparams![title, started_at, metadata_json],
        )?;

        for (i, (idx, content, extra)) in messages.iter().enumerate() {
            conn.execute_compat(
                r#"INSERT INTO messages (id, conversation_id, idx, content, extra_json)
                 VALUES (?1, 1, ?2, ?3, ?4)"#,
                fparams![i as i64 + 1, *idx, *content, extra.unwrap_or("null")],
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

    fn fixture(parts: &[&str]) -> String {
        parts.concat()
    }

    fn oai_fixture() -> String {
        fixture(&["sk-", "TEST", "abcdefghijklmnopqrstuvwxyz012345"])
    }

    fn allowlisted_oai_fixture() -> String {
        fixture(&["sk-", "ALLOWLIST", "abcdefghijklmnopqrstuvwxyz012345"])
    }

    fn anthropic_fixture() -> String {
        fixture(&["sk-", "ant-", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh"])
    }

    fn aws_access_fixture() -> String {
        fixture(&["AKIA", "IOSFODNN7EXAMPLE"])
    }

    fn aws_s_fixture() -> String {
        fixture(&["wJalr", "XUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"])
    }

    fn gh_fixture() -> String {
        fixture(&["ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"])
    }

    fn jwt_fixture() -> String {
        fixture(&[
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            ".",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            ".",
            "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ])
    }

    fn private_block_fixture(kind: &str, body: &str) -> String {
        format!("-----BEGIN {kind} PRIVATE KEY-----\n{body}")
    }

    fn database_url_fixture(scheme: &str, userinfo: &str, host: &str, path: &str) -> String {
        format!("{scheme}://{userinfo}@{host}/{path}")
    }

    fn generic_kv_line(value: &str) -> String {
        format!("{}={value}", fixture(&["api", "_", "key"]))
    }

    // =========================================================================
    // Original tests
    // =========================================================================

    #[test]
    fn test_secret_scan_detects_oai_fixture() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let payload = oai_fixture();
        setup_db(&db_path, &payload)?;

        let report = scan(&db_path)?;
        assert!(report.findings.iter().any(|f| f.kind == "openai_key"));
        Ok(())
    }

    #[test]
    fn test_secret_scan_allowlist_suppresses() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let payload = allowlisted_oai_fixture();
        setup_db(&db_path, &payload)?;

        let allowlist = vec![format!("{}.*", fixture(&["sk-", "ALLOWLIST"]))];
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

    #[test]
    fn detects_secret_in_message_snippet() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, "harmless content")?;

        let conn = open_db(&db_path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT NOT NULL
            );
            "#,
        )?;
        let snippet_text = format!(r#"const OPENAI = \"{}\";"#, oai_fixture());
        conn.execute_compat(
            r#"INSERT INTO snippets (
                id, message_id, file_path, start_line, end_line, language, snippet_text
            ) VALUES (1, 1, '/tmp/project/src/lib.rs', 10, 12, 'rust', ?1)"#,
            fparams![snippet_text.as_str()],
        )?;
        drop(conn);

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| {
                f.kind == "openai_key"
                    && f.location
                        == coding_agent_search::pages::secret_scan::SecretLocation::MessageSnippet
            }),
            "should detect secrets present only in snippets"
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
        let content = format!("credentials: {}", aws_access_fixture());
        setup_db(&db_path, &content)?;

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
    fn detects_aws_s_fixture() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(
            &db_path,
            &format!(
                "{}={}",
                fixture(&["aws", "_secret", "_key"]),
                aws_s_fixture()
            ),
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
    fn detects_gh_fixture() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let content = format!("token {}", gh_fixture());
        setup_db(&db_path, &content)?;

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
    fn detects_anthropic_fixture() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, &anthropic_fixture())?;

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
    fn anthropic_key_is_not_reported_as_oai_fixture() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, &anthropic_fixture())?;

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
        setup_db(&db_path, &format!("auth: {}", jwt_fixture()))?;

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
            &private_block_fixture("RSA", "MIIEpAIBAAKCAQEA..."),
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
            &private_block_fixture("ENCRYPTED", "MIIFHjBABgkqhkiG9w0BBQMwDgQIc..."),
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
            &format!(
                "db={}",
                database_url_fixture(
                    "postgres",
                    "admin:secret123",
                    "db.example.com:5432",
                    "production"
                )
            ),
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
        setup_db(&db_path, &generic_kv_line("abcdefgh12345678"))?;

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
        let title = format!("Debug {} issue", oai_fixture());
        setup_db_full(
            &db_path,
            "claude",
            "/tmp/proj",
            &title,
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
        let metadata_json = format!(r#"{{"token":"{}"}}"#, oai_fixture());
        setup_db_full(
            &db_path,
            "claude",
            "/tmp/proj",
            "Clean title",
            &metadata_json,
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
        let extra_json = format!(r#"{{"key":"{}"}}"#, aws_access_fixture());
        let messages = [(0, "safe content", Some(extra_json.as_str()))];
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "Clean title",
            "{}",
            1700000000000,
            &messages,
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
        let payload = oai_fixture();
        let messages = [(0, payload.as_str(), None)];
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "title",
            "{}",
            1700000000000,
            &messages,
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
        let payload = oai_fixture();
        let messages = [(0, payload.as_str(), None)];
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/project-a",
            "title",
            "{}",
            1700000000000,
            &messages,
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
        let payload = oai_fixture();
        let messages = [(0, payload.as_str(), None)];
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "title",
            "{}",
            1000000000000, // old timestamp
            &messages,
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

        let conn = open_db(&db_path)?;
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
        let aws_message = format!("found key {} in env", aws_access_fixture());
        let openai_message = format!("using {} for API", oai_fixture());
        let db_message = format!(
            "connect {}",
            database_url_fixture("postgres", "admin:pass", "host:5432", "db")
        );
        let messages = [
            (0, aws_message.as_str(), None),
            (1, openai_message.as_str(), None),
            (2, db_message.as_str(), None),
        ];
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "Clean title",
            "{}",
            1700000000000,
            &messages,
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
        let content = format!(
            "{}={} {} {}",
            fixture(&["aws", "_secret", "_key"]),
            aws_s_fixture(),
            oai_fixture(),
            generic_kv_line("my_generic_token_value_here"),
        );
        setup_db(&db_path, &content)?;

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
            &format!(
                "{} and {}",
                oai_fixture(),
                generic_kv_line("my_token_value_here")
            ),
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
        setup_db(&db_path, &private_block_fixture("RSA", "MIIEpAI..."))?;

        let report = scan(&db_path)?;
        assert!(report.summary.has_critical, "should flag critical severity");
        Ok(())
    }

    #[test]
    fn has_critical_flag_false_when_no_critical() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        // api_key is Low severity only
        setup_db(&db_path, &generic_kv_line("my_generic_token_value_here"))?;

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
    fn redaction_does_not_leak_full_match() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let full_match = oai_fixture();
        setup_db(&db_path, &full_match)?;

        let report = scan(&db_path)?;
        for finding in &report.findings {
            assert!(
                !finding.match_redacted.contains(&full_match),
                "match_redacted should not contain full secret: {}",
                finding.match_redacted,
            );
            assert!(
                !finding.context.contains(&full_match),
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
        let payload = oai_fixture();
        let messages = [(0, payload.as_str(), None)];
        setup_db_full(
            &db_path,
            "gemini",
            "/home/user/myproject",
            "title",
            "{}",
            1700000000000,
            &messages,
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
            &private_block_fixture("OPENSSH", "b3BlbnNzaC1rZXktdjEA..."),
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
        setup_db(&db_path, &private_block_fixture("EC", "MHQCAQEE..."))?;

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
        setup_db(
            &db_path,
            &database_url_fixture("mysql", "root:password", "localhost:3306", "mydb"),
        )?;

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
        setup_db(
            &db_path,
            &database_url_fixture("mongodb", "admin:secret", "cluster.mongodb.net", "prod"),
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "database_url"),
            "should detect MongoDB connection URL"
        );
        Ok(())
    }
}
