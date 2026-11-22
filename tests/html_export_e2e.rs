//! E2E Tests for HTML Export Visual Validation
//!
//! Tests the complete export pipeline including visual structure,
//! CSS presence, JavaScript functionality, and accessibility.

use assert_cmd::Command;
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use tracing::{debug, info};

// ============================================================================
// Test Helpers
// ============================================================================

fn base_cmd() -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

/// Export a fixture file and return the HTML content.
fn export_fixture(fixture_name: &str) -> String {
    let fixture_path = Path::new("tests/fixtures/message_grouping").join(fixture_name);
    let temp_dir = TempDir::new().expect("create temp dir");

    info!(
        fixture = fixture_name,
        output_dir = %temp_dir.path().display(),
        "Exporting fixture to HTML"
    );

    let mut cmd = base_cmd();
    cmd.args([
        "export-html",
        fixture_path.to_str().unwrap(),
        "--output-dir",
        temp_dir.path().to_str().unwrap(),
        "--json",
    ]);

    let output = cmd.output().expect("run export command");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("Export failed for {}: {}", fixture_name, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("parse export JSON result");

    let output_path = json["exported"]["output_path"]
        .as_str()
        .expect("output_path in exported result");

    let html = fs::read_to_string(output_path).expect("read exported HTML");

    info!(
        fixture = fixture_name,
        size_bytes = html.len(),
        "Export complete"
    );

    html
}

/// Count occurrences of a pattern in HTML.
fn count_pattern(html: &str, pattern: &str) -> usize {
    let re = Regex::new(pattern).expect("valid regex");
    re.find_iter(html).count()
}

const EXPORT_PRIVACY_SECRET: &str = "synthetic-export-credential-a81b4e9d"; // ubs:ignore[rust.security.hardcoded-secrets] — Synthetic regression sentinel, never an authentication credential.

fn export_privacy_command(root: &Path) -> Command {
    let mut cmd = base_cmd();
    cmd.current_dir(root)
        .env("CASS_DATA_DIR", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_INDEX_REDACTION", "full")
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_IGNORE_SOURCES_CONFIG")
        .env_remove("CASS_EXCLUDE_PATHS")
        .env_remove("CASS_EXCLUDE_WORKSPACES")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .timeout(std::time::Duration::from_secs(60));
    cmd
}

fn export_privacy_session(root: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let config = root.join("config/cass");
    fs::create_dir_all(&config)?;
    fs::write(
        config.join("sources.toml"),
        "[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"fixture.invalid\"\npaths = [\"~/.codex/sessions\"]\nsync_schedule = \"manual\"\n",
    )?;
    // Reingest supplies an empty default-local-root map and reads only this
    // mirror. It neither discovers the operator's homes nor opens SSH.
    let mirror = root.join("data/remotes/fixture/mirror").join(
        coding_agent_search::sources::sync::path_to_safe_dirname("~/.codex/sessions"),
    );
    fs::create_dir_all(&mirror)?;
    let session = mirror.join("rollout-export-privacy.jsonl");
    let records = [
        serde_json::json!({"timestamp":"2026-10-27T12:00:00Z","type":"session_meta","payload":{"id":"export-privacy-84","cwd":"/synthetic/export-privacy","originator":"codex_cli_rs"}}),
        serde_json::json!({"timestamp":"2026-10-27T12:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("exportprivacycanary café 界 password={EXPORT_PRIVACY_SECRET}\nDB_PASSWORD={EXPORT_PRIVACY_SECRET}\nGITHUB_TOKEN={EXPORT_PRIVACY_SECRET}\nSTRIPE_SECRET={EXPORT_PRIVACY_SECRET}\nMY_API_KEY={EXPORT_PRIVACY_SECRET}")}]}}),
        serde_json::json!({"timestamp":"2026-10-27T12:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"exportprivacycanary ordinary-kept-marker; discuss password rotation"}]}}),
    ];
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &record)?;
        bytes.push(b'\n');
    }
    fs::write(&session, bytes)?;
    Ok(session)
}

fn export_privacy_html(
    root: &Path,
    session: &Path,
    source: Option<&str>,
    include_secrets: bool,
    encrypt: bool,
) -> Result<(Value, String), Box<dyn std::error::Error>> {
    let mut cmd = export_privacy_command(root);
    cmd.arg("export-html")
        .arg(session)
        .arg("--output-dir")
        .arg(root.join("exports"))
        .arg("--json");
    if let Some(source) = source {
        cmd.args(["--source", source]);
    }
    if include_secrets {
        cmd.arg("--include-secrets");
    }
    if encrypt {
        cmd.args(["--encrypt", "--password-stdin"])
            .write_stdin("export privacy regression passphrase\n");
    }
    let output = cmd.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(receipt["privacy"]["sensitive_output"], include_secrets);
    assert!(receipt["privacy"]["redaction_applied"].is_boolean());
    if include_secrets {
        assert_eq!(receipt["privacy"]["redaction_applied"], false);
    }
    assert_eq!(
        receipt["privacy"]["policy"],
        if include_secrets {
            "include-secrets"
        } else {
            "redact-secrets"
        }
    );
    let html = fs::read_to_string(
        receipt["exported"]["output_path"]
            .as_str()
            .ok_or("output path")?,
    )?;
    Ok((receipt, html))
}

#[test]
fn export_redaction_parity_for_live_and_archived_sources() -> Result<(), Box<dyn std::error::Error>>
{
    let root = TempDir::new()?;
    let session = export_privacy_session(root.path())?;
    let source_bytes = fs::read(&session)?;
    for plan_flag in ["--dry-run", "--explain"] {
        let output = export_privacy_command(root.path())
            .arg("export-html")
            .arg(&session)
            .args(["--json", plan_flag])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(plan["privacy"]["policy"], "redact-secrets");
        assert_eq!(plan["privacy"]["redaction_applied"], true);
        assert!(!plan.to_string().contains(EXPORT_PRIVACY_SECRET));
    }
    let indexed = export_privacy_command(root.path())
        .args([
            "sources", "reingest", "--source", "fixture", "--full", "--json",
        ])
        .output()?;
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed_receipt: Value = serde_json::from_slice(&indexed.stdout)?;
    assert_eq!(indexed_receipt["indexing"]["conversations"], 1);
    let query = export_privacy_command(root.path())
        .args([
            "search",
            "exportprivacycanary",
            "--mode",
            "lexical",
            "--no-maintenance",
            "--robot",
        ])
        .output()?;
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let hits: Value = serde_json::from_slice(&query.stdout)?;
    assert!(hits["hits"].as_array().is_some_and(|hits| !hits.is_empty()));
    assert!(!String::from_utf8_lossy(&query.stdout).contains(EXPORT_PRIVACY_SECRET));

    // Searching the credential itself catches a leak even if a positive
    // search's bounded content preview would omit the assignment's tail.
    let secret_phrase = format!("\"{EXPORT_PRIVACY_SECRET}\"");
    let secret_query = export_privacy_command(root.path())
        .args([
            "search",
            secret_phrase.as_str(),
            "--mode",
            "lexical",
            "--no-maintenance",
            "--robot",
        ])
        .output()?;
    assert!(
        secret_query.status.success(),
        "{}",
        String::from_utf8_lossy(&secret_query.stderr)
    );
    let secret_hits: Value = serde_json::from_slice(&secret_query.stdout)?;
    assert!(
        secret_hits["hits"].as_array().is_some_and(Vec::is_empty),
        "prefixed credential assignments remained searchable: {secret_hits}"
    );

    let db_path = root.path().join("data/agent_search.db");
    let db_before = fs::read(&db_path)?;
    let wal_path = root.path().join("data/agent_search.db-wal");
    let wal_before = fs::read(&wal_path).ok();
    for (label, source) in [("live", None), ("archive", Some("fixture"))] {
        for format in ["markdown", "text", "json", "html"] {
            let output_path = root.path().join(format!("{label}-{format}.out"));
            let mut cmd = export_privacy_command(root.path());
            cmd.arg("export")
                .arg(&session)
                .args(["--format", format, "--include-tools"])
                .arg("--output")
                .arg(&output_path);
            if let Some(source) = source {
                cmd.args(["--source", source]);
            }
            // Index-time configuration cannot opt a sharing command in.
            cmd.env("CASS_INDEX_REDACTION", "off");
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let exported = fs::read_to_string(output_path)?;
            assert!(
                !exported.contains(EXPORT_PRIVACY_SECRET),
                "{label}/{format} leaked a credential"
            );
            assert!(
                exported.contains("ordinary-kept-marker"),
                "{label}/{format} lost benign content"
            );
            assert!(exported.contains("password rotation"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("redact-secrets"));
        }
        let (receipt, html) = export_privacy_html(root.path(), &session, source, false, false)?;
        assert!(
            !html.contains(EXPORT_PRIVACY_SECRET),
            "{label}/HTML leaked a credential"
        );
        assert!(!receipt.to_string().contains(EXPORT_PRIVACY_SECRET));
        assert!(html.contains("ordinary-kept-marker"));
        assert!(html.contains("café 界"));
    }
    assert_eq!(fs::read(&session)?, source_bytes);
    assert_eq!(fs::read(&db_path)?, db_before);
    assert_eq!(fs::read(&wal_path).ok(), wal_before);
    Ok(())
}

#[test]
fn export_redaction_covers_tool_arguments_results_and_nested_metadata()
-> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let session = root.path().join("tool-session.jsonl");
    let events = [
        serde_json::json!({"type":"user","message":{"role":"user","content":format!("ordinary-kept-marker café 界\nDB_PASSWORD={EXPORT_PRIVACY_SECRET}")}}),
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"call-84","name":"Read","input":{"path":"src/lib.rs","authorization":EXPORT_PRIVACY_SECRET}}
        ]},"metadata":{"nested":{"passphrase":EXPORT_PRIVACY_SECRET},"token_count":42}}),
        serde_json::json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call-84","content":format!("tool-result-kept password={EXPORT_PRIVACY_SECRET} <script>synthetic_tool_markup()</script>")}
        ]}}),
    ];
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, &event)?;
        bytes.push(b'\n');
    }
    fs::write(&session, &bytes)?;
    for include_secrets in [false, true] {
        for format in ["markdown", "text", "json", "html"] {
            let mut cmd = export_privacy_command(root.path());
            cmd.arg("export")
                .arg(&session)
                .args(["--format", format, "--include-tools"]);
            if include_secrets {
                cmd.arg("--include-secrets");
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let exported = String::from_utf8(output.stdout)?;
            assert_eq!(
                exported.contains(EXPORT_PRIVACY_SECRET),
                include_secrets,
                "{format}, include_secrets={include_secrets}"
            );
            assert!(
                exported.contains("tool-result-kept"),
                "{format}, include_secrets={include_secrets} lost tool output"
            );
            if format == "html" {
                assert!(!exported.contains("<script>synthetic_tool_markup()</script>"));
                assert!(exported.contains("&lt;script&gt;synthetic_tool_markup()&lt;/script&gt;"));
            }
            if format == "json" {
                let messages: Value = serde_json::from_str(&exported)?;
                assert_eq!(messages[1]["metadata"]["token_count"], 42);
                assert_eq!(messages[1]["message"]["content"][0]["id"], "call-84");
            }
        }
        let (_, html) = export_privacy_html(root.path(), &session, None, include_secrets, false)?;
        assert_eq!(html.contains(EXPORT_PRIVACY_SECRET), include_secrets);
        assert!(html.contains("tool-result-kept"));
        assert!(html.contains("src/lib.rs"));
        assert!(!html.contains("<script>synthetic_tool_markup()</script>"));
    }
    for format in ["markdown", "text", "html"] {
        let output = export_privacy_command(root.path())
            .arg("export")
            .arg(&session)
            .args(["--format", format])
            .output()?;
        assert!(output.status.success());
        assert!(
            !String::from_utf8(output.stdout)?.contains("tool-result-kept"),
            "{format} must require --include-tools for structured results"
        );
    }
    assert_eq!(fs::read(&session)?, bytes);
    Ok(())
}

#[test]
fn export_redaction_covers_multiline_and_overlapping_credential_values()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-multiline-export-privacy-")
        .disable_cleanup(true)
        .tempdir()?;
    let cases = [
        (
            "double-lf",
            format!(
                "DB_PASSWORD=\"synthetic-first-piece\n{EXPORT_PRIVACY_SECRET}\"\nmultiline-suffix-kept"
            ),
            EXPORT_PRIVACY_SECRET,
            "[REDACTED]\nmultiline-suffix-kept",
        ),
        (
            "single-crlf",
            format!(
                "AWS_SESSION_TOKEN='synthetic-first-piece\r\n{EXPORT_PRIVACY_SECRET}'\r\nmultiline-suffix-kept"
            ),
            EXPORT_PRIVACY_SECRET,
            "[REDACTED]\r\nmultiline-suffix-kept",
        ),
        (
            "aws-prefix",
            format!(
                "AWS_SECRET_ACCESS_KEY=\"{}\n{EXPORT_PRIVACY_SECRET}\"\nmultiline-suffix-kept",
                "A".repeat(40)
            ),
            EXPORT_PRIVACY_SECRET,
            "[REDACTED]\nmultiline-suffix-kept",
        ),
        (
            "compact-overlap",
            "awsSecretAccessKey=\"AKIAABCDEFGHIJKLMNOP abcdefghijklmnopqrst\nTAIL\"\nmultiline-suffix-kept"
                .to_owned(),
            "abcdefghijklmnopqrst",
            "[REDACTED]\nmultiline-suffix-kept",
        ),
    ];
    for (case, payload, sentinel, expected) in cases {
        let session = root.path().join(format!("multiline-{case}.jsonl"));
        let events = [
            serde_json::json!({"type":"user","message":{"role":"user","content":"multiline-context-kept café 界"}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"call-multiline","name":"read_config","input":{"path":"fixture.env"}}
            ]}}),
            serde_json::json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call-multiline","content":payload}
            ]}}),
        ];
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, &event)?;
            bytes.push(b'\n');
        }
        fs::write(&session, &bytes)?;
        for include_secrets in [false, true] {
            for format in ["markdown", "text", "json", "html"] {
                let mut cmd = export_privacy_command(root.path());
                cmd.arg("export")
                    .arg(&session)
                    .args(["--format", format, "--include-tools"]);
                if include_secrets {
                    cmd.arg("--include-secrets");
                }
                let output = cmd.output()?;
                assert!(output.status.success(), "{case}/{format}: {output:?}");
                let exported = String::from_utf8(output.stdout)?;
                assert_eq!(
                    exported.contains(sentinel),
                    include_secrets,
                    "{case}/{format}"
                );
                assert!(exported.contains("multiline-context-kept"));
                assert!(exported.contains("multiline-suffix-kept"));
                let stderr = String::from_utf8(output.stderr)?;
                let announcement = stderr
                    .lines()
                    .find_map(|line| line.strip_prefix("note: export privacy: "))
                    .ok_or("export privacy announcement missing")?;
                let privacy: Value = serde_json::from_str(announcement)?;
                assert_eq!(privacy["redaction_applied"], !include_secrets);
                assert_eq!(privacy["sensitive_output"], include_secrets);
                if format == "json" {
                    let messages: Value = serde_json::from_str(&exported)?;
                    assert_eq!(messages[1]["message"]["content"][0]["id"], "call-multiline");
                    assert_eq!(
                        messages[2]["message"]["content"][0]["content"].as_str(),
                        Some(if include_secrets {
                            payload.as_str()
                        } else {
                            expected
                        })
                    );
                }
            }
            let (receipt, html) =
                export_privacy_html(root.path(), &session, None, include_secrets, false)?;
            assert_eq!(html.contains(sentinel), include_secrets, "{case}/export-html");
            assert!(html.contains("multiline-context-kept"));
            assert!(html.contains("multiline-suffix-kept"));
            assert_eq!(receipt["privacy"]["redaction_applied"], !include_secrets);
        }
        assert_eq!(
            fs::read(&session)?,
            bytes,
            "export changed the source session"
        );
    }
    Ok(())
}

#[test]
fn gh551_export_redaction_covers_json_titles() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-gh551-json-title-")
        .disable_cleanup(true)
        .tempdir()?;
    let storage = root.path().join("storage");
    let sessions = storage.join("session/project-551");
    let messages = storage.join("message/session-title-551");
    let parts = storage.join("part/message-551");
    for directory in [&sessions, &messages, &parts] {
        fs::create_dir_all(directory)?;
    }
    let session = sessions.join("session-title-551.json");
    let message = messages.join("message-551.json");
    let part = parts.join("part-551.json");
    fs::write(
        &message,
        serde_json::to_vec(&serde_json::json!({
            "id":"message-551", "sessionID":"session-title-551", "role":"user",
            "time":{"created":1733000001000_i64}
        }))?,
    )?;
    fs::write(
        &part,
        serde_json::to_vec(&serde_json::json!({
            "id":"part-551", "messageID":"message-551", "type":"text",
            "text":"export-title-clean-body café 界"
        }))?,
    )?;
    // Only the title contains a credential: message redaction cannot make the
    // receipt pass while the title still leaks. Use a real OpenCode layout.
    let structured_title = serde_json::json!({
        "registration_token":EXPORT_PRIVACY_SECRET,
        "note":"title-kept café 界",
        "token_count":42
    })
    .to_string();
    for wrappers in 0..=2 {
        let mut title = structured_title.clone();
        for _ in 0..wrappers {
            title = serde_json::to_string(&title)?;
        }
        fs::write(
            &session,
            serde_json::to_vec(&serde_json::json!({
                "id":"session-title-551", "title":title,
                "time":{"created":1733000000000_i64, "updated":1733000002000_i64}
            }))?,
        )?;
        let originals = [
            (&session, fs::read(&session)?),
            (&message, fs::read(&message)?),
            (&part, fs::read(&part)?),
        ];
        for include_secrets in [false, true] {
            for format in ["markdown", "text", "json", "html"] {
                let mut command = export_privacy_command(root.path());
                command
                    .arg("export")
                    .arg(&session)
                    .args(["--format", format]);
                if include_secrets {
                    command.arg("--include-secrets");
                }
                let output = command.output()?;
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let exported = String::from_utf8(output.stdout)?;
                // Text and JSON export contain only messages; Markdown and
                // HTML also render the session title.
                let renders_title = matches!(format, "markdown" | "html");
                assert_eq!(
                    exported.contains(EXPORT_PRIVACY_SECRET),
                    include_secrets && renders_title,
                    "{format}, wrappers={wrappers}, include_secrets={include_secrets}"
                );
                assert!(exported.contains("export-title-clean-body"));
                if renders_title {
                    assert!(exported.contains("title-kept"));
                }
                let stderr = String::from_utf8(output.stderr)?;
                let announcement = stderr
                    .lines()
                    .find_map(|line| line.strip_prefix("note: export privacy: "))
                    .ok_or("export privacy announcement missing")?;
                let privacy: Value = serde_json::from_str(announcement)?;
                assert_eq!(privacy["redaction_applied"], !include_secrets);
                assert_eq!(privacy["sensitive_output"], include_secrets);
            }
            let (receipt, html) =
                export_privacy_html(root.path(), &session, None, include_secrets, false)?;
            assert_eq!(html.contains(EXPORT_PRIVACY_SECRET), include_secrets);
            assert!(html.contains("title-kept"));
            assert!(html.contains("export-title-clean-body"));
            assert_eq!(receipt["privacy"]["redaction_applied"], !include_secrets);
        }
        for (path, original) in originals {
            assert_eq!(
                fs::read(path)?,
                original,
                "export changed {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[test]
fn gh551_export_redaction_covers_embedded_json_tool_results()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-gh551-quoted-json-")
        .disable_cleanup(true)
        .tempdir()?;
    let session = root.path().join("embedded-json-tool-session.jsonl");
    // The only credentials are fields inside this JSON-encoded string. Plain
    // assignment redaction elsewhere must not make the privacy receipt pass.
    let tool_result = serde_json::json!({
        "ok": true,
        "registration_token": EXPORT_PRIVACY_SECRET,
        "note": "tool-json-kept café 界",
        "token_count": 42,
        "records": [{"passphrase": "tiny", "enabled": true}]
    })
    .to_string();
    for wrappers in 0..=2 {
        let mut payload = tool_result.clone();
        for _ in 0..wrappers {
            payload = serde_json::to_string(&payload)?;
        }
        let events = [
            serde_json::json!({"type":"user","message":{"role":"user","content":"Register this agent"}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"call-551","name":"register_agent","input":{"agent":"DemoAgent"}}
            ]}}),
            serde_json::json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call-551","content":payload}
            ]}}),
        ];
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, &event)?;
            bytes.push(b'\n');
        }
        fs::write(&session, &bytes)?;

        for include_secrets in [false, true] {
            for format in ["markdown", "text", "json", "html"] {
                let mut cmd = export_privacy_command(root.path());
                cmd.arg("export")
                    .arg(&session)
                    .args(["--format", format, "--include-tools"]);
                if include_secrets {
                    cmd.arg("--include-secrets");
                }
                let output = cmd.output()?;
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let exported = String::from_utf8(output.stdout)?;
                assert_eq!(
                    exported.contains(EXPORT_PRIVACY_SECRET),
                    include_secrets,
                    "{format}, include_secrets={include_secrets}"
                );
                assert!(exported.contains("tool-json-kept"));
                let stderr = String::from_utf8(output.stderr)?;
                let announcement = stderr
                    .lines()
                    .find_map(|line| line.strip_prefix("note: export privacy: "))
                    .ok_or("export privacy announcement missing")?;
                let privacy: Value = serde_json::from_str(announcement)?;
                assert_eq!(privacy["redaction_applied"], !include_secrets);
                assert_eq!(privacy["sensitive_output"], include_secrets);

                if format == "json" {
                    let messages: Value = serde_json::from_str(&exported)?;
                    assert_eq!(messages[1]["message"]["content"][0]["id"], "call-551");
                    let embedded = messages[2]["message"]["content"][0]["content"]
                        .as_str()
                        .ok_or("tool result changed JSON type")?;
                    let mut decoded = embedded.to_owned();
                    for _ in 0..wrappers {
                        decoded = serde_json::from_str::<String>(&decoded)?;
                    }
                    let result: Value = serde_json::from_str(&decoded)?;
                    assert_eq!(result["ok"], true);
                    assert_eq!(result["note"], "tool-json-kept café 界");
                    assert_eq!(result["token_count"], 42);
                    assert_eq!(result["records"][0]["enabled"], true);
                    assert_eq!(
                        result["registration_token"],
                        if include_secrets {
                            EXPORT_PRIVACY_SECRET
                        } else {
                            "[REDACTED]"
                        }
                    );
                    assert_eq!(
                        result["records"][0]["passphrase"],
                        if include_secrets {
                            "tiny"
                        } else {
                            "[REDACTED]"
                        }
                    );
                }
            }
            let (receipt, html) =
                export_privacy_html(root.path(), &session, None, include_secrets, false)?;
            assert_eq!(html.contains(EXPORT_PRIVACY_SECRET), include_secrets);
            assert!(html.contains("tool-json-kept"));
            assert_eq!(receipt["privacy"]["redaction_applied"], !include_secrets);
        }
        assert_eq!(
            fs::read(&session)?,
            bytes,
            "export changed the source session"
        );
    }
    Ok(())
}

#[test]
fn gh551_truncated_json_tool_results_keep_export_privacy() -> Result<(), Box<dyn std::error::Error>>
{
    let root = tempfile::Builder::new()
        .prefix("cass-gh551-truncated-json-")
        .disable_cleanup(true)
        .tempdir()?;
    let session = root.path().join("truncated-json-tool-session.jsonl");
    let complete = serde_json::json!({"registration_token": EXPORT_PRIVACY_SECRET}).to_string();
    let payloads = [
        complete
            .strip_suffix('}')
            .ok_or("object terminator missing")?
            .to_owned(),
        format!("[{complete}"),
        format!(r#"{{"registration_token":"{EXPORT_PRIVACY_SECRET}"#),
    ];
    for payload in payloads {
        // The physical JSONL records are intact. Only the JSON encoded inside
        // the tool-result string is truncated, as with bounded tool output.
        let events = [
            serde_json::json!({"type":"user","message":{"role":"user","content":"truncated-json-context-kept café 界"}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"call-551-truncated","name":"register_agent","input":{"agent":"DemoAgent"}}
            ]}}),
            serde_json::json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call-551-truncated","content":payload}
            ]}}),
        ];
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, &event)?;
            bytes.push(b'\n');
        }
        fs::write(&session, &bytes)?;

        for include_secrets in [false, true] {
            for format in ["markdown", "text", "json", "html"] {
                let mut cmd = export_privacy_command(root.path());
                cmd.arg("export")
                    .arg(&session)
                    .args(["--format", format, "--include-tools"]);
                if include_secrets {
                    cmd.arg("--include-secrets");
                }
                let output = cmd.output()?;
                assert!(output.status.success(), "{format}: {output:?}");
                let exported = String::from_utf8(output.stdout)?;
                assert_eq!(
                    exported.contains(EXPORT_PRIVACY_SECRET),
                    include_secrets,
                    "{format}, include_secrets={include_secrets}, payload={payload:?}"
                );
                assert!(exported.contains("truncated-json-context-kept"));
                let stderr = String::from_utf8(output.stderr)?;
                let announcement = stderr
                    .lines()
                    .find_map(|line| line.strip_prefix("note: export privacy: "))
                    .ok_or("export privacy announcement missing")?;
                let privacy: Value = serde_json::from_str(announcement)?;
                assert_eq!(privacy["redaction_applied"], !include_secrets);
                assert_eq!(privacy["sensitive_output"], include_secrets);
                if format == "json" {
                    let messages: Value = serde_json::from_str(&exported)?;
                    assert_eq!(
                        messages[1]["message"]["content"][0]["id"],
                        "call-551-truncated"
                    );
                    assert_eq!(
                        messages[2]["message"]["content"][0]["content"].as_str(),
                        Some(if include_secrets {
                            payload.as_str()
                        } else {
                            "[REDACTED]"
                        })
                    );
                }
            }
            let (receipt, html) =
                export_privacy_html(root.path(), &session, None, include_secrets, false)?;
            assert_eq!(html.contains(EXPORT_PRIVACY_SECRET), include_secrets);
            assert!(html.contains("truncated-json-context-kept"));
            assert_eq!(receipt["privacy"]["redaction_applied"], !include_secrets);
        }
        assert_eq!(
            fs::read(&session)?,
            bytes,
            "export changed the source session"
        );
    }
    Ok(())
}

#[test]
fn export_redaction_covers_unredacted_archive_with_never_created_source()
-> Result<(), Box<dyn std::error::Error>> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::SqliteStorage;

    let root = TempDir::new()?;
    let absent = root.path().join("never-created.jsonl");
    let db_path = root.path().join("data/agent_search.db");
    fs::create_dir_all(db_path.parent().ok_or("database directory")?)?;
    let storage = SqliteStorage::open(&db_path)?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    // This archive deliberately predates ingest redaction. Its source path has
    // never existed; fallback is exercised without deleting or moving a file.
    let conversation = Conversation {
        id: None,
        agent_slug: "codex".into(),
        workspace: None,
        external_id: Some("export-privacy-archive-only".into()),
        title: Some(format!("password={EXPORT_PRIVACY_SECRET}")),
        source_path: absent.clone(),
        started_at: None,
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![
            Message {
                id: None,
                idx: 7,
                role: MessageRole::User,
                author: None,
                created_at: None,
                content: format!("café 界 password={EXPORT_PRIVACY_SECRET}"),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 8,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: "ARCHIVED_PRIVATE_NARRATIVE".into(),
                extra_json: serde_json::json!({
                    "type": "message",
                    "message": {"role": "assistant", "content": "ARCHIVED_PRIVATE_NARRATIVE"},
                    "metadata": {"source": "Base directory for this skill: /synthetic/archive"}
                }),
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 9,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: "ordinary-kept-marker; discuss password rotation".into(),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            },
        ],
        source_id: "local".into(),
        origin_host: None,
    };
    storage.insert_conversation_tree(agent_id, None, &conversation)?;
    let title_source = root.path().join("never-created-title.jsonl");
    let mut titled = conversation.clone();
    titled.source_path = title_source.clone();
    titled.external_id = Some("archived-skill-derived-title".into());
    titled.title = Some("ARCHIVED_PRIVATE…".into());
    titled
        .messages
        .first_mut()
        .ok_or("no archive prompt")?
        .content = "ARCHIVED_TITLE_ORDINARY café 界".into();
    storage.insert_conversation_tree(agent_id, None, &titled)?;
    drop(storage);
    let db_before = fs::read(&db_path)?;
    let wal_path = root.path().join("data/agent_search.db-wal");
    let wal_before = fs::read(&wal_path).ok();
    for include_secrets in [false, true] {
        for format in ["markdown", "text", "json", "html"] {
            let mut cmd = export_privacy_command(root.path());
            cmd.arg("export")
                .arg(&absent)
                .args(["--source", "local", "--format", format]);
            if include_secrets {
                cmd.arg("--include-secrets");
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let exported = String::from_utf8(output.stdout)?;
            assert_eq!(exported.contains(EXPORT_PRIVACY_SECRET), include_secrets);
            assert!(!exported.contains("ARCHIVED_PRIVATE_NARRATIVE"), "{format}");
            assert!(exported.contains("ordinary-kept-marker"));
            assert!(exported.contains("café 界"));
        }
        let (receipt, html) =
            export_privacy_html(root.path(), &absent, Some("local"), include_secrets, false)?;
        assert_eq!(html.contains(EXPORT_PRIVACY_SECRET), include_secrets);
        assert_eq!(
            receipt.to_string().contains(EXPORT_PRIVACY_SECRET),
            include_secrets
        );
        assert!(html.contains("ordinary-kept-marker"));
        assert!(html.contains("café 界"));
        assert!(!html.contains("ARCHIVED_PRIVATE_NARRATIVE"));
    }
    let output = export_privacy_command(root.path())
        .arg("export")
        .arg(&absent)
        .args(["--source", "local", "--format", "json", "--include-skills"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported = String::from_utf8(output.stdout)?;
    assert!(exported.contains("ARCHIVED_PRIVATE_NARRATIVE"));
    assert!(!exported.contains(EXPORT_PRIVACY_SECRET));
    assert!(!exported.contains("/synthetic/archive"));
    for include_skills in [false, true] {
        for markdown in [false, true] {
            let mut cmd = export_privacy_command(root.path());
            if markdown {
                cmd.arg("export")
                    .arg(&title_source)
                    .args(["--format", "markdown"]);
            } else {
                cmd.arg("export-html")
                    .arg(&title_source)
                    .arg("--output-dir")
                    .arg(root.path().join("title-exports"))
                    .arg("--json");
            }
            cmd.args(["--source", "local"]);
            if include_skills {
                cmd.arg("--include-skills");
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = if markdown {
                String::from_utf8(output.stdout)?
            } else {
                let receipt: Value = serde_json::from_slice(&output.stdout)?;
                let path = Path::new(
                    receipt["exported"]["output_path"]
                        .as_str()
                        .ok_or("output path missing")?,
                );
                let name = path
                    .file_name()
                    .ok_or("filename missing")?
                    .to_string_lossy()
                    .to_ascii_lowercase();
                assert_eq!(name.contains("private"), include_skills, "{name}");
                fs::read_to_string(path)?
            };
            let title = if include_skills {
                "ARCHIVED_PRIVATE…"
            } else {
                "ARCHIVED_TITLE_ORDINARY café 界"
            };
            if markdown {
                assert!(rendered.starts_with(&format!("# {title}\n")));
            } else {
                assert!(rendered.contains(&format!("<title>{title}</title>")));
            }
            assert_eq!(rendered.contains("ARCHIVED_PRIVATE"), include_skills);
            assert!(rendered.contains("ordinary-kept-marker"));
        }
    }
    assert!(!title_source.exists());
    assert!(!absent.exists());
    assert_eq!(fs::read(&db_path)?, db_before);
    assert_eq!(fs::read(&wal_path).ok(), wal_before);
    Ok(())
}

#[test]
fn nd6pc_raw_and_indexed_export_privacy_flags_are_independent()
-> Result<(), Box<dyn std::error::Error>> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::SqliteStorage;

    let root = tempfile::Builder::new()
        .prefix("cass-export-injection-opt-ins-")
        .disable_cleanup(true)
        .tempdir()?;
    let raw = root.path().join("direct.jsonl");
    let indexed = root.path().join("indexed-only.jsonl");
    let db_path = root.path().join("data/agent_search.db");
    fs::create_dir_all(root.path().join("data"))?;
    let records = [
        serde_json::json!({"role":"user", "content":"SKILLBODYPRIVATE\nBase directory for this skill: /home/private-user/.claude/skills/example-skill/"}),
        serde_json::json!({"role":"user", "content":"HOOKBODYPRIVATE", "metadata":{"skillInjection":{"matchedSkills":["review"]}}}),
        serde_json::json!({"role":"user", "content":"Base directory for this skill: C:\\private-user\\mixed-skill\n<system-reminder>MIXEDBODYPRIVATE</system-reminder>"}),
        serde_json::json!({"role":"user", "content":"Ordinary prompt café 界"}),
    ];
    let mut bytes = Vec::new();
    for record in &records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    fs::write(&raw, &bytes)?;

    let storage = SqliteStorage::open(&db_path)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let indexed_messages = records
        .iter()
        .enumerate()
        .map(|(index, record)| Message {
            id: None,
            idx: index as i64,
            role: MessageRole::User,
            author: None,
            created_at: None,
            content: record["content"].as_str().unwrap_or_default().into(),
            extra_json: if record.get("metadata").is_some() {
                serde_json::json!({"source_line": record.to_string()})
            } else {
                record.clone()
            },
            snippets: Vec::new(),
        })
        .collect();
    storage.insert_conversation_tree(
        agent,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("injection-independence".into()),
            title: Some("HOOKBODYPRIVATE…".into()),
            source_path: indexed.clone(),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({}),
            messages: indexed_messages,
            source_id: "local".into(),
            origin_host: None,
        },
    )?;
    drop(storage);
    let db_before = fs::read(&db_path)?;
    let wal_path = db_path.with_extension("db-wal");
    let wal_before = fs::read(&wal_path).ok();
    for (path, archive_only) in [(&raw, false), (&indexed, true)] {
        for (skills, reminders) in [(false, false), (true, false), (false, true), (true, true)] {
            for format in ["markdown", "json", "export-html"] {
                let mut cmd = export_privacy_command(root.path());
                if format == "export-html" {
                    cmd.arg("export-html")
                        .arg(path)
                        .arg("--output-dir")
                        .arg(root.path().join("exports"))
                        .arg("--json");
                } else {
                    cmd.arg("export").arg(path).args(["--format", format]);
                }
                if archive_only {
                    cmd.args(["--source", "local"]);
                }
                if skills {
                    cmd.arg("--include-skills");
                }
                if reminders {
                    cmd.arg("--include-system-reminders");
                }
                let output = cmd.output()?;
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let rendered = if format == "export-html" {
                    let receipt: Value = serde_json::from_slice(&output.stdout)?;
                    let out = Path::new(
                        receipt["exported"]["output_path"]
                            .as_str()
                            .ok_or("export path")?,
                    );
                    let name = out
                        .file_name()
                        .ok_or("filename")?
                        .to_string_lossy()
                        .to_ascii_lowercase();
                    assert_eq!(
                        name.contains("hookbodyprivate"),
                        reminders && (archive_only || !skills)
                    );
                    fs::read_to_string(out)?
                } else {
                    String::from_utf8(output.stdout)?
                };
                assert!(rendered.contains("Ordinary prompt café 界"));
                assert_eq!(
                    rendered.contains("SKILLBODYPRIVATE"),
                    skills,
                    "{archive_only} {format}"
                );
                assert_eq!(
                    rendered.contains("HOOKBODYPRIVATE"),
                    reminders,
                    "{archive_only} {format}"
                );
                assert_eq!(rendered.contains("MIXEDBODYPRIVATE"), skills && reminders);
                assert_eq!(rendered.contains("[Skill loaded: example-skill]"), !skills);
                assert_eq!(
                    rendered.contains("[Skill loaded: mixed-skill]"),
                    !skills && reminders
                );
                assert_eq!(rendered.contains("/home/private-user/"), skills);
            }
        }
    }
    assert_eq!(fs::read(raw)?, bytes);
    assert!(!indexed.exists());
    assert_eq!(fs::read(db_path)?, db_before);
    assert_eq!(fs::read(wal_path).ok(), wal_before);
    Ok(())
}

#[test]
fn nd6pc_export_preserves_single_blocks_and_tool_visibility()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-export-single-text-block-")
        .disable_cleanup(true)
        .tempdir()?;
    let session = root.path().join("session.jsonl");
    let records = [
        serde_json::json!({"role":"user","content":"ORDINARY_STRING"}),
        serde_json::json!({"type":"user","message":{"role":"user","content":{
            "type":"text","text":"Base directory for this skill: /private/synthetic/object-skill\nPRIVATE_OBJECT_BODY"
        }}}),
        serde_json::json!({"type":"response_item","payload":{"role":"assistant","content":{
            "type":"output_text","text":"ORDINARY_CODEX_OBJECT café 界"
        }}}),
        serde_json::json!({"role":"user","content":{"type":"input_text","text":"ORDINARY_INPUT_OBJECT"}}),
        serde_json::json!({"role":"assistant","content":{"text":"ORDINARY_UNTYPED_OBJECT"}}),
        serde_json::json!({"role":"assistant","content":{"type":"tool_result","text":"TOOL_OBJECT_CANARY"}}),
        serde_json::json!({"role":"assistant","content":[
            {"type":"text","text":"ORDINARY_MIXED_ARRAY"},
            {"type":"tool_use","id":"array","name":"Read","input":{"description":"ARRAY_TOOL_DETAIL"}}
        ]}),
        serde_json::json!({"type":"response_item","payload":{"role":"assistant","content":{
            "type":"tool_use","id":"scalar","name":"ScalarRead","input":{"query":"example"}
        }}}),
        serde_json::json!({"type":"user","message":{"role":"user","content":{
            "type":"tool_result","tool_use_id":"scalar","content":"SCALAR_TOOL_RESULT","is_error":true
        }}}),
        serde_json::json!({"role":"tool","content":"STANDALONE_TOOL_DETAIL"}),
    ];
    let mut bytes = Vec::new();
    for record in &records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    fs::write(&session, &bytes)?;

    for format in ["markdown", "text", "html", "export-html"] {
        for (include_skills, include_tools) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let mut cmd = export_privacy_command(root.path());
            if format == "export-html" {
                cmd.arg("export-html")
                    .arg(&session)
                    .arg("--output-dir")
                    .arg(
                        root.path()
                            .join(format!("output-{include_skills}-{include_tools}")),
                    )
                    .arg("--json")
                    .arg(format!("--include-tools={include_tools}"));
            } else {
                cmd.arg("export").arg(&session).args(["--format", format]);
                if include_tools {
                    cmd.arg("--include-tools");
                }
            }
            if include_skills {
                cmd.arg("--include-skills");
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{format}, skills={include_skills}, tools={include_tools}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = if format == "export-html" {
                let receipt: Value = serde_json::from_slice(&output.stdout)?;
                fs::read_to_string(
                    receipt["exported"]["output_path"]
                        .as_str()
                        .ok_or("output path")?,
                )?
            } else {
                String::from_utf8(output.stdout)?
            };
            for expected in [
                "ORDINARY_STRING",
                "ORDINARY_CODEX_OBJECT café 界",
                "ORDINARY_INPUT_OBJECT",
                "ORDINARY_UNTYPED_OBJECT",
                "ORDINARY_MIXED_ARRAY",
            ] {
                assert!(rendered.contains(expected), "{format}: missing {expected}");
            }
            assert!(!rendered.contains("TOOL_OBJECT_CANARY"), "{format}");
            for detail in [
                "ARRAY_TOOL_DETAIL",
                "SCALAR_TOOL_RESULT",
                "STANDALONE_TOOL_DETAIL",
                "ScalarRead",
            ] {
                assert_eq!(
                    rendered.contains(detail),
                    include_tools,
                    "{format}: {detail}"
                );
            }
            if format == "export-html" && include_tools {
                assert!(rendered.contains(r#"aria-label="ScalarRead: error""#));
                let prompts = if include_skills {
                    "3 prompts"
                } else {
                    "2 prompts"
                };
                assert!(rendered.contains(&format!("<span>{prompts}, ")));
                assert!(rendered.contains("2 tool uses</span>"));
            }
            assert_eq!(
                rendered.contains("PRIVATE_OBJECT_BODY"),
                include_skills,
                "{format}"
            );
            assert_eq!(
                rendered.contains("/private/synthetic/"),
                include_skills,
                "{format}"
            );
            assert_eq!(
                rendered.contains("[Skill loaded: object-skill]"),
                !include_skills,
                "{format}"
            );
        }
    }
    assert_eq!(fs::read(session)?, bytes);
    Ok(())
}

#[test]
fn nd6pc_indexed_pi_tool_visibility_applies_to_narrative_and_titles()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-indexed-pi-export-tools-")
        .disable_cleanup(true)
        .tempdir()?;
    let pi_home = root.path().join(".pi/agent");
    let source = pi_home.join("sessions/workspace/2026-01-01T00-00-00_export.jsonl");
    fs::create_dir_all(source.parent().ok_or("Pi session directory")?)?;
    let records = [
        serde_json::json!({
            "type":"session", "version":3, "id":"export-tools", "cwd":"/synthetic/workspace",
            "timestamp":"2026-01-01T00:00:00.000Z"
        }),
        serde_json::json!({
            "type":"message", "id":"first", "parentId":null,
            "timestamp":"2026-01-01T00:00:01.000Z", "message":{"role":"assistant","content":[
                {"type":"toolCall","id":"call","name":"TitleLeak","arguments":{
                    "input":"PRIVATE_TOOL_ARGUMENT\nPRIVATE_TOOL_CONTINUATION"
                }},
                {"type":"text","text":"Ordinary narrative café 界\n[Tool: literal discussion] KEEP_LITERAL"}
            ]}
        }),
    ];
    let mut bytes = Vec::new();
    for record in &records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    fs::write(&source, &bytes)?;

    let data = root.path().join("data");
    let command = || {
        let mut cmd = base_cmd();
        cmd.env_clear()
            .current_dir(root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(
                "SYSTEMROOT",
                std::env::var_os("SYSTEMROOT").unwrap_or_default(),
            )
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("PI_CODING_AGENT_DIR", &pi_home)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("local-share"))
            .env("CASS_DATA_DIR", &data)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .timeout(std::time::Duration::from_secs(120));
        cmd
    };
    let index = command().args(["index", "--full", "--json"]).output()?;
    assert!(
        index.status.success(),
        "{}",
        String::from_utf8_lossy(&index.stderr)
    );
    let receipt: Value = serde_json::from_slice(&index.stdout)?;
    assert_eq!(receipt["indexing_stats"]["total_messages"], 1);
    let retained = root.path().join("retained-session.jsonl");
    fs::rename(&source, &retained)?;
    let db = data.join("agent_search.db");
    let db_before = fs::read(&db)?;
    let wal = data.join("agent_search.db-wal");
    let wal_before = fs::read(&wal).ok();

    for format in ["markdown", "text", "html", "json", "export-html"] {
        for include_tools in [false, true] {
            let mut cmd = command();
            if format == "export-html" {
                cmd.arg(format)
                    .arg(&source)
                    .arg("--output-dir")
                    .arg(root.path().join(format!("html-{include_tools}")))
                    .args(["--source", "local", "--json"])
                    .arg(format!("--include-tools={include_tools}"));
            } else {
                cmd.arg("export")
                    .arg(&source)
                    .args(["--source", "local", "--format", format]);
                if include_tools {
                    cmd.arg("--include-tools");
                }
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{format}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = if format == "export-html" {
                let receipt: Value = serde_json::from_slice(&output.stdout)?;
                let path = Path::new(
                    receipt["exported"]["output_path"]
                        .as_str()
                        .ok_or("output path")?,
                );
                assert_eq!(
                    path.file_name()
                        .ok_or("output filename")?
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("titleleak"),
                    include_tools,
                );
                fs::read_to_string(path)?
            } else {
                String::from_utf8(output.stdout)?
            };
            assert!(rendered.contains("Ordinary narrative café 界"), "{format}");
            assert!(
                rendered.contains("[Tool: literal discussion] KEEP_LITERAL"),
                "{format}"
            );
            for detail in [
                "TitleLeak",
                "PRIVATE_TOOL_ARGUMENT",
                "PRIVATE_TOOL_CONTINUATION",
            ] {
                assert_eq!(
                    rendered.contains(detail),
                    include_tools || format == "json",
                    "{format}: {detail}"
                );
            }
        }
    }
    assert_eq!(fs::read(retained)?, bytes);
    assert!(!source.exists());
    assert_eq!(fs::read(db)?, db_before);
    assert_eq!(fs::read(wal).ok(), wal_before);
    Ok(())
}

#[test]
fn nd6pc_html_omits_an_all_filtered_session_without_writing_an_artifact()
-> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let session = root.path().join("reminder.jsonl");
    let bytes =
        br#"{"role":"user","content":"<system-reminder>ONLY_PRIVATE_REMINDER</system-reminder>"}
"#;
    fs::write(&session, bytes)?;
    let directory = root.path().join("exports");
    for option in [
        None,
        Some("--dry-run"),
        Some("--explain"),
        Some("--include-skills"),
    ] {
        let mut cmd = export_privacy_command(root.path());
        cmd.arg("export-html")
            .arg(&session)
            .arg("--output-dir")
            .arg(&directory)
            .arg("--json");
        if let Some(option) = option {
            cmd.arg(option);
        }
        let output = cmd.output()?;
        assert_eq!(output.status.code(), Some(9));
        let receipt: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(receipt["success"], false);
        assert_eq!(receipt["error"]["kind"], "empty-session");
        assert!(!directory.exists());
    }
    let output = export_privacy_command(root.path())
        .arg("export-html")
        .arg(&session)
        .arg("--output-dir")
        .arg(&directory)
        .args(["--json", "--include-system-reminders"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout)?;
    let result = fs::read_to_string(
        receipt["exported"]["output_path"]
            .as_str()
            .ok_or("output path missing")?,
    )?;
    assert!(result.contains("ONLY_PRIVATE_REMINDER"));
    assert_eq!(fs::read(session)?, bytes);

    let session = root.path().join("skill.jsonl");
    let bytes = br#"{"role":"user","content":"Base directory for this skill: /synthetic/only-skill\nONLY_PRIVATE_SKILL"}
"#;
    fs::write(&session, bytes)?;
    let (_, html) = export_privacy_html(root.path(), &session, None, false, false)?;
    assert!(html.contains("[Skill loaded: only-skill]"));
    assert!(!html.contains("ONLY_PRIVATE_SKILL"));
    assert!(!html.contains("/synthetic/"));
    assert_eq!(fs::read(session)?, bytes);
    Ok(())
}

#[test]
fn export_redaction_raw_fidelity_requires_explicit_sensitive_option()
-> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let session = export_privacy_session(root.path())?;
    for format in ["markdown", "text", "json", "html"] {
        let output = export_privacy_command(root.path())
            .arg("export")
            .arg(&session)
            .args(["--format", format, "--include-secrets"])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(EXPORT_PRIVACY_SECRET));
        let policy = String::from_utf8_lossy(&output.stderr);
        assert!(policy.contains("include-secrets"));
        assert!(policy.contains("\"sensitive_output\":true"));
    }
    let (receipt, html) = export_privacy_html(root.path(), &session, None, true, false)?;
    assert!(html.contains(EXPORT_PRIVACY_SECRET));
    assert!(
        receipt["exported"]["title"]
            .as_str()
            .is_some_and(|title| title.contains(EXPORT_PRIVACY_SECRET))
    );
    assert!(!root.path().join("data/agent_search.db").exists());
    Ok(())
}

#[test]
#[cfg(feature = "encryption")]
fn export_redaction_policy_applies_before_encryption() -> Result<(), Box<dyn std::error::Error>> {
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit},
    };
    use base64::Engine as _;
    use base64::prelude::BASE64_STANDARD;
    use ring::pbkdf2;
    use std::num::NonZeroU32;

    let root = TempDir::new()?;
    let session = export_privacy_session(root.path())?;
    let carrier = Regex::new(r#"(?s)<div id="encrypted-content" hidden>(.*?)</div>"#)?;
    for include_secrets in [false, true] {
        let (receipt, html) =
            export_privacy_html(root.path(), &session, None, include_secrets, true)?;
        let payload = carrier
            .captures(&html)
            .and_then(|caps| caps.get(1))
            .ok_or("encrypted payload")?;
        let encrypted: Value = serde_json::from_str(payload.as_str())?;
        let salt = BASE64_STANDARD.decode(encrypted["salt"].as_str().ok_or("salt")?)?;
        let iv = BASE64_STANDARD.decode(encrypted["iv"].as_str().ok_or("nonce")?)?;
        let ciphertext =
            BASE64_STANDARD.decode(encrypted["ciphertext"].as_str().ok_or("ciphertext")?)?;
        let iterations = u32::try_from(encrypted["iterations"].as_u64().ok_or("KDF work")?)?;
        let mut key = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(iterations).ok_or("nonzero KDF work")?,
            &salt,
            b"export privacy regression passphrase",
            &mut key,
        );
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "AES key")?;
        let nonce = Nonce::try_from(iv.as_slice()).map_err(|_| "AES nonce")?;
        let plaintext = cipher
            .decrypt(&nonce, ciphertext.as_ref())
            .map_err(|_| "decrypt")?;
        let plaintext = String::from_utf8(plaintext)?;
        assert!(plaintext.contains("ordinary-kept-marker"));
        assert_eq!(plaintext.contains(EXPORT_PRIVACY_SECRET), include_secrets);
        if !include_secrets {
            assert!(!receipt.to_string().contains(EXPORT_PRIVACY_SECRET));
            assert!(!html.contains(EXPORT_PRIVACY_SECRET));
        }
    }
    Ok(())
}

// ============================================================================
// Structure Validation Tests
// ============================================================================

#[test]
fn test_no_separate_tool_articles() {
    let html = export_fixture("claude_session.jsonl");

    // Count message elements
    let assistant_count = count_pattern(&html, r#"class="[^"]*message[^"]*message-assistant"#);
    let tool_count = count_pattern(&html, r#"class="[^"]*message[^"]*message-tool"#);

    debug!(
        assistant_articles = assistant_count,
        tool_articles = tool_count,
        "Article counts"
    );

    // Tool messages should NOT have their own separate articles
    // They should be integrated into assistant message groups
    assert_eq!(
        tool_count, 0,
        "Tool messages should not be separate articles; they should be badges in assistant headers"
    );

    // Should have assistant message groups
    assert!(
        assistant_count > 0,
        "Should have at least one assistant message group"
    );
}

#[test]
fn test_tool_badges_in_header() {
    let html = export_fixture("claude_session.jsonl");

    // Tool badges should be present
    let has_badges = html.contains("tool-badge");
    assert!(
        has_badges,
        "HTML should contain tool-badge elements for tool calls"
    );

    // Badges should be button elements (for accessibility)
    let badge_buttons = html.contains("<button class=\"tool-badge");
    assert!(
        badge_buttons,
        "Tool badges should be button elements for keyboard accessibility"
    );

    // Badges should be inside message-header-right divs
    let header_right_with_badges =
        html.contains("message-header-right") && html.contains("tool-badge");
    assert!(
        header_right_with_badges,
        "Tool badges should be in message-header-right sections"
    );
}

#[test]
fn test_message_group_structure() {
    let html = export_fixture("claude_session.jsonl");

    // Should have message containers (articles)
    let message_count = count_pattern(&html, r#"class="message "#);

    debug!(message_count = message_count, "Message count");

    // Claude session fixture should produce multiple messages
    // (user request, assistant with tools, follow-up user, etc.)
    assert!(
        message_count >= 2,
        "Should have at least 2 messages, found {}",
        message_count
    );
}

#[test]
fn test_user_and_assistant_messages_present() {
    let html = export_fixture("claude_session.jsonl");

    // Should have both user and assistant messages
    let user_messages = count_pattern(&html, r#"message-user"#);
    let assistant_messages = count_pattern(&html, r#"message-assistant"#);

    debug!(
        user_messages = user_messages,
        assistant_messages = assistant_messages,
        "Message type counts"
    );

    assert!(user_messages > 0, "Should have user messages");
    assert!(assistant_messages > 0, "Should have assistant messages");
}

// ============================================================================
// CSS Validation Tests
// ============================================================================

#[test]
fn test_glassmorphism_css_present() {
    let html = export_fixture("claude_session.jsonl");

    assert!(
        html.contains("backdrop-filter"),
        "Glassmorphism requires backdrop-filter CSS"
    );
    assert!(
        html.contains("blur("),
        "Glassmorphism should have blur effect"
    );
}

#[test]
fn test_color_variables_defined() {
    let html = export_fixture("claude_session.jsonl");

    // Should use CSS custom properties for theming
    assert!(
        html.contains("--primary") || html.contains("--accent"),
        "Should define color CSS variables"
    );

    // Should use modern color formats (oklch preferred)
    let has_colors = html.contains("oklch(") || html.contains("rgb(") || html.contains("#");
    assert!(has_colors, "Should have color values defined");
}

#[test]
fn test_popover_css_present() {
    let html = export_fixture("claude_session.jsonl");

    assert!(
        html.contains(".tool-popover"),
        "Should have tool-popover CSS class"
    );

    // Popover visibility toggle
    let has_visible_state =
        html.contains(".tool-popover.visible") || html.contains(".tool-popover.active");
    assert!(
        has_visible_state || html.contains("popover"),
        "Popover should have visibility state CSS"
    );
}

#[test]
fn test_tool_badge_styling() {
    let html = export_fixture("claude_session.jsonl");

    assert!(
        html.contains(".tool-badge"),
        "Should have tool-badge CSS styling"
    );

    // Status variants
    let has_status_styles = html.contains("tool-status-success") || html.contains("tool-status");
    assert!(
        has_status_styles,
        "Should have tool status CSS classes for success/error states"
    );
}

#[test]
fn test_responsive_design() {
    let html = export_fixture("claude_session.jsonl");

    // Should have media queries for responsive design
    assert!(
        html.contains("@media"),
        "Should have responsive CSS media queries"
    );
}

// ============================================================================
// JavaScript Validation Tests
// ============================================================================

#[test]
fn test_popover_js_present() {
    let html = export_fixture("claude_session.jsonl");

    // Popover controller should be initialized
    let has_popover_js =
        html.contains("ToolPopovers") || html.contains("popover") || html.contains("showPopover");

    assert!(
        has_popover_js,
        "Should have popover JavaScript functionality"
    );
}

#[test]
fn test_aria_expanded_attribute() {
    let html = export_fixture("claude_session.jsonl");

    // Tool badges should have aria-expanded for accessibility
    assert!(
        html.contains("aria-expanded"),
        "Tool badges should have aria-expanded attribute for screen readers"
    );
}

#[test]
fn test_keyboard_navigation_support() {
    let html = export_fixture("claude_session.jsonl");

    // Should handle keyboard events
    let has_keyboard_support =
        html.contains("keydown") || html.contains("Enter") || html.contains("Escape");

    assert!(
        has_keyboard_support,
        "Should have keyboard navigation support for popovers"
    );
}

// ============================================================================
// Accessibility Validation Tests
// ============================================================================

#[test]
fn test_aria_labels_present() {
    let html = export_fixture("claude_session.jsonl");

    assert!(
        html.contains("aria-label"),
        "Should have aria-label attributes for accessibility"
    );
}

#[test]
fn test_semantic_html_structure() {
    let html = export_fixture("claude_session.jsonl");

    // Should use semantic article elements
    assert!(
        html.contains("<article") || html.contains(r#"role="article""#),
        "Messages should use article elements or role=article"
    );

    // Should have proper document structure
    assert!(html.contains("<main") || html.contains(r#"role="main""#));
}

#[test]
fn test_badges_are_interactive() {
    let html = export_fixture("claude_session.jsonl");

    // Badges should be button elements for keyboard accessibility
    let badge_buttons = count_pattern(&html, r#"<button[^>]*tool-badge"#);

    // Or at least have tabindex for focusability
    let has_tabindex = html.contains("tabindex");

    assert!(
        badge_buttons > 0 || has_tabindex,
        "Tool badges should be interactive (button elements or have tabindex)"
    );
}

#[test]
fn test_color_contrast_indicators() {
    let html = export_fixture("claude_session.jsonl");

    // Success should use green-ish colors
    let success_styles =
        html.contains("success") && (html.contains("green") || html.contains("0.7 0.15 145"));

    // Error should use red-ish colors
    let error_styles =
        html.contains("error") && (html.contains("red") || html.contains("0.65 0.2 25"));

    assert!(
        success_styles || error_styles || html.contains("tool-status"),
        "Should have distinct colors for success/error states"
    );
}

// ============================================================================
// Format-Specific Tests
// ============================================================================

#[test]
fn nd6pc_explicit_titles_follow_skill_policy() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-skill-title-policy-")
        .disable_cleanup(true)
        .tempdir()?;
    let session = root
        .path()
        .join("storage/session/project-title/title-session.json");
    let message = root
        .path()
        .join("storage/message/title-session/message.json");
    let part = root.path().join("storage/part/message/part.json");
    for path in [&session, &message, &part] {
        fs::create_dir_all(path.parent().ok_or("fixture parent missing")?)?;
    }
    fs::write(
        &message,
        serde_json::to_vec(&serde_json::json!({
            "id":"message", "sessionID":"title-session", "role":"user",
            "time":{"created":1733000001000_i64}
        }))?,
    )?;
    fs::write(
        &part,
        serde_json::to_vec(&serde_json::json!({
            "id":"part", "messageID":"message", "type":"text",
            "text":"TITLE_ORDINARY_PROMPT café 界"
        }))?,
    )?;
    for (title, html_title, opt_in) in [
        (
            "Base directory for this skill: /synthetic/TITLESECRETALPHA",
            "Base directory for this skill: /synthetic/TITLESECRETALPHA",
            Some("--include-skills"),
        ),
        (
            "<system-reminder>TITLESECRETALPHA</system-reminder>",
            "&lt;system-reminder&gt;TITLESECRETALPHA&lt;/system-reminder&gt;",
            Some("--include-system-reminders"),
        ),
        (
            "Ordinary explicit title café 界",
            "Ordinary explicit title café 界",
            None,
        ),
    ] {
        fs::write(
            &session,
            serde_json::to_vec(&serde_json::json!({
                "id":"title-session", "title":title,
                "time":{"created":1733000000000_i64, "updated":1733000002000_i64}
            }))?,
        )?;
        let originals = [&session, &message, &part]
            .into_iter()
            .map(|path| Ok((path, fs::read(path)?, fs::metadata(path)?)))
            .collect::<Result<Vec<_>, std::io::Error>>()?;
        for flags in [
            [].as_slice(),
            ["--include-skills"].as_slice(),
            ["--include-system-reminders"].as_slice(),
            ["--include-skills", "--include-system-reminders"].as_slice(),
        ] {
            let reveal_private = opt_in.is_some_and(|flag| flags.contains(&flag));
            for format in ["markdown", "html", "export-html"] {
                let mut cmd = export_privacy_command(root.path());
                if format == "export-html" {
                    cmd.arg("export-html")
                        .arg(&session)
                        .arg("--output-dir")
                        .arg(root.path().join("exports"))
                        .arg("--json");
                } else {
                    cmd.arg("export").arg(&session).args(["--format", format]);
                }
                cmd.args(flags);
                let output = cmd.output()?;
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let exported = if format == "export-html" {
                    let receipt: Value = serde_json::from_slice(&output.stdout)?;
                    let path = Path::new(
                        receipt
                            .get("exported")
                            .and_then(|exported| exported.get("output_path"))
                            .and_then(Value::as_str)
                            .ok_or("output path missing")?,
                    );
                    if !reveal_private {
                        assert!(
                            !path
                                .file_name()
                                .ok_or("filename missing")?
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .contains("titlesecretalpha")
                        );
                    }
                    fs::read_to_string(path)?
                } else {
                    String::from_utf8(output.stdout)?
                };
                assert!(exported.contains("TITLE_ORDINARY_PROMPT café 界"));
                assert_eq!(
                    exported.contains("TITLESECRETALPHA"),
                    reveal_private,
                    "{format}, {title}, {flags:?}"
                );
                let expected_title = if opt_in.is_some() && !reveal_private {
                    "TITLE_ORDINARY_PROMPT café 界"
                } else {
                    title
                };
                if format == "markdown" {
                    assert!(exported.starts_with(&format!("# {expected_title}\n")));
                } else {
                    let expected = if opt_in.is_some() && !reveal_private {
                        "TITLE_ORDINARY_PROMPT café 界"
                    } else {
                        html_title
                    };
                    assert!(exported.contains(&format!("<title>{expected}</title>")));
                }
            }
        }
        for (path, bytes, before) in originals {
            assert_eq!(fs::read(path)?, bytes);
            let after = fs::metadata(path)?;
            assert_eq!(before.modified()?, after.modified()?);
            assert_eq!(before.permissions(), after.permissions());
        }
    }
    assert!(!root.path().join("data/agent_search.db").exists());
    Ok(())
}

#[test]
fn nd6pc_all_export_formats_share_complete_message_skill_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let body = "Base directory for this skill: /synthetic/private\nFORMATS_PRIVATE_BODY";
    let variants = [
        serde_json::json!({"id":"private","type":"message","message":{"role":"assistant","content":[
            {"type":"text","text":"WHOLEMESSAGE_BYSTANDER"},
            {"type":"toolCall","id":"private-call","name":"Skill","arguments":{"body":body}}
        ]}}),
        serde_json::json!({"id":"private","type":"tool","message":{"role":"tool","content":"WHOLEMESSAGE_BYSTANDER","tool_name":"Skill","tool_output":body}}),
        serde_json::json!({"id":"private","role":"assistant","content":[
            {"type":"text","text":"WHOLEMESSAGE_BYSTANDER"},
            {"type":"tool_use","id":"private-call","name":"Skill","input":{"body":body}}
        ]}),
        serde_json::json!({"id":"private","role":"assistant","content":"WHOLEMESSAGE_BYSTANDER","metadata":{"documents":[{"text":body}]}}),
        serde_json::json!({"id":"private","role":"assistant","content":"WHOLEMESSAGE_BYSTANDER","metadata":{"skillInjection:":{"matchedSkills":["FORMATS_PRIVATE_BODY"]}}}),
        serde_json::json!({"id":"private","role":"assistant","content":"WHOLEMESSAGE_BYSTANDER","metadata":{(body):"private document key"}}),
    ];
    for (variant, injection) in variants.into_iter().enumerate() {
        let case_root = root.path().join(variant.to_string());
        fs::create_dir_all(&case_root)?;
        let session = case_root.join("session.jsonl");
        let tool_record = injection["type"] == "tool";
        let records = [
            serde_json::json!({"id":"prompt","role":"user","content":"FORMATS_ORDINARY_PROMPT café 界"}),
            serde_json::json!({"id":"answer","role":"assistant","content":"FORMATS_ORDINARY_ANSWER: discuss skills and the matchedSkills field.","metadata":{"ok":true,"count":7,"missing":null,"tags":["ordinary"]}}),
            injection,
            serde_json::json!({"id":"final","role":"assistant","content":"FORMATS_ORDINARY_FINAL"}),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record)?;
            bytes.push(b'\n');
        }
        fs::write(&session, &bytes)?;
        let before = fs::metadata(&session)?;
        for format in ["markdown", "text", "json", "html"] {
            for include_skills in [false, true] {
                for include_tools in [false, true] {
                    let mut cmd = export_privacy_command(&case_root);
                    cmd.arg("export").arg(&session).args(["--format", format]);
                    if include_skills {
                        cmd.args(["--include-skills", "--include-system-reminders"]);
                    }
                    if include_tools {
                        cmd.arg("--include-tools");
                    }
                    let output = cmd.output()?;
                    assert!(
                        output.status.success(),
                        "variant {variant}/{format}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let exported = String::from_utf8(output.stdout)?;
                    for marker in [
                        "FORMATS_ORDINARY_PROMPT",
                        "FORMATS_ORDINARY_ANSWER",
                        "FORMATS_ORDINARY_FINAL",
                        "café 界",
                    ] {
                        assert!(
                            exported.contains(marker),
                            "missing {marker} in variant {variant}/{format}"
                        );
                    }
                    assert_eq!(
                        exported.contains("WHOLEMESSAGE_BYSTANDER"),
                        include_skills && (format == "json" || include_tools || !tool_record),
                        "variant {variant}/{format}, skills={include_skills}, tools={include_tools}"
                    );
                    if !include_skills {
                        assert!(
                            !exported.contains("FORMATS_PRIVATE_BODY"),
                            "variant {variant}/{format}"
                        );
                    }
                    if format == "json" {
                        let messages: Vec<Value> = serde_json::from_str(&exported)?;
                        let ids: Vec<_> = messages
                            .iter()
                            .filter_map(|m| m.get("id").and_then(Value::as_str))
                            .collect();
                        let expected = if include_skills {
                            vec!["prompt", "answer", "private", "final"]
                        } else {
                            vec!["prompt", "answer", "final"]
                        };
                        assert_eq!(ids, expected);
                        assert_eq!(exported.contains("FORMATS_PRIVATE_BODY"), include_skills);
                    }
                }
            }
        }
        assert_eq!(fs::read(&session)?, bytes);
        let after = fs::metadata(&session)?;
        assert_eq!(before.modified()?, after.modified()?);
        assert_eq!(before.permissions(), after.permissions());
        assert!(!case_root.join("data/agent_search.db").exists());
    }
    Ok(())
}

#[test]
fn nd6pc_skill_selection_precedes_credential_redaction() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::Builder::new()
        .prefix("cass-export-skill-order-")
        .disable_cleanup(true)
        .tempdir()?;
    let session = root.path().join("session.jsonl");
    let marker = "Base directory for this skill: /synthetic/marker";
    let credential = "synthetic-credential-order-canary";
    let prompt = serde_json::json!({
        "id": "prompt", "role": "user", "content": "ORDER_ORDINARY café 界",
        "metadata": {"registration_token": credential}
    });
    let private = serde_json::json!({
        "id": "private", "role": "assistant", "content": "ORDER_PRIVATE_CANARY",
        "metadata": {"registration_token": marker}
    });
    let bytes = format!("{prompt}\n{private}\n");
    fs::write(&session, &bytes)?;
    let before = fs::metadata(&session)?;
    for (case, (skills, secrets)) in [(false, false), (true, false), (false, true), (true, true)]
        .into_iter()
        .enumerate()
    {
        for html in [false, true] {
            let mut cmd = export_privacy_command(root.path());
            if html {
                cmd.arg("export-html")
                    .arg(&session)
                    .arg("--output-dir")
                    .arg(root.path().join(format!("html-{case}")))
                    .arg("--json");
            } else {
                cmd.arg("export").arg(&session).args(["--format", "json"]);
            }
            if skills {
                cmd.arg("--include-skills");
            }
            if secrets {
                cmd.arg("--include-secrets");
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "{case}/{html}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result: Value = serde_json::from_slice(&output.stdout)?;
            if html {
                let path = result["exported"]["output_path"]
                    .as_str()
                    .ok_or("HTML output path missing")?;
                let rendered = fs::read_to_string(path)?;
                assert!(rendered.contains("ORDER_ORDINARY café 界"));
                assert_eq!(rendered.contains("ORDER_PRIVATE_CANARY"), skills);
                let count = if skills { 1 } else { 0 };
                assert!(rendered.contains(&format!("<span>1 prompt, {count} response")));
                assert_eq!(result["privacy"]["redaction_applied"], !secrets);
            } else {
                let records = result.as_array().ok_or("exported JSON is not an array")?;
                let ids: Vec<_> = records
                    .iter()
                    .filter_map(|record| record["id"].as_str())
                    .collect();
                let expected: &[_] = if skills {
                    &["prompt", "private"]
                } else {
                    &["prompt"]
                };
                assert_eq!(ids, expected);
                let rendered = String::from_utf8(output.stdout)?;
                assert!(rendered.contains("ORDER_ORDINARY café 界"));
                assert_eq!(rendered.contains("ORDER_PRIVATE_CANARY"), skills);
                assert_eq!(rendered.contains(credential), secrets);
                assert_eq!(rendered.contains(marker), skills && secrets);
            }
        }
    }
    assert_eq!(fs::read(&session)?, bytes.as_bytes());
    let after = fs::metadata(&session)?;
    assert_eq!(before.modified()?, after.modified()?);
    assert_eq!(before.permissions(), after.permissions());
    assert!(!root.path().join("data/agent_search.db").exists());
    Ok(())
}

#[test]
fn nd6pc_html_counts_exclude_structured_skill_messages() -> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let session = root.path().join("session.jsonl");
    let records = [
        serde_json::json!({"role":"user","content":"COUNTS_ORDINARY_PROMPT"}),
        serde_json::json!({"type":"message","message":{"role":"assistant","content":[
            {"type":"text","text":"COUNTS_PRIVATE_NARRATIVE"},
            {"type":"toolCall","id":"private-call","name":"Skill","arguments":{"body":"Base directory for this skill: /synthetic/private"}}
        ]}}),
        serde_json::json!({"role":"assistant","content":"COUNTS_ORDINARY_ANSWER"}),
    ];
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &record)?;
        bytes.push(b'\n');
    }
    fs::write(&session, &bytes)?;
    let (_, html) = export_privacy_html(root.path(), &session, None, false, false)?;
    assert!(html.contains("<span>1 prompt, 1 response, 0 tool uses</span>"));
    assert!(!html.contains("COUNTS_PRIVATE_NARRATIVE"));
    assert!(html.contains("COUNTS_ORDINARY_PROMPT") && html.contains("COUNTS_ORDINARY_ANSWER"));
    assert_eq!(fs::read(session)?, bytes);
    Ok(())
}

#[test]
fn nd6pc_html_skill_blocks_require_explicit_opt_in() -> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let body = "Base directory for this skill: /synthetic/private\nPROPRIETARYSKILLBODY café 界";
    let variants = [
        serde_json::json!({"type":"user","message":{"role":"user","content":[
            {"type":"text","text":"WHOLEMESSAGEBYSTANDER"},
            {"type":"tool_result","tool_use_id":"skill-load","content":body}
        ]}}),
        serde_json::json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"skill-load","content":[{"type":"text","text":body}]}
        ]}}),
        serde_json::json!({"type":"message","message":{"role":"toolResult","toolCallId":"skill-load","toolName":"Skill","content":[{"type":"text","text":body}]}}),
        serde_json::json!({"type":"tool","message":{"role":"tool","tool_name":"Skill","tool_output":body,"tool_call_id":"skill-load"}}),
        serde_json::json!({"type":"message","message":{"role":"assistant","content":[
            {"type":"text","text":"WHOLEMESSAGEBYSTANDER"},
            {"type":"toolCall","id":"private-input","name":"Skill","arguments":{"body":body}}
        ]}}),
    ];
    for (variant, injection) in variants.into_iter().enumerate() {
        let case_root = root.path().join(variant.to_string());
        fs::create_dir_all(&case_root)?;
        let session = case_root.join("session.jsonl");
        let records = [
            serde_json::json!({"role":"user","content":"PRIVATETITLESENTINEL\nBase directory for this skill: /synthetic/private"}),
            serde_json::json!({"role":"user","content":"ORDINARYPROMPT café 界"}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"text","text":"ORDINARYNARRATIVE"},
                {"type":"tool_use","id":"ordinary","name":"Read","input":{"path":"ordinary.txt"}}
            ]}}),
            serde_json::json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"ordinary","content":"ORDINARYRESULT: discuss skills; matchedSkills is a field name."}
            ]}}),
            injection,
            serde_json::json!({"role":"assistant","content":"ORDINARYFINAL"}),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record)?;
            bytes.push(b'\n');
        }
        fs::write(&session, &bytes)?;
        let before = fs::metadata(&session)?;
        for (include_skills, include_tools) in
            [(false, true), (true, true), (false, false), (true, false)]
        {
            let mut cmd = export_privacy_command(&case_root);
            cmd.arg("export-html")
                .arg(&session)
                .arg("--output-dir")
                .arg(case_root.join(format!("export-{include_skills}-{include_tools}")))
                .arg("--json");
            if include_skills {
                cmd.arg("--include-skills");
            }
            if !include_tools {
                cmd.arg("--include-tools=false");
            }
            let output = cmd.output()?;
            assert!(
                output.status.success(),
                "variant {variant}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let receipt: Value = serde_json::from_slice(&output.stdout)?;
            let output_path = receipt["exported"]["output_path"]
                .as_str()
                .ok_or("output path")?;
            let html = fs::read_to_string(output_path)?;
            assert!(
                html.contains("ORDINARYPROMPT")
                    && html.contains("ORDINARYNARRATIVE")
                    && html.contains("ORDINARYFINAL")
            );
            assert!(html.contains("café") && html.contains('界'));
            assert_eq!(
                html.contains("ORDINARYRESULT"),
                include_tools,
                "variant {variant}"
            );
            assert_eq!(
                html.contains("PROPRIETARYSKILLBODY"),
                include_skills && include_tools,
                "variant {variant}"
            );
            assert_eq!(html.contains("PRIVATETITLESENTINEL"), include_skills);
            if !include_skills {
                assert!(
                    !output_path
                        .to_ascii_lowercase()
                        .contains("privatetitlesentinel")
                );
                assert!(html.contains("<title>ORDINARYPROMPT"));
                assert!(
                    !html.contains("WHOLEMESSAGEBYSTANDER"),
                    "whole skill message retained in variant {variant}"
                );
            }
            assert_eq!(receipt["privacy"]["sensitive_output"], false);
        }
        assert_eq!(fs::read(&session)?, bytes);
        let after = fs::metadata(&session)?;
        assert_eq!(before.modified()?, after.modified()?);
        assert_eq!(before.permissions(), after.permissions());
        assert!(!case_root.join("data/agent_search.db").exists());
    }
    Ok(())
}

#[test]
fn test_claude_tool_result_error_status_survives_html_grouping()
-> Result<(), Box<dyn std::error::Error>> {
    let root = TempDir::new()?;
    let session = root.path().join("session.jsonl");
    let records = [
        serde_json::json!({"role":"user","content":"STATUSPROMPT"}),
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"STATUSNARRATIVE"},
            {"type":"tool_use","id":"failed","name":"Bash","input":{"command":"false"}},
            {"type":"tool_use","id":"ok","name":"Read","input":{"path":"ok.txt"}},
            {"type":"tool_use","id":"unspecified","name":"Write","input":{"path":"written.txt"}},
            {"type":"tool_use","id":"invalid","name":"Glob","input":{"pattern":"*.rs"}}
        ]}}),
        serde_json::json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"failed","content":"FAILEDOUTPUT (text says success)","is_error":true},
            {"type":"tool_result","tool_use_id":"ok","content":"SUCCESSOUTPUT (text discusses an error)","is_error":false},
            {"type":"tool_result","tool_use_id":"unspecified","content":"UNSPECIFIEDOUTPUT"},
            {"type":"tool_result","tool_use_id":"invalid","content":"INVALIDSTATUSOUTPUT","is_error":"true"}
        ]}}),
        serde_json::json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"orphan","content":"ORPHANFAILEDOUTPUT","is_error":true}
        ]}}),
        serde_json::json!({"role":"assistant","content":"STATUSFINAL"}),
    ];
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &record)?;
        bytes.push(b'\n');
    }
    fs::write(&session, &bytes)?;
    let (_, html) = export_privacy_html(root.path(), &session, None, false, false)?;
    let badges = Regex::new(r#"<button class="tool-badge ([^"]*)"\s+aria-label="([^"]*)""#)?;
    let actual: Vec<_> = badges
        .captures_iter(&html)
        .map(|c| (c[1].to_owned(), c[2].to_owned()))
        .collect();
    assert_eq!(
        actual,
        [
            ("tool-status-error".to_owned(), "Bash: error".to_owned()),
            ("tool-status-success".to_owned(), "Read: success".to_owned()),
            (
                "tool-status-success".to_owned(),
                "Write: success".to_owned()
            ),
            ("tool-status-success".to_owned(), "Glob: success".to_owned()),
        ]
    );
    for retained in [
        "STATUSPROMPT",
        "STATUSNARRATIVE",
        "STATUSFINAL",
        "FAILEDOUTPUT",
        "SUCCESSOUTPUT",
        "UNSPECIFIEDOUTPUT",
        "INVALIDSTATUSOUTPUT",
        "ORPHANFAILEDOUTPUT",
        "tool_result error",
    ] {
        assert!(html.contains(retained), "missing {retained}");
    }
    assert_eq!(fs::read(session)?, bytes);
    Ok(())
}

#[test]
fn test_claude_format_export() {
    let html = export_fixture("claude_session.jsonl");

    // Claude exports should work
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("conversation"));

    // Should have tool usage (Claude format has tool_use/tool_result)
    let has_tool_indicators = html.contains("tool-badge") || html.contains("Read");
    assert!(
        has_tool_indicators,
        "Claude format export should show tool usage"
    );
}

#[test]
fn test_codex_format_export() {
    let html = export_fixture("codex_session.jsonl");

    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("conversation"));

    // Codex uses function_call structure
    let has_content = html.contains("shell") || html.contains("Python") || html.contains("list");
    assert!(
        has_content,
        "Codex format export should have session content"
    );
}

#[test]
fn test_cursor_format_export() {
    let html = export_fixture("cursor_session.jsonl");

    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("conversation"));

    // Cursor format has embedded tool results
    let has_content = html.contains("main") || html.contains("function") || html.contains("Config");
    assert!(
        has_content,
        "Cursor format export should have session content"
    );
}

#[test]
fn test_opencode_format_export() {
    let html = export_fixture("opencode_session.jsonl");

    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("conversation"));

    // OpenCode has tool_calls arrays
    let has_content =
        html.contains("auth") || html.contains("JWT") || html.contains("authentication");
    assert!(
        has_content,
        "OpenCode format export should have session content"
    );
}

#[test]
fn test_edge_cases_export() {
    let html = export_fixture("edge_cases.jsonl");

    assert!(html.contains("<!DOCTYPE html>"));

    // Should handle unicode content
    assert!(
        html.contains("你好") || html.contains("&#") || html.contains("Unicode"),
        "Should preserve or escape unicode content"
    );

    // Should escape HTML special characters
    assert!(
        !html.contains("<script>alert"),
        "Should escape XSS attempts in content"
    );
}

// ============================================================================
// Export Option Variations
// ============================================================================

#[test]
fn test_export_produces_valid_html() {
    let html = export_fixture("claude_session.jsonl");

    // Basic HTML structure
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("<html"));
    assert!(html.contains("</html>"));
    assert!(html.contains("<head>"));
    assert!(html.contains("</head>"));
    assert!(html.contains("<body"));
    assert!(html.contains("</body>"));
}

#[test]
fn test_export_includes_styles() {
    let html = export_fixture("claude_session.jsonl");

    // Should have inline critical CSS
    assert!(
        html.contains("<style>") || html.contains("<style "),
        "Should include inline CSS styles"
    );

    // Should have substantial styling
    let style_content_len = html
        .find("</style>")
        .map(|end| {
            html[..end]
                .rfind("<style")
                .map(|start| end - start)
                .unwrap_or(0)
        })
        .unwrap_or(0);

    assert!(
        style_content_len > 1000,
        "Should have substantial CSS (found {} chars)",
        style_content_len
    );
}

#[test]
fn test_export_includes_scripts() {
    let html = export_fixture("claude_session.jsonl");

    // Should have JavaScript for interactivity
    assert!(
        html.contains("<script>") || html.contains("<script "),
        "Should include JavaScript"
    );
}

#[test]
fn test_export_self_contained() {
    let html = export_fixture("claude_session.jsonl");

    // Should be largely self-contained (CSS inlined)
    // May have CDN links for syntax highlighting
    assert!(
        html.contains("<style>"),
        "Should have inlined critical CSS for self-contained export"
    );
}

// ============================================================================
// Performance and Size Tests
// ============================================================================

#[test]
fn test_export_reasonable_size() {
    let html = export_fixture("claude_session.jsonl");

    // Export should be reasonably sized (not bloated)
    // Claude session fixture is small, so export should be < 500KB
    let size_kb = html.len() / 1024;

    debug!(size_kb = size_kb, "Export size");

    assert!(
        size_kb < 500,
        "Export should be < 500KB for small session, got {}KB",
        size_kb
    );
}

#[test]
fn test_export_completes_quickly() {
    use std::time::Instant;

    let start = Instant::now();
    let _html = export_fixture("claude_session.jsonl");
    let elapsed = start.elapsed();

    debug!(elapsed_ms = elapsed.as_millis(), "Export duration");

    // Debug-test runs on remote workers are substantially slower than release builds.
    // Keep this as a broad regression guard rather than a stale sub-10s budget.
    assert!(
        elapsed.as_secs() < 120,
        "Export should complete in < 120s in debug-test environments, took {:?}",
        elapsed
    );
}
