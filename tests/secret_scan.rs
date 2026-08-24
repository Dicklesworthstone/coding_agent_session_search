#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::franken_sync::Connection as FrankenConnection;
    use coding_agent_search::franken_sync::compat::ConnectionExt;
    use coding_agent_search::franken_sync::params as fparams;
    use coding_agent_search::pages::secret_scan::{
        SecretLocation, SecretScanConfig, SecretScanFilters, SecretScanReport, SecretSeverity,
        scan_database,
    };
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

    fn setup_db_with_binary_metadata(
        path: &Path,
        metadata_json: &str,
        metadata_bin: &[u8],
        extra_json: &str,
        extra_bin: &[u8],
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
                metadata_json TEXT,
                metadata_bin BLOB
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                content TEXT NOT NULL,
                extra_json TEXT,
                extra_bin BLOB
            );
            "#,
        )?;

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")?;
        conn.execute("INSERT INTO workspaces (id, path) VALUES (1, '/tmp/project')")?;
        conn.execute_compat(
            r#"INSERT INTO conversations (
                id, agent_id, workspace_id, title, source_path, started_at,
                metadata_json, metadata_bin
            ) VALUES (1, 1, 1, 'Binary metadata', '/tmp/project/session.json',
                1700000000000, ?1, ?2)"#,
            fparams![metadata_json, metadata_bin],
        )?;
        conn.execute_compat(
            r#"INSERT INTO messages (
                id, conversation_id, idx, content, extra_json, extra_bin
            ) VALUES (1, 1, 0, 'safe content', ?1, ?2)"#,
            fparams![extra_json, extra_bin],
        )?;

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

    fn aws_temporary_access_fixture() -> String {
        fixture(&["ASIA", "IOSFODNN7EXAMPLE"])
    }

    fn aws_s_fixture() -> String {
        fixture(&["wJalr", "XUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"])
    }

    fn gh_fixture() -> String {
        fixture(&["ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"])
    }

    fn fine_grained_gh_fixture() -> String {
        fixture(&["github", "_pat_", "11AA22bb33CC44dd55EE66ff77GG88hh"])
    }

    fn project_oai_fixture() -> String {
        fixture(&["sk-", "proj-", "AbCdEfGhIjKlMnOpQrStUvWxYz_12345"])
    }

    fn segmented_anthropic_fixture() -> String {
        fixture(&["sk-", "ant-", "api03-", "AbCdEfGhIjKlMnOpQrStUvWxYz_12345"])
    }

    fn aws_session_fixture() -> String {
        fixture(&[
            "IQoJb3JpZ2luX2VjEExampleSessionToken",
            "1234567890+/=",
        ])
    }

    fn slack_xoxo_fixture() -> String {
        fixture(&["xox", "o-", "1234567890-ABCDEFGHIJ"])
    }

    fn stripe_live_fixture() -> String {
        fixture(&["sk_", "live_", "AbCdEfGhIjKlMnOpQrStUvWxYz123456"])
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

    fn pgp_private_block_fixture(body: &str) -> String {
        format!("-----BEGIN {} PRIVATE KEY {}-----\n{body}", "PGP", "BLOCK")
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
        let content = format!(
            "permanent credentials: {}; temporary credentials: {}",
            aws_access_fixture(),
            aws_temporary_access_fixture()
        );
        setup_db(&db_path, &content)?;

        let report = scan(&db_path)?;
        let access_key_findings = report
            .findings
            .iter()
            .filter(|finding| finding.kind == "aws_access_key_id")
            .collect::<Vec<_>>();
        assert_eq!(
            access_key_findings.len(),
            2,
            "should detect both AKIA and ASIA access key IDs"
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
    fn detects_pgp_private_key_block_header() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        setup_db(&db_path, &pgp_private_block_fixture("xcLYBF..."))?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|f| f.kind == "private_key"),
            "should detect PGP private key block header"
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

    #[test]
    fn detects_current_segmented_and_provider_credential_formats() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let mongodb_url = database_url_fixture(
            "mongodb+srv",
            "service:credential",
            "cluster.example.net",
            "production",
        );
        let amqp_url = database_url_fixture(
            "amqp",
            "worker:credential",
            "queue.example.net",
            "vhost",
        );
        let content = format!(
            "{} {} {} aws_session_token={} {} {} {} {}",
            project_oai_fixture(),
            segmented_anthropic_fixture(),
            fine_grained_gh_fixture(),
            aws_session_fixture(),
            slack_xoxo_fixture(),
            stripe_live_fixture(),
            mongodb_url,
            amqp_url,
        );
        setup_db(&db_path, &content)?;

        let report = scan(&db_path)?;
        for expected_kind in [
            "openai_key",
            "anthropic_key",
            "github_pat",
            "aws_session_token",
            "slack_token",
            "stripe_key",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.kind == expected_kind),
                "scanner missed current credential kind {expected_kind}: {:#?}",
                report.findings
            );
        }
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|finding| finding.kind == "database_url")
                .count(),
            2,
            "mongodb+srv and amqp credential URLs should both be detected"
        );
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
        // Bead 7k7pl: pin SEVERITY + REDACTION on the found secret,
        // not just "finding present". openai_key is a high-severity
        // secret and the scanner must redact the payload. A
        // regression that down-graded severity to Low or left
        // match_redacted empty would slip past `.is_some()` while
        // breaking the security contract.
        let finding = title_finding.expect("should detect secret in title");
        assert!(
            matches!(
                finding.severity,
                SecretSeverity::Critical | SecretSeverity::High
            ),
            "openai_key in title must be Critical or High severity; got {:?}",
            finding.severity
        );
        assert!(
            !finding.match_redacted.is_empty(),
            "redacted match must be non-empty"
        );
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
        // Bead 7k7pl: pin SEVERITY + REDACTION, not just "finding
        // present" (see detects_secret_in_title for the same
        // rationale).
        let finding = meta_finding.expect("should detect secret in metadata");
        assert!(
            matches!(
                finding.severity,
                SecretSeverity::Critical | SecretSeverity::High
            ),
            "openai_key in metadata must be Critical or High severity; got {:?}",
            finding.severity
        );
        assert!(
            !finding.match_redacted.is_empty(),
            "redacted match must be non-empty"
        );
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

    #[test]
    fn binary_metadata_is_authoritative_and_scanned_before_legacy_json() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let metadata_bin = rmp_serde::to_vec(&serde_json::json!({
            "credential": aws_access_fixture(),
        }))?;
        let extra_bin = rmp_serde::to_vec(&serde_json::json!({
            "credential": fine_grained_gh_fixture(),
        }))?;
        let legacy_metadata = serde_json::json!({ "credential": oai_fixture() }).to_string();
        let legacy_extra =
            serde_json::json!({ "credential": segmented_anthropic_fixture() }).to_string();
        setup_db_with_binary_metadata(
            &db_path,
            &legacy_metadata,
            &metadata_bin,
            &legacy_extra,
            &extra_bin,
        )?;

        let report = scan(&db_path)?;
        assert!(
            report.findings.iter().any(|finding| {
                finding.kind == "aws_access_key_id"
                    && finding.location
                        == coding_agent_search::pages::secret_scan::SecretLocation::ConversationMetadata
            }),
            "authoritative metadata_bin secret should be scanned"
        );
        assert!(
            report.findings.iter().any(|finding| {
                finding.kind == "github_pat"
                    && finding.location
                        == coding_agent_search::pages::secret_scan::SecretLocation::MessageMetadata
            }),
            "authoritative extra_bin secret should be scanned"
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.kind == "openai_key"),
            "legacy metadata_json must not override a non-empty metadata_bin"
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.kind == "anthropic_key"),
            "legacy extra_json must not override a non-empty extra_bin"
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.kind == "sensitive_metadata_field"),
            "a structurally sensitive field already covered by a token detector must not duplicate the finding"
        );
        Ok(())
    }

    #[test]
    fn binary_metadata_detects_short_sensitive_fields_without_leaking_values() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let short_pin = ["12", "34"].concat();
        let short_cookie = ["s", "id"].concat();
        let metadata_bin = rmp_serde::to_vec(&serde_json::json!({
            "pin": short_pin,
        }))?;
        let extra_bin = rmp_serde::to_vec(&serde_json::json!({
            "nested": { "cookie": short_cookie },
        }))?;
        setup_db_with_binary_metadata(&db_path, "{}", &metadata_bin, "{}", &extra_bin)?;

        let report = scan(&db_path)?;
        let structural = report
            .findings
            .iter()
            .filter(|finding| finding.kind == "sensitive_metadata_field")
            .collect::<Vec<_>>();
        assert_eq!(structural.len(), 2);
        assert!(structural.iter().any(|finding| {
            finding.location
                == coding_agent_search::pages::secret_scan::SecretLocation::ConversationMetadata
        }));
        assert!(structural.iter().any(|finding| {
            finding.location
                == coding_agent_search::pages::secret_scan::SecretLocation::MessageMetadata
        }));

        let serialized = serde_json::to_string(&report)?;
        assert!(!serialized.contains("1234"));
        assert!(!serialized.contains("sid"));
        Ok(())
    }

    #[test]
    fn legacy_json_detects_short_sensitive_fields() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let metadata = serde_json::json!({ "pin": ["12", "34"].concat() }).to_string();
        let extra = serde_json::json!({ "cookie": ["s", "id"].concat() }).to_string();
        let messages = [(0, "safe content", Some(extra.as_str()))];
        setup_db_full(
            &db_path,
            "codex",
            "/tmp/proj",
            "Clean title",
            &metadata,
            1700000000000,
            &messages,
        )?;

        let report = scan(&db_path)?;
        let structural = report
            .findings
            .iter()
            .filter(|finding| finding.kind == "sensitive_metadata_field")
            .collect::<Vec<_>>();
        assert_eq!(structural.len(), 2);
        assert!(structural.iter().any(|finding| {
            finding.location
                == coding_agent_search::pages::secret_scan::SecretLocation::ConversationMetadata
        }));
        assert!(structural.iter().any(|finding| {
            finding.location
                == coding_agent_search::pages::secret_scan::SecretLocation::MessageMetadata
        }));
        Ok(())
    }

    #[test]
    fn empty_null_and_already_redacted_sensitive_fields_remain_clean() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let metadata_bin = rmp_serde::to_vec(&serde_json::json!({
            "pin": "",
            "cookie": null,
            "password": "[REDACTED]",
        }))?;
        let extra_bin = rmp_serde::to_vec(&serde_json::json!({
            "credentials": { "empty": "" },
            "token": [],
        }))?;
        setup_db_with_binary_metadata(&db_path, "{}", &metadata_bin, "{}", &extra_bin)?;

        let report = scan(&db_path)?;
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.kind == "sensitive_metadata_field")
        );
        Ok(())
    }

    #[test]
    fn malformed_nonempty_metadata_bin_fails_closed() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let malformed = [0xc1_u8];
        let safe_extra = rmp_serde::to_vec(&serde_json::json!({ "safe": true }))?;
        setup_db_with_binary_metadata(&db_path, "{}", &malformed, "{}", &safe_extra)?;

        let error = scan(&db_path).expect_err("malformed authoritative metadata must fail");
        assert!(
            error.to_string().contains("conversations.metadata_bin"),
            "error should identify malformed authoritative column: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn malformed_nonempty_extra_bin_fails_closed() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let safe_metadata = rmp_serde::to_vec(&serde_json::json!({ "safe": true }))?;
        let malformed = [0xc1_u8];
        setup_db_with_binary_metadata(&db_path, "{}", &safe_metadata, "{}", &malformed)?;

        let error = scan(&db_path).expect_err("malformed authoritative message metadata must fail");
        assert!(
            error.to_string().contains("messages.extra_bin"),
            "error should identify malformed authoritative column: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn metadata_bin_with_trailing_bytes_fails_closed() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let mut metadata_with_trailing =
            rmp_serde::to_vec(&serde_json::json!({ "safe": true }))?;
        metadata_with_trailing.push(0xc1);
        let safe_extra = rmp_serde::to_vec(&serde_json::json!({ "safe": true }))?;
        setup_db_with_binary_metadata(
            &db_path,
            "{}",
            &metadata_with_trailing,
            "{}",
            &safe_extra,
        )?;

        let error = scan(&db_path).expect_err("trailing metadata bytes must fail closed");
        let message = error.to_string();
        assert!(message.contains("conversations.metadata_bin"), "{error:#}");
        assert!(message.contains("trailing bytes"), "{error:#}");
        Ok(())
    }

    #[test]
    fn extra_bin_with_trailing_bytes_fails_closed() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let safe_metadata = rmp_serde::to_vec(&serde_json::json!({ "safe": true }))?;
        let mut extra_with_trailing =
            rmp_serde::to_vec(&serde_json::json!({ "safe": true }))?;
        extra_with_trailing.push(0xc1);
        setup_db_with_binary_metadata(
            &db_path,
            "{}",
            &safe_metadata,
            "{}",
            &extra_with_trailing,
        )?;

        let error = scan(&db_path).expect_err("trailing message metadata bytes must fail closed");
        let message = error.to_string();
        assert!(message.contains("messages.extra_bin"), "{error:#}");
        assert!(message.contains("trailing bytes"), "{error:#}");
        Ok(())
    }

    #[test]
    fn keyset_pages_reach_later_conversations_messages_and_snippets() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let conn = open_db(&db_path)?;
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
                metadata_json TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                content TEXT NOT NULL,
                extra_json TEXT
            );
            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                snippet_text TEXT NOT NULL
            );
            INSERT INTO agents (id, slug) VALUES (1, 'codex');
            INSERT INTO workspaces (id, path) VALUES (1, '/tmp/project');
            BEGIN;
            "#,
        )?;

        // The production scanner pages at 128 rows. Use negative primary keys
        // so a zero cursor sentinel would skip the entire scan, and put each
        // only finding on row 129 so every table must advance its keyset before
        // detecting it.
        for ordinal in 1_i64..=129 {
            let id = ordinal - 130;
            let title = if ordinal == 129 {
                "PAGE_CONVERSATION_SECRET"
            } else {
                "safe title"
            };
            let content = if ordinal == 129 {
                "PAGE_MESSAGE_SECRET"
            } else {
                "safe content"
            };
            let snippet = if ordinal == 129 {
                "PAGE_SNIPPET_SECRET"
            } else {
                "safe snippet"
            };
            let source_path = format!("/tmp/project/session-{id}.jsonl");
            conn.execute_compat(
                "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, metadata_json) VALUES (?1, 1, 1, ?2, ?3, 1700000000000, '{}')",
                fparams![id, title, source_path],
            )?;
            conn.execute_compat(
                "INSERT INTO messages (id, conversation_id, idx, content, extra_json) VALUES (?1, ?1, 0, ?2, '{}')",
                fparams![id, content],
            )?;
            conn.execute_compat(
                "INSERT INTO snippets (id, message_id, snippet_text) VALUES (?1, ?1, ?2)",
                fparams![id, snippet],
            )?;
        }
        conn.execute("COMMIT")?;
        drop(conn);

        let denylist = vec!["PAGE_(?:CONVERSATION|MESSAGE|SNIPPET)_SECRET".to_string()];
        let config = SecretScanConfig::from_inputs_with_env(&[], &denylist, false)?;
        let filters = SecretScanFilters {
            agents: Some(vec!["codex".to_string()]),
            workspaces: Some(vec![PathBuf::from("/tmp/project")]),
            since_ts: Some(1_699_999_999_999),
            until_ts: Some(1_700_000_000_001),
        };
        let report = scan_database(&db_path, &filters, &config, None, None)?;
        assert_eq!(report.summary.total, 3);
        assert!(!report.summary.truncated);

        for (location, expected_message_id, expected_message_idx) in [
            (SecretLocation::ConversationTitle, None, None),
            (SecretLocation::MessageContent, Some(-1), Some(0)),
            (SecretLocation::MessageSnippet, Some(-1), Some(0)),
        ] {
            let finding = report
                .findings
                .iter()
                .find(|finding| finding.location == location)
                .ok_or_else(|| anyhow::anyhow!("keyset paging missed {location:?}"))?;
            assert_eq!(finding.conversation_id, Some(-1));
            assert_eq!(finding.message_id, expected_message_id);
            assert_eq!(finding.message_idx, expected_message_idx);
            assert_eq!(
                finding.source_path.as_deref(),
                Some("/tmp/project/session--1.jsonl")
            );
        }
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
    fn finding_context_and_pattern_are_safe_for_adjacent_secret_classes() -> Result<()> {
        let temp = TempDir::new()?;
        let db_path = temp.path().join("scan.db");
        let focal = project_oai_fixture();
        let private_body = fixture(&[
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAA",
            "Bbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZWQyNTUxOQ",
        ]);
        let private_block = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{private_body}\n-----END OPENSSH PRIVATE KEY-----"
        );
        let denied_secret = "INTERNAL_SECRET_ABC123XYZ789";
        let entropy_secret =
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let content = format!(
            "safe prefix {focal} private {private_block} denied {denied_secret} entropy {entropy_secret} safe suffix"
        );
        setup_db(&db_path, &content)?;

        let raw_denylist = "INTERNAL_SECRET_[A-Z0-9]+".to_string();
        let mut config =
            SecretScanConfig::from_inputs_with_env(&[], std::slice::from_ref(&raw_denylist), false)?;
        config.context_bytes = content.len() * 2;
        let report = scan_database(&db_path, &no_filters(), &config, None, None)?;
        assert!(
            report.findings.iter().any(|finding| {
                finding.kind == "denylist" && finding.pattern == "custom_denylist"
            }),
            "custom denylist finding should use an opaque pattern identifier"
        );

        for finding in &report.findings {
            for raw_secret in [
                focal.as_str(),
                private_body.as_str(),
                denied_secret,
                entropy_secret,
            ] {
                assert!(
                    !finding.context.contains(raw_secret),
                    "{} context leaked adjacent secret {raw_secret:?}: {}",
                    finding.kind,
                    finding.context,
                );
            }
        }

        let serialized = serde_json::to_string(&report)?;
        assert!(
            !serialized.contains(&raw_denylist),
            "report serialized raw custom denylist regex"
        );
        assert!(!serialized.contains(&private_body));
        assert!(!serialized.contains(denied_secret));
        assert!(!serialized.contains(entropy_secret));
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
        // Bead 7k7pl: pin the SHAPE of source_path (non-empty string)
        // and conversation_id (positive i64). A regression that
        // emitted None-wrapped-empty or i64::MIN would slip past
        // `.is_some()` while breaking the link to a real row.
        let source_path = finding
            .source_path
            .as_deref()
            .expect("source_path must be set for a finding rooted in a stored session");
        assert!(
            !source_path.is_empty(),
            "source_path must be a non-empty string; got {:?}",
            finding.source_path
        );
        let conversation_id = finding
            .conversation_id
            .expect("conversation_id must be set for a finding rooted in a stored session");
        assert!(
            conversation_id > 0,
            "conversation_id must be a positive row id; got {}",
            conversation_id
        );
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
