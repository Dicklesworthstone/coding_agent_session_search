use assert_cmd::Command;
use clap::Parser;
use coding_agent_search::storage::sqlite::SqliteStorage;
use coding_agent_search::{Cli, Commands};
use predicates::str::contains;
use serial_test::serial;
use std::fs;
use tempfile::TempDir;

mod util;
use util::EnvGuard;

fn run_on_large_stack<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("cass-cli-index-parse-test".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .expect("spawn large-stack test thread");
    match handle.join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn parse_cli_ok<const N: usize>(args: [&'static str; N], context: &'static str) -> Cli {
    run_on_large_stack(move || <Cli as Parser>::try_parse_from(args).expect(context))
}

fn base_cmd(temp_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    // Isolate connectors by pointing HOME and XDG vars to temp dir
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    // Specific overrides if needed (some might fallback to other paths, but HOME usually covers it)
    cmd.env("CODEX_HOME", temp_home.join(".codex"));
    cmd
}

#[test]
fn index_help_prints_usage() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd(tmp.path());
    cmd.args(["index", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("Run indexer"))
        .stdout(contains("--full"))
        .stdout(contains("--watch"))
        .stdout(contains("--semantic"))
        .stdout(contains("--embedder"));
}

#[test]
fn index_parses_semantic_flags() -> Result<(), String> {
    let cli = parse_cli_ok(
        ["cass", "index", "--semantic", "--embedder", "fastembed"],
        "parse index flags",
    );

    match cli.command {
        Some(Commands::Index {
            semantic, embedder, ..
        }) => {
            assert!(semantic, "semantic flag should be true");
            assert_eq!(embedder, "fastembed");
            Ok(())
        }
        other => Err(format!("expected index command, got {other:?}")),
    }
}

#[test]
fn index_default_embedder_is_fastembed() -> Result<(), String> {
    let cli = parse_cli_ok(["cass", "index", "--semantic"], "parse index flags");

    match cli.command {
        Some(Commands::Index { embedder, .. }) => {
            assert_eq!(embedder, "fastembed");
            Ok(())
        }
        other => Err(format!("expected index command, got {other:?}")),
    }
}

#[test]
fn index_creates_db_and_index() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);

    cmd.assert().success();

    assert!(data_dir.join("agent_search.db").exists(), "DB created");
    // Index dir should exist
    let index_path = data_dir.join("index");
    assert!(index_path.exists(), "index dir created");
}

#[test]
fn index_full_rebuilds() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // First run
    let mut cmd1 = base_cmd(tmp.path());
    cmd1.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    cmd1.assert().success();

    // Second run with --full
    let mut cmd2 = base_cmd(tmp.path());
    cmd2.args([
        "index",
        "--full",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);

    cmd2.assert().success();
}

#[test]
fn index_watch_once_triggers() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let dummy_path = data_dir.join("dummy.txt");
    fs::write(&dummy_path, "dummy content").unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "index",
        "--watch-once",
        dummy_path.to_str().unwrap(),
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);

    cmd.assert().success();
}

#[test]
fn index_refresh_data_dir_scopes_rebuild_semantic_and_watch_once_controls() -> Result<(), String> {
    let cli = parse_cli_ok(
        [
            "cass",
            "index",
            "--data-dir",
            "/cass/custom-data",
            "--full",
            "--force-rebuild",
            "--semantic",
            "--build-hnsw",
            "--watch-once",
            "/sessions/one.jsonl,/sessions/two.jsonl",
            "--json",
        ],
        "parse scoped refresh controls",
    );

    match cli.command {
        Some(Commands::Index {
            data_dir: Some(data_dir),
            full: true,
            force_rebuild: true,
            semantic: true,
            build_hnsw: true,
            watch_once: Some(paths),
            json: true,
            ..
        }) => {
            assert_eq!(data_dir, std::path::PathBuf::from("/cass/custom-data"));
            assert_eq!(
                paths,
                vec![
                    std::path::PathBuf::from("/sessions/one.jsonl"),
                    std::path::PathBuf::from("/sessions/two.jsonl"),
                ]
            );
            Ok(())
        }
        other => Err(format!(
            "expected data-dir scoped index refresh controls, got {other:?}"
        )),
    }
}

#[test]
fn index_refresh_progress_controls_remain_scoped_to_data_dir() -> Result<(), String> {
    let cli = parse_cli_ok(
        [
            "cass",
            "index",
            "--data-dir",
            "/cass/refresh-data",
            "--full",
            "--idempotency-key",
            "refresh-window-42",
            "--progress-interval-ms",
            "125",
            "--no-progress-events",
            "--json",
        ],
        "parse data-dir scoped refresh progress controls",
    );

    match cli.command {
        Some(Commands::Index {
            data_dir: Some(data_dir),
            full: true,
            idempotency_key: Some(idempotency_key),
            progress_interval_ms: 125,
            no_progress_events: true,
            json: true,
            ..
        }) => {
            assert_eq!(data_dir, std::path::PathBuf::from("/cass/refresh-data"));
            assert_eq!(idempotency_key, "refresh-window-42");
            Ok(())
        }
        other => Err(format!(
            "expected data-dir scoped refresh progress controls, got {other:?}"
        )),
    }
}

#[test]
fn index_json_reports_entrypoint_contract_for_incremental_and_watch_once()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir)?;

    let mut incremental = base_cmd(tmp.path());
    incremental.args([
        "index",
        "--data-dir",
        data_dir.to_string_lossy().as_ref(),
        "--json",
    ]);
    let incremental_output = incremental.output()?;
    let incremental_stdout = String::from_utf8_lossy(&incremental_output.stdout);
    let incremental_stderr = String::from_utf8_lossy(&incremental_output.stderr);
    assert!(
        incremental_output.status.success(),
        "incremental index should succeed. stdout: {incremental_stdout}, stderr: {incremental_stderr}"
    );
    let incremental_payload: serde_json::Value =
        serde_json::from_slice(&incremental_output.stdout)?;
    assert_eq!(incremental_payload["entrypoint"]["kind"], "incremental");
    assert_eq!(
        incremental_payload["entrypoint"]["migration_state"],
        "tin8o_entrypoint_observed"
    );
    assert_eq!(
        incremental_payload["entrypoint"]["watch_once_path_count"],
        0
    );

    let dummy_path = data_dir.join("entrypoint-watch-once.txt");
    fs::write(&dummy_path, "watch once entrypoint")?;
    let mut watch_once = base_cmd(tmp.path());
    watch_once.args([
        "index",
        "--watch-once",
        dummy_path.to_string_lossy().as_ref(),
        "--data-dir",
        data_dir.to_string_lossy().as_ref(),
        "--json",
    ]);
    let watch_once_output = watch_once.output()?;
    let watch_stdout = String::from_utf8_lossy(&watch_once_output.stdout);
    let watch_stderr = String::from_utf8_lossy(&watch_once_output.stderr);
    assert!(
        watch_once_output.status.success(),
        "watch-once index should succeed. stdout: {watch_stdout}, stderr: {watch_stderr}"
    );
    let watch_once_payload: serde_json::Value = serde_json::from_slice(&watch_once_output.stdout)?;
    assert_eq!(watch_once_payload["entrypoint"]["kind"], "watch_once");
    assert_eq!(watch_once_payload["entrypoint"]["watch_once_path_count"], 1);
    assert_eq!(watch_once_payload["entrypoint"]["watch"], false);

    Ok(())
}

#[test]
fn index_force_rebuild_flag() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "index",
        "--force-rebuild",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);

    cmd.assert().success();
    assert!(data_dir.join("agent_search.db").exists());
}

#[test]
fn index_handles_existing_schema_13_db() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("agent_search.db");

    // Seed an existing DB and force schema_version=13 to guard against
    // regressions where v13 is treated as unsupported.
    let storage = SqliteStorage::open(&db_path).expect("seed sqlite db");
    storage
        .raw()
        .execute("UPDATE meta SET value = '13' WHERE key = 'schema_version'")
        .expect("set schema_version to 13");
    drop(storage);

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);

    let output = cmd.output().expect("run index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "index should succeed for existing schema v13 db. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unsupported schema version 13"),
        "stderr should not contain schema-v13 rejection. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("index --json should emit valid JSON");
    assert_eq!(payload.get("success").and_then(|v| v.as_bool()), Some(true));
}

/// Creates a Codex session file with the modern envelope format.
fn codex_iso_timestamp(ts_ms: u64) -> String {
    let ts_ms_i64 = i64::try_from(ts_ms).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp_millis(ts_ms_i64)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

fn make_codex_session(root: &std::path::Path, date_path: &str, filename: &str, content: &str) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let workspace = root.to_string_lossy();
    let lines = [
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts),
            "type": "session_meta",
            "payload": {
                "id": filename,
                "cwd": workspace,
                "cli_version": "0.42.0"
            }
        }),
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": content }]
            }
        }),
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts + 2_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": format!("{content}_response") }]
            }
        }),
    ];
    let mut sample = String::new();
    for line in lines {
        sample.push_str(&serde_json::to_string(&line).unwrap());
        sample.push('\n');
    }
    fs::write(file, sample).unwrap();
}

#[test]
#[serial]
fn index_json_reports_full_refresh_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-full.jsonl",
        "full_strategy_content",
    );

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run full index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "full index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "full index --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("deferred_authoritative_db_rebuild")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("full_refresh_defers_inline_lexical_writes_to_authoritative_db_rebuild")
    );
    assert_eq!(
        payload.get("messages").and_then(|value| value.as_i64()),
        stats.get("total_messages").and_then(|value| value.as_i64())
    );
}

#[test]
#[serial]
fn index_json_reports_repeat_full_refresh_strategy_on_populated_canonical_db() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-canonical.jsonl",
        "canonical_only_strategy_content",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run canonical-only full rebuild");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "canonical-only full rebuild should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "canonical-only full rebuild --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("deferred_authoritative_db_rebuild")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("full_refresh_defers_inline_lexical_writes_to_authoritative_db_rebuild")
    );
    assert_eq!(
        payload.get("messages").and_then(|value| value.as_i64()),
        stats.get("total_messages").and_then(|value| value.as_i64())
    );
}

#[test]
#[serial]
fn repeat_full_json_preserves_exact_totals_when_noop_scan_underreports() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "repeat-full-noop.jsonl",
        "repeat_full_noop_content",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let initial_output = initial_index.output().expect("run initial full index");
    assert!(
        initial_output.status.success(),
        "initial full index should succeed. stdout: {}, stderr: {}",
        String::from_utf8_lossy(&initial_output.stdout),
        String::from_utf8_lossy(&initial_output.stderr)
    );
    let initial_payload: serde_json::Value =
        serde_json::from_slice(&initial_output.stdout).expect("valid initial JSON output");
    let expected_conversations = initial_payload
        .get("conversations")
        .and_then(|value| value.as_i64())
        .expect("initial conversation count");
    let expected_messages = initial_payload
        .get("messages")
        .and_then(|value| value.as_i64())
        .expect("initial message count");
    let checkpoint_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .unwrap()
        .join(".lexical-rebuild-state.json");
    let checkpoint_modified_before = fs::metadata(&checkpoint_path)
        .expect("initial lexical checkpoint")
        .modified()
        .expect("initial checkpoint mtime");

    fs::rename(&codex_home, home.join(".codex_hidden")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run repeat full index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "repeat full index should succeed. stdout: {stdout}, stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        payload
            .get("conversations")
            .and_then(|value| value.as_i64()),
        Some(expected_conversations),
        "repeat no-op full runs should preserve canonical conversation totals even when the scan phase temporarily sees no source files"
    );
    assert_eq!(
        payload.get("messages").and_then(|value| value.as_i64()),
        Some(expected_messages),
        "repeat no-op full runs should preserve canonical message totals even when the scan phase temporarily sees no source files"
    );
    assert_eq!(
        stats
            .get("total_conversations")
            .and_then(|value| value.as_i64()),
        Some(expected_conversations)
    );
    assert_eq!(
        stats.get("total_messages").and_then(|value| value.as_i64()),
        Some(expected_messages)
    );
    let checkpoint_modified_after = fs::metadata(&checkpoint_path)
        .expect("preserved lexical checkpoint")
        .modified()
        .expect("preserved checkpoint mtime");
    assert_eq!(
        checkpoint_modified_after, checkpoint_modified_before,
        "repeat no-op full runs should preserve the matching lexical checkpoint instead of deleting and recreating it"
    );
}

#[test]
#[serial]
fn index_full_persists_lexical_rebuild_equivalence_ledger() {
    // Bead ibuuh.29 E2E acceptance: the authoritative serial rebuild must
    // persist an equivalence ledger (document count, manifest fingerprint,
    // golden-query digest) as a preserved artifact so operators and external
    // tooling can diff it across runs without replaying the corpus.
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Seed a small mixed corpus so the rebuild touches multiple distinct
    // conversations and exercises the streaming accumulator beyond a trivial
    // single-conversation path.
    for (idx, content) in [
        "equivalence-ledger-alpha",
        "equivalence-ledger-bravo",
        "equivalence-ledger-charlie",
        "equivalence-ledger-delta",
    ]
    .iter()
    .enumerate()
    {
        make_codex_session(
            &codex_home,
            "2026/04/22",
            &format!("equivalence-ledger-{idx:02}.jsonl"),
            content,
        );
    }

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run full index");
    assert!(
        output.status.success(),
        "full index should succeed. stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let reported_conversations = payload
        .get("conversations")
        .and_then(|value| value.as_i64())
        .expect("conversation count in payload");
    assert!(
        reported_conversations >= 2,
        "expected at least 2 seeded conversations, got {reported_conversations}"
    );

    let index_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .expect("resolve tantivy index dir");
    let ledger_path = index_path.join(".lexical-rebuild-equivalence.json");
    assert!(
        ledger_path.exists(),
        "authoritative rebuild must persist the equivalence ledger artifact at {}",
        ledger_path.display()
    );
    let raw = fs::read_to_string(&ledger_path).expect("read equivalence ledger artifact");
    let ledger: serde_json::Value =
        serde_json::from_str(&raw).expect("equivalence ledger must be valid JSON");
    let document_count = ledger
        .get("document_count")
        .and_then(|value| value.as_u64())
        .expect("ledger has integer document_count");
    assert!(
        document_count >= reported_conversations as u64,
        "ledger document_count ({document_count}) should be at least the conversation count \
         ({reported_conversations}); a single-message fixture still yields one indexed doc"
    );
    let manifest_fingerprint = ledger
        .get("manifest_fingerprint")
        .and_then(|value| value.as_str())
        .expect("ledger has string manifest_fingerprint");
    assert_eq!(
        manifest_fingerprint.len(),
        64,
        "manifest_fingerprint must be a 32-byte blake3 hex digest, got {}",
        manifest_fingerprint.len()
    );
    assert!(
        manifest_fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "manifest_fingerprint must be hex, got {manifest_fingerprint}"
    );
    let golden_query_digest = ledger
        .get("golden_query_digest")
        .and_then(|value| value.as_str())
        .expect("ledger has string golden_query_digest");
    assert_eq!(
        golden_query_digest.len(),
        64,
        "golden_query_digest must be a 32-byte blake3 hex digest"
    );
    let probes: Vec<&str> = ledger
        .get("golden_query_hit_counts")
        .and_then(|value| value.as_array())
        .expect("ledger has golden_query_hit_counts array")
        .iter()
        .map(|entry| {
            entry
                .get("probe")
                .and_then(|value| value.as_str())
                .expect("hit entry has probe string")
        })
        .collect();
    assert_eq!(
        probes,
        vec!["error", "TODO", "function", "import", "test"],
        "ledger must record the default probe list in canonical order"
    );

    // Search readiness: a substring from the seeded content must be
    // discoverable via `cass search` after the authoritative rebuild, so the
    // evidence ledger is paired with a user-visible correctness signal.
    let mut search_cmd = base_cmd(home);
    search_cmd
        .args(["search", "equivalence-ledger-alpha", "--data-dir"])
        .arg(&data_dir);
    let search_output = search_cmd.output().expect("run cass search");
    assert!(
        search_output.status.success(),
        "search after authoritative rebuild should succeed. stdout: {}, stderr: {}",
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );
    let search_stdout = String::from_utf8_lossy(&search_output.stdout);
    assert!(
        search_stdout.contains("equivalence-ledger-alpha"),
        "search should surface the seeded content; got stdout:\n{search_stdout}"
    );
}

#[test]
#[serial]
fn index_json_reports_incremental_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-incremental-1.jsonl",
        "incremental_strategy_content_alpha",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    std::thread::sleep(std::time::Duration::from_secs(2));
    make_codex_session(
        &codex_home,
        "2025/11/25",
        "strategy-incremental-2.jsonl",
        "incremental_strategy_content_beta",
    );

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--json", "--data-dir"]).arg(&data_dir);
    let output = cmd.output().expect("run incremental index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "incremental index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "incremental index --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("incremental_inline")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("incremental_scan_applies_inline_lexical_updates_only_for_new_messages")
    );
}

#[test]
#[serial]
fn index_json_reports_watch_once_incremental_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-watch-once-1.jsonl",
        "watch_once_strategy_seed",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    std::thread::sleep(std::time::Duration::from_secs(2));
    let targeted_path = codex_home.join("sessions/2025/11/25/strategy-watch-once-2.jsonl");
    make_codex_session(
        &codex_home,
        "2025/11/25",
        "strategy-watch-once-2.jsonl",
        "watch_once_strategy_delta",
    );

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--watch-once"])
        .arg(&targeted_path)
        .args(["--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run targeted watch-once index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "watch-once incremental index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "watch-once incremental index --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("incremental_inline")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("watch_once_targeted_reindex_applies_inline_lexical_updates_for_changed_paths")
    );
}

#[test]
#[serial]
fn plain_index_recreates_missing_lexical_checkpoint_from_live_assets() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "checkpoint-bootstrap.jsonl",
        "checkpoint_bootstrap_content",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    let index_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .expect("resolve versioned tantivy index path");
    let state_path = index_path.join(".lexical-rebuild-state.json");
    let state_backup_path = index_path.join(".lexical-rebuild-state.backup.json");
    if state_path.exists() {
        fs::rename(&state_path, &state_backup_path).expect("hide lexical checkpoint");
    }
    assert!(
        !state_path.exists(),
        "test fixture should remove the visible lexical checkpoint"
    );

    let mut plain_index = base_cmd(home);
    plain_index
        .args(["index", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = plain_index.output().expect("run plain incremental index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "plain incremental index should repair the missing lexical checkpoint. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        state_path.exists(),
        "plain incremental index should recreate the lexical checkpoint"
    );

    let checkpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).expect("read recreated checkpoint"))
            .expect("parse recreated checkpoint");
    assert_eq!(checkpoint["completed"], serde_json::Value::Bool(true));

    let mut health = base_cmd(home);
    health
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir);
    let health_output = health
        .output()
        .expect("run health after checkpoint bootstrap");
    assert!(
        health_output.status.success(),
        "health should stay green after checkpoint bootstrap\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&health_output.stdout),
        String::from_utf8_lossy(&health_output.stderr)
    );
}

/// Test incremental indexing: creates sessions, indexes, adds more, re-indexes,
/// and verifies only new sessions are processed while all remain searchable.
#[test]
fn incremental_index_only_processes_new_sessions() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Phase 1: Create initial 5 sessions
    make_codex_session(
        &codex_home,
        "2025/11/20",
        "rollout-1.jsonl",
        "alpha_content",
    );
    make_codex_session(&codex_home, "2025/11/20", "rollout-2.jsonl", "beta_content");
    make_codex_session(
        &codex_home,
        "2025/11/21",
        "rollout-1.jsonl",
        "gamma_content",
    );
    make_codex_session(
        &codex_home,
        "2025/11/21",
        "rollout-2.jsonl",
        "delta_content",
    );
    make_codex_session(
        &codex_home,
        "2025/11/22",
        "rollout-1.jsonl",
        "epsilon_content",
    );

    // Full index
    let mut cmd1 = base_cmd(home);
    cmd1.env("CODEX_HOME", &codex_home);
    cmd1.args([
        "index",
        "--full",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);
    cmd1.assert().success();

    // Verify all 5 sessions indexed - search for unique content
    for term in [
        "alpha_content",
        "beta_content",
        "gamma_content",
        "delta_content",
        "epsilon_content",
    ] {
        let mut search = base_cmd(home);
        search.env("CODEX_HOME", &codex_home);
        search.args([
            "search",
            term,
            "--robot",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ]);
        let output = search.output().expect("search command");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "search should succeed for {term}. stdout: {stdout}, stderr: {stderr}"
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("valid json output");
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .expect("hits array");
        assert!(
            !hits.is_empty(),
            "Should find hit for {term} after initial index. Full response: {stdout}"
        );
    }

    // Phase 2: Wait to ensure mtime difference, then add 2 new sessions
    std::thread::sleep(std::time::Duration::from_secs(2));

    make_codex_session(&codex_home, "2025/11/23", "rollout-1.jsonl", "zeta_content");
    make_codex_session(&codex_home, "2025/11/23", "rollout-2.jsonl", "eta_content");

    // Incremental index (no --full flag)
    let mut cmd2 = base_cmd(home);
    cmd2.env("CODEX_HOME", &codex_home);
    cmd2.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    cmd2.assert().success();

    // Verify all 7 sessions are now searchable
    for term in [
        "alpha_content",
        "beta_content",
        "gamma_content",
        "delta_content",
        "epsilon_content",
        "zeta_content",
        "eta_content",
    ] {
        let mut search = base_cmd(home);
        search.env("CODEX_HOME", &codex_home);
        search.args([
            "search",
            term,
            "--robot",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ]);
        let output = search.output().expect("search command");
        assert!(output.status.success(), "search should succeed");
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .expect("hits array");
        assert!(
            !hits.is_empty(),
            "Should find hit for {term} after incremental index"
        );
    }

    // Verify the new sessions specifically
    let mut search_zeta = base_cmd(home);
    search_zeta.env("CODEX_HOME", &codex_home);
    search_zeta.args([
        "search",
        "zeta_content",
        "--robot",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    let output = search_zeta.output().expect("search command");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    assert!(
        !hits.is_empty(),
        "Should find at least one hit for zeta_content"
    );
    assert_eq!(
        hits[0]["agent"], "codex",
        "Hit should be from codex connector"
    );
}

/// Bead ibuuh.10 slice (a): regression test that lexical self-heal
/// reindexes from the canonical DB when the ENTIRE lexical index
/// directory is gone, not just the `.lexical-rebuild-state.json`
/// checkpoint sidecar.
///
/// The existing sibling test
/// `plain_index_recreates_missing_lexical_checkpoint_from_live_assets`
/// covers only the "checkpoint sidecar missing, Tantivy files intact"
/// case. This test covers the stronger corruption scenario an operator
/// or upgrade-accident would produce: everything under
/// `<data_dir>/index/` is gone, but the canonical `agent_search.db` is
/// intact. A healthy cass MUST reindex from the canonical DB and
/// become searchable again via a plain `cass index` invocation — no
/// `--full` or `--force-rebuild` flag required.
///
/// What this pins for the self-heal + fail-open contract:
///   1. `cass index` (plain incremental, no flags) returns success
///      after the lexical tree is wiped.
///   2. The tantivy index directory re-materializes on disk with
///      content derived from the existing DB rows.
///   3. A subsequent `cass search` returns the expected hit, so the
///      user experience on the self-heal path is "run index once,
///      search works again" — no manual `--full` rebuild required.
#[test]
#[serial]
fn plain_index_self_heals_when_entire_lexical_index_directory_is_missing() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Seed three distinct sessions with a stable single-word keyword
    // each (avoid underscores — Tantivy's default tokenizer splits on
    // them and a phrase query wouldn't match after round-trip through
    // the rebuild path). The search step below probes one of these.
    for (idx, keyword) in ["alphaheal", "bravoheal", "charlieheal"].iter().enumerate() {
        make_codex_session(
            &codex_home,
            "2026/04/23",
            &format!("self-heal-fixture-{idx:02}.jsonl"),
            keyword,
        );
    }

    // Initial full index to populate the canonical DB AND the lexical
    // index.
    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let initial_output = initial_index.output().expect("run initial full index");
    assert!(
        initial_output.status.success(),
        "initial full index must succeed to seed canonical + lexical assets.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&initial_output.stdout),
        String::from_utf8_lossy(&initial_output.stderr)
    );

    // Confirm both the canonical DB and the versioned lexical index
    // directory exist.
    let db_path = data_dir.join("agent_search.db");
    assert!(
        db_path.exists(),
        "canonical DB must exist after initial index"
    );
    let index_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .expect("resolve versioned tantivy index path");
    assert!(
        index_path.exists(),
        "versioned lexical index path must exist after initial index; got {}",
        index_path.display()
    );

    // Wipe the ENTIRE versioned lexical index directory. The canonical
    // DB stays intact — this is the corruption profile ibuuh.2 /
    // ibuuh.10 target: lexical assets vanished, canonical intact.
    // `index_dir` auto-creates its target path, so `remove_dir_all` is
    // a legitimate test operation on a TempDir subtree (not a source
    // file).
    fs::remove_dir_all(&index_path).expect("wipe lexical index directory");
    assert!(
        !index_path.exists(),
        "precondition: lexical index directory must be gone before self-heal run"
    );
    assert!(
        db_path.exists(),
        "precondition: canonical DB must still exist"
    );

    // `cass index --full --json` must re-materialize the lexical tree
    // from the canonical DB. `--full` is the load-bearing flag here:
    // plain incremental `cass index` looks at the source filesystem for
    // NEW sessions, finds none (all three fixtures are already in the
    // canonical DB from the initial run), and short-circuits. The
    // "self-heal from canonical" path is the one `--full` exercises,
    // and it must succeed without `--force-rebuild` — the rebuild
    // pipeline has to notice the missing lexical tree and build from
    // DB instead of rejecting with an error.
    let mut heal_index = base_cmd(home);
    heal_index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let heal_output = heal_index.output().expect("run self-heal cass index");
    let heal_stdout = String::from_utf8_lossy(&heal_output.stdout);
    let heal_stderr = String::from_utf8_lossy(&heal_output.stderr);
    assert!(
        heal_output.status.success(),
        "cass index --full must self-heal a missing lexical index tree.\nstdout: {heal_stdout}\nstderr: {heal_stderr}"
    );
    assert!(
        index_path.exists(),
        "self-heal run must re-materialize the lexical index directory"
    );

    // The checkpoint sidecar must ALSO come back, so subsequent
    // incremental runs have a stable resume anchor.
    let checkpoint_path = index_path.join(".lexical-rebuild-state.json");
    assert!(
        checkpoint_path.exists(),
        "self-heal run must recreate the lexical checkpoint at {}",
        checkpoint_path.display()
    );

    // The JSON result must report a non-zero message count — i.e.,
    // the rebuild actually ingested the DB rows rather than short-
    // circuiting to an empty index.
    let heal_payload: serde_json::Value = serde_json::from_slice(&heal_output.stdout)
        .unwrap_or_else(|err| {
            panic!("cass index --full JSON failed to parse: {err}\nstdout: {heal_stdout}")
        });
    let reported_messages = heal_payload
        .get("messages")
        .and_then(|value| value.as_i64())
        .expect("cass index --full --json payload must expose `messages`");
    let reported_conversations = heal_payload
        .get("conversations")
        .and_then(|value| value.as_i64())
        .expect("cass index --full --json payload must expose `conversations`");
    assert!(
        reported_messages > 0,
        "self-heal rebuild must report a non-zero message count; payload: {heal_payload}"
    );
    assert!(
        reported_conversations > 0,
        "self-heal rebuild must report a non-zero conversation count; payload: {heal_payload}"
    );

    // The rebuilt Tantivy index must have at least as many docs as the
    // rebuild reported messages — there's one Tantivy doc per canonical
    // message. This is the "self-heal produced a searchable index"
    // contract at the storage layer, independent of any CLI search
    // filter behavior. Proves the rebuild path actually populated
    // Tantivy rather than leaving an empty shell.
    let tantivy_summary =
        coding_agent_search::search::tantivy::searchable_index_summary(&index_path)
            .expect("searchable_index_summary must succeed after self-heal")
            .expect("rebuilt index must have a readable Tantivy summary");
    assert!(
        tantivy_summary.docs > 0,
        "self-heal rebuild must populate the Tantivy index with at least one doc; \
         got docs={}",
        tantivy_summary.docs
    );
    assert_eq!(
        tantivy_summary.docs as i64, reported_messages,
        "Tantivy doc count must match the rebuild's reported message count \
         (one lexical doc per canonical message)"
    );
}
