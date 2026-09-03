use assert_cmd::Command;
use clap::Parser;
use coding_agent_search::storage::sqlite::SqliteStorage;
use coding_agent_search::{Cli, Commands};
use fs2::FileExt;
use predicates::str::contains;
use serial_test::serial;
use std::fs;
use std::fs::OpenOptions;
use tempfile::TempDir;

mod util;

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
fn full_index_worker_overrides_the_linux_default_stack_for_franken_open()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir)?;

    // The ordinary integration harness uses a 16 MiB RUST_MIN_STACK. Pin the
    // production Linux default here so this child reproduces the debug-build
    // FrankenStorage::open overflow unless cass explicitly sizes its index
    // worker thread.
    let mut cmd = base_cmd(tmp.path());
    cmd.env("RUST_MIN_STACK", (2 * 1024 * 1024).to_string())
        .arg("index")
        .arg("--full")
        .arg("--json")
        .arg("--no-progress-events")
        .arg("--data-dir")
        .arg(&data_dir);
    let output = cmd.output()?;

    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "full index should survive FrankenStorage::open on its explicitly sized worker: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
        .into());
    }

    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if payload.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(
            std::io::Error::other(format!("full index did not report success: {payload}")).into(),
        );
    }
    if !data_dir.join("agent_search.db").is_file() {
        return Err(
            std::io::Error::other("full index did not create the canonical archive").into(),
        );
    }

    Ok(())
}

#[test]
#[serial]
fn index_watch_once_skips_advisory_locked_active_source_without_quarantine() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let source_dir = tmp.path().join("amp");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&source_dir).unwrap();
    let source_path = source_dir.join("thread-active-source.json");
    fs::write(
        &source_path,
        r#"{"id":"thread-active-source","messages":[{"role":"user","text":"still being written","createdAt":1700000000100}]}"#,
    )
    .unwrap();
    let locked_source = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&source_path)
        .unwrap();
    locked_source.lock_exclusive().unwrap();

    let output = base_cmd(tmp.path())
        .args([
            "index",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--watch-once",
            source_path.to_str().unwrap(),
            "--json",
            "--no-progress-events",
        ])
        .output()
        .expect("run cass index --watch-once");
    assert!(
        output.status.success(),
        "watch-once should skip active source successfully: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("watch-once JSON payload");
    assert_eq!(
        payload.get("conversations").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert!(
        !data_dir
            .join("quarantine/watch_ingest_poison.jsonl")
            .exists(),
        "active source skip must not create watch poison quarantine"
    );

    locked_source.unlock().unwrap();
}

#[test]
#[serial]
fn index_json_warns_for_disabled_sources_agents() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().join("data");
    let config_dir = tmp.path().join(".config").join("cass");
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&config_dir)?;
    fs::write(
        config_dir.join("sources.toml"),
        "disabled_agents = [\"claude-code\", \"codex\"]\n",
    )?;

    let mut cmd = base_cmd(tmp.path());
    cmd.arg("index")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--json")
        .arg("--no-progress-events");

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(format!(
            "cass index should still succeed when connectors are excluded\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "index stdout is not JSON: {err}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    if stdout.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("index stdout should report success=true: {stdout}").into());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let exclusion_event = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| {
            event.get("event").and_then(serde_json::Value::as_str) == Some("indexing_exclusions")
        })
        .ok_or_else(|| format!("missing indexing_exclusions event in stderr:\n{stderr}"))?;

    let expected_disabled_agents = serde_json::json!(["claude", "codex"]);
    if exclusion_event.get("disabled_agents") != Some(&expected_disabled_agents) {
        return Err(format!(
            "disabled_agents should be {expected_disabled_agents}: {exclusion_event}"
        )
        .into());
    }
    let disabled_connectors = exclusion_event
        .get("disabled_connectors")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("missing disabled_connectors array: {exclusion_event}"))?;
    if !disabled_connectors
        .iter()
        .any(|value| value.as_str() == Some("claude"))
    {
        return Err(format!(
            "claude-code alias should surface the disabled claude connector factory: {exclusion_event}"
        )
        .into());
    }
    if !disabled_connectors
        .iter()
        .any(|value| value.as_str() == Some("codex"))
    {
        return Err(
            format!("codex connector should be listed as disabled: {exclusion_event}").into(),
        );
    }
    let disabled_index_agents = exclusion_event
        .get("disabled_index_agents")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("missing disabled_index_agents array: {exclusion_event}"))?;
    if !disabled_index_agents
        .iter()
        .any(|value| value.as_str() == Some("claude_code"))
    {
        return Err(format!(
            "diagnostic should also surface the indexed claude_code agent slug: {exclusion_event}"
        )
        .into());
    }
    let expected_commands = serde_json::json!([
        "cass sources agents include claude",
        "cass sources agents include codex"
    ]);
    if exclusion_event.get("reenable_commands") != Some(&expected_commands) {
        return Err(
            format!("reenable_commands should be {expected_commands}: {exclusion_event}").into(),
        );
    }

    Ok(())
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
fn index_robot_trace_ingest_flag_parses_for_perf_bisection() -> Result<(), String> {
    let cli = parse_cli_ok(
        ["cass", "index", "--json", "--robot-trace-ingest"],
        "parse ingest trace control",
    );

    match cli.command {
        Some(Commands::Index {
            json: true,
            robot_trace_ingest: true,
            ..
        }) => Ok(()),
        other => Err(format!(
            "expected index --robot-trace-ingest to parse, got {other:?}"
        )),
    }
}

#[test]
#[serial]
fn index_robot_trace_ingest_emits_batch_ndjson_with_lookup_counters()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir)?;
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2026/05/13",
        "rollout-trace-ingest.jsonl",
        "trace_ingest_probe",
    );

    let mut seed = base_cmd(home);
    seed.args([
        "index",
        "--full",
        "--json",
        "--no-progress-events",
        "--data-dir",
    ])
    .arg(&data_dir);
    let seed_output = seed.output()?;
    assert!(
        seed_output.status.success(),
        "seed index should succeed. stdout: {}, stderr: {}",
        String::from_utf8_lossy(&seed_output.stdout),
        String::from_utf8_lossy(&seed_output.stderr)
    );

    let mut traced = base_cmd(home);
    traced
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--robot-trace-ingest",
            "--data-dir",
        ])
        .arg(&data_dir);
    let traced_output = traced.output()?;
    let stdout = String::from_utf8_lossy(&traced_output.stdout);
    let stderr = String::from_utf8_lossy(&traced_output.stderr);
    assert!(
        traced_output.status.success(),
        "traced index should succeed. stdout: {stdout}, stderr: {stderr}"
    );

    let payload: serde_json::Value = serde_json::from_slice(&traced_output.stdout)?;
    assert_eq!(
        payload.get("success").and_then(|value| value.as_bool()),
        Some(true)
    );

    let trace = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| line.get("event").and_then(|value| value.as_str()) == Some("ingest_batch"))
        .ok_or_else(|| format!("stderr should contain ingest_batch trace JSON; got: {stderr}"))?;
    assert_eq!(trace["status"], "ok");
    assert_eq!(
        trace["lexical_strategy"],
        "deferred_authoritative_db_rebuild"
    );
    assert!(
        trace["batch_n"].as_u64().unwrap_or_default() >= 1,
        "trace must include a stable batch number: {trace}"
    );
    assert!(
        trace["batch_conversations"].as_u64().unwrap_or_default() >= 1,
        "trace must include at least one normalized conversation: {trace}"
    );
    assert!(
        trace["batch_msgs"].as_u64().unwrap_or_default() >= 2,
        "trace must include normalized message count: {trace}"
    );
    assert!(
        trace["wall_ms"].as_u64().is_some(),
        "trace must include elapsed wall time: {trace}"
    );
    assert!(
        trace["lookups_against_global"].as_u64().is_some(),
        "trace must include global lookup work proxy: {trace}"
    );
    let lookup_trace = trace
        .get("lookup_trace")
        .and_then(|value| value.as_object())
        .ok_or_else(|| format!("lookup_trace object missing from trace: {trace}"))?;
    for field in [
        "exact_idx_probes",
        "bounded_lookup_queries",
        "full_scan_queries",
        "rows_materialized",
    ] {
        assert!(
            lookup_trace
                .get(field)
                .and_then(|value| value.as_u64())
                .is_some(),
            "lookup_trace.{field} must be numeric: {trace}"
        );
    }

    Ok(())
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

fn make_codex_session(
    root: &std::path::Path,
    date_path: &str,
    filename: &str,
    content: &str,
) -> std::path::PathBuf {
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
    fs::write(&file, sample).unwrap();
    file
}

#[test]
#[serial]
fn watch_once_indexes_real_aider_session_with_deferred_tantivy_open() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    // qu81y: redundant HOME guard removed — spawns pass env explicitly.
    let history_file = home.join(".aider.chat.history.md");
    fs::write(
        &history_file,
        "\n> lazywatchprobe\n\nassistant says lazywatchprobe response\n",
    )
    .unwrap();

    let mut index = base_cmd(home);
    index.current_dir(home);
    index
        .args(["index", "--watch-once"])
        .arg(&history_file)
        .args(["--json", "--data-dir"])
        .arg(&data_dir);
    let output = index.output().expect("run watch-once index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "watch-once index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("index --json should emit valid JSON");
    assert_eq!(
        payload.get("success").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        payload
            .get("messages")
            .and_then(|value| value.as_i64())
            .unwrap_or_default()
            >= 2,
        "watch-once should ingest the real session messages; payload: {payload}"
    );
    assert!(
        data_dir.join("index").exists(),
        "lazy Tantivy open should publish an index"
    );

    let mut search = base_cmd(home);
    search.current_dir(home);
    search
        .args(["search", "lazywatchprobe", "--json", "--data-dir"])
        .arg(&data_dir)
        .args(["--limit", "5", "--mode", "lexical", "--color=never"]);
    let output = search.output().expect("run search after watch-once index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "search should find the watch-once indexed session. stdout: {stdout}, stderr: {stderr}"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("search --json should emit valid JSON");
    let hits = payload["hits"]
        .as_array()
        .expect("search JSON should contain hits array");
    assert!(
        hits.iter().any(|hit| {
            hit.get("content")
                .and_then(|value| value.as_str())
                .is_some_and(|content| content.contains("lazywatchprobe"))
        }),
        "search results should include the watch-once session content; payload: {payload}"
    );
}

#[test]
#[serial]
fn index_json_reports_full_refresh_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "rollout-strategy-full.jsonl",
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "rollout-strategy-canonical.jsonl",
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "rollout-repeat-full-noop.jsonl",
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
    // Bead cxhqb: capture the checkpoint file's BYTES instead of its
    // filesystem mtime. Comparing mtimes is fragile on filesystems
    // with coarse (≥2s) granularity — the previous approach paired a
    // 5ms sleep with a "same mtime" assertion, which was always a
    // happy-path-only signal. The checkpoint JSON's own content (plus
    // embedded updated_at_ms field) changes ONLY when cass rewrites
    // the file, independent of filesystem mtime resolution, so a
    // byte-for-byte comparison is both tighter and portable.
    let checkpoint_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .unwrap()
        .join(".lexical-rebuild-state.json");
    let checkpoint_bytes_before =
        fs::read(&checkpoint_path).expect("initial lexical checkpoint must be readable");
    assert!(
        !checkpoint_bytes_before.is_empty(),
        "precondition: initial checkpoint must be non-empty"
    );

    fs::rename(&codex_home, home.join(".codex_hidden")).unwrap();

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
    let checkpoint_bytes_after =
        fs::read(&checkpoint_path).expect("preserved lexical checkpoint must be readable");
    assert_eq!(
        checkpoint_bytes_after, checkpoint_bytes_before,
        "repeat no-op full runs should preserve the matching lexical checkpoint instead \
         of deleting and recreating it (content byte-for-byte identical; file mtime is \
         fragile on coarse-granularity filesystems, content is not)"
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    // Seed a small mixed corpus so the rebuild touches multiple distinct
    // conversations and exercises the streaming accumulator beyond a trivial
    // single-conversation path.
    for (idx, content) in [
        "equivalenceledgeralpha",
        "equivalenceledgerbravo",
        "equivalenceledgercharlie",
        "equivalenceledgerdelta",
    ]
    .iter()
    .enumerate()
    {
        make_codex_session(
            &codex_home,
            "2026/04/22",
            &format!("rollout-equivalence-ledger-{idx:02}.jsonl"),
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
        .args(["search", "equivalenceledgeralpha", "--data-dir"])
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
        search_stdout.contains("equivalenceledgeralpha"),
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "rollout-strategy-incremental-1.jsonl",
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
        "rollout-strategy-incremental-2.jsonl",
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "rollout-strategy-watch-once-1.jsonl",
        "watch_once_strategy_seed",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    std::thread::sleep(std::time::Duration::from_secs(2));
    let targeted_path = codex_home.join("sessions/2025/11/25/rollout-strategy-watch-once-2.jsonl");
    make_codex_session(
        &codex_home,
        "2025/11/25",
        "rollout-strategy-watch-once-2.jsonl",
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "rollout-checkpoint-bootstrap.jsonl",
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
    // qu81y: no process-global env mutation — base_cmd(home) passes
    // HOME/XDG/CODEX_HOME (= home/.codex) explicitly to every cass child.

    // Seed three distinct sessions with a stable single-word keyword
    // each (avoid underscores — Tantivy's default tokenizer splits on
    // them and a phrase query wouldn't match after round-trip through
    // the rebuild path). The search step below probes one of these.
    for (idx, keyword) in ["alphaheal", "bravoheal", "charlieheal"].iter().enumerate() {
        make_codex_session(
            &codex_home,
            "2026/04/23",
            &format!("rollout-self-heal-fixture-{idx:02}.jsonl"),
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

/// GH#413 (bead cjugu) end-to-end receipt: a #413-shaped corpus — few
/// conversations, one far exceeding the in-flight byte budget — must complete
/// `cass index --full` instead of wedging at a batch boundary. Before the
/// sink starvation flush (424765b3), `pending_batch` retained byte
/// reservations with only count and shard-boundary flush triggers, so once
/// the oversized page was retained downstream the turn-holding page-prep
/// worker parked in `acquire_with_wait` forever (every later sequence in
/// `wait_for_turn`, producer in waiting_result, ~0 CPU). A tiny
/// `CASS_TANTIVY_REBUILD_PIPELINE_MAX_MESSAGE_BYTES_IN_FLIGHT` below the
/// oversized conversation's page bytes makes that wedge deterministic on a
/// pre-fix binary; the starvation flush must release the budget and let the
/// run drain.
///
/// Currently ignore-gated: on current main the ingest path parks on the
/// oversized conversation itself (heartbeat shows `rebuild_pipeline` fully
/// idle — `page_prep_workers=0`, `producer_state=null` — while ingest crawls
/// at ~0.02 conv/s), so the run never reaches the lexical pipeline phase
/// where the GH#413 retention wedge lives. That is the same mwkw0-class
/// ingest park MossyBridge documented when first attempting a tiny GH#413
/// repro, not this fix. Un-ignore once ingest bounding lands; this then
/// becomes the definitive pipeline-level receipt for cjugu.
#[ignore = "ingest path parks on multi-MiB conversations before the lexical pipeline starts (mwkw0-class); see bead cjugu comments"]
#[test]
fn gh413_full_rebuild_drains_when_one_conversation_exceeds_the_inflight_budget() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let codex_root = data_dir.join(".codex").join("sessions");
    fs::create_dir_all(&codex_root).unwrap();

    let huge_marker = "gh413-huge-needle";
    let filler_line = "gh413 filler 0123456789012345678901234567890123456789\n";
    let mut huge_text = String::with_capacity(5 * 1024 * 1024);
    while huge_text.len() < 4 * 1024 * 1024 {
        huge_text.push_str(filler_line);
    }
    huge_text.push_str(huge_marker);

    let write_session = |name: &str, user_text: &str, session_id: &str| {
        let sample = format!(
            concat!(
                "{{\"timestamp\":\"2025-09-30T15:42:34.559Z\",\"type\":\"session_meta\",",
                "\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/test/workspace\",\"cli_version\":\"0.42.0\"}}}}\n",
                "{{\"timestamp\":\"2025-09-30T15:42:36.190Z\",\"type\":\"response_item\",",
                "\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",",
                "\"text\":\"{user_text}\"}}]}}}}\n",
                "{{\"timestamp\":\"2025-09-30T15:42:43.000Z\",\"type\":\"response_item\",",
                "\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",",
                "\"text\":\"acknowledged\"}}]}}}}\n"
            ),
            session_id = session_id,
            user_text = user_text
        );
        fs::write(codex_root.join(name), sample).unwrap();
    };
    write_session("rollout-huge.jsonl", &huge_text, "gh413-huge");
    write_session(
        "rollout-small-a.jsonl",
        "gh413 small marker alpha",
        "gh413-a",
    );
    write_session(
        "rollout-small-b.jsonl",
        "gh413 small marker beta",
        "gh413-b",
    );

    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.args([
        "index",
        "--full",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
        "--no-progress-events",
    ])
    .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
    .env("HOME", tmp.path())
    .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
    .env("CODEX_HOME", data_dir.join(".codex"))
    // Far below the oversized conversation's ~4 MiB page: after the huge
    // page is retained by the sink, every later budget acquisition must
    // depend on the starvation flush releasing retained bytes, which is
    // exactly the pre-fix wedge site.
    .env(
        "CASS_TANTIVY_REBUILD_PIPELINE_MAX_MESSAGE_BYTES_IN_FLIGHT",
        "2097152",
    );
    let stdout_path = tmp.path().join("gh413-index-stdout.json");
    let stderr_path = tmp.path().join("gh413-index-stderr.log");
    let stdout_file = fs::File::create(&stdout_path).unwrap();
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    cmd.stdout(stdout_file).stderr(stderr_file);
    let mut child = cmd.spawn().expect("spawn cass index --full");

    // Bounded wait: the pre-fix wedge was a permanent park with ~0 CPU, so a
    // generous wall-clock deadline turns a regression into a failed test
    // instead of a hung suite.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let status = loop {
        match child.try_wait().expect("poll cass index") {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "GH#413 regression: index --full wedged past the 600s deadline on a \
                         #413-shaped corpus (one conversation exceeding the in-flight budget)"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    };
    assert!(
        status.success(),
        "index --full must succeed on the GH#413-shaped corpus (exit: {status:?}); \
         stderr tail: {}",
        fs::read_to_string(&stderr_path)
            .map(|log| {
                let tail: String = log.chars().rev().take(12000).collect();
                tail.chars().rev().collect()
            })
            .unwrap_or_else(|err| format!("<unreadable: {err}>"))
    );

    // Completeness: the huge and the small conversations must all be
    // searchable after the drain.
    for (query, needle) in [
        (huge_marker, huge_marker),
        ("gh413 small marker alpha", "gh413 small marker alpha"),
        ("gh413 small marker beta", "gh413 small marker beta"),
    ] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args([
                "search",
                query,
                "--json",
                "--data-dir",
                data_dir.to_str().unwrap(),
            ])
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("HOME", tmp.path())
            .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
            .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
            .env("CODEX_HOME", data_dir.join(".codex"))
            .output()
            .expect("run cass search");
        assert!(
            output.status.success(),
            "search for {query:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hits = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .expect("parse search json")["hits"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            hits.iter().any(|hit| {
                hit.get("content")
                    .and_then(|content| content.as_str())
                    .is_some_and(|content| content.contains(needle))
            }),
            "expected a search hit containing {needle:?} after the GH#413-shaped drain"
        );
    }
}

/// GH #439 / WS-B.2c: the post-publish fallback-FTS repair runs after the
/// lexical generation is published, in phase 0, and used to emit no liveness
/// signal — so the stall watchdog killed healthy `cass index --full` runs on
/// large archives with exit 70. The repair now ticks the indexer heartbeat per
/// page. This test pins the contract on both sides with the watchdog cranked
/// down to seconds:
///
/// - Positive observable: with `CASS_FTS_REBUILD_BATCH_SIZE=1` and a 4 s sleep
///   per page, a four-message archive spends about 20 s inside the repair
///   (five pages) under a 20 s abort threshold, and still exits 0, because
///   every page
///   ticks the heartbeat. The elapsed-time floor proves the repair actually
///   paged (a skipped repair would finish in well under 8 s and fail here).
/// - Planted negative: a repair that parks for 40 s before its first page
///   without ticking is the shape of a genuine wedge; the watchdog aborts it
///   with exit 70 and the `index-stalled` error envelope on stderr.
///
/// No-claim: this does not measure the reporter's 5,256-conversation corpus;
/// it proves the liveness mechanism, not its scale.
fn fts_repair_liveness_index_cmd(home: &std::path::Path, data_dir: &std::path::Path) -> Command {
    let mut cmd = base_cmd(home);
    cmd.current_dir(home);
    cmd.args([
        "index",
        "--full",
        "--json",
        "--no-progress-events",
        "--progress-interval-ms",
        "250",
        "--data-dir",
    ])
    .arg(data_dir)
    .env("CASS_AUTO_REFRESH", "0")
    .env("CASS_FTS_REBUILD_BATCH_SIZE", "1")
    // Loose enough for the pre-index phases on a loaded fleet worker (the
    // first attempt used 2 s / 6 s and was aborted during connector scanning),
    // tight enough that the repair's 20 s of injected work exceeds it.
    .env("CASS_INDEX_STALL_DETECT_SECS", "5")
    .env("CASS_INDEX_STALL_ABORT_SECS", "20")
    .env("CASS_INDEX_FINALIZE_ABORT_SECS", "20");
    cmd
}

fn seed_fts_liveness_sessions(home: &std::path::Path) {
    let codex_root = home.join(".codex");
    make_codex_session(
        &codex_root,
        "2026/09/01",
        "rollout-liveness-a.jsonl",
        "fts liveness alpha",
    );
    make_codex_session(
        &codex_root,
        "2026/09/01",
        "rollout-liveness-b.jsonl",
        "fts liveness beta",
    );
}

/// GH #413 follow-up (iify0): once this run's inline `fts_messages` shadow
/// writes exceed their budget the run skips the shadow and still completes:
/// the new sessions are searchable through the Quill index, the run exits 0,
/// and the reason is persisted where `status` (and doctor's snapshot) read
/// it. Plants a 1 s budget and a 1.5 s park inside the first flush, so the
/// first new conversation's flush trips the budget and the second one's is
/// skipped. (`--json` pins the stderr log filter, so the warn line is not an
/// observable here.) The negative control is a fresh archive indexed within
/// budget: no marker.
#[test]
fn gh413_inline_fts_shadow_writes_past_their_budget_suspend_and_the_run_still_completes() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_fts_liveness_sessions(home);
    let codex_root = home.join(".codex");

    let full = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("seed the archive with a full index");
    assert!(
        full.status.success(),
        "seed full index failed: {}",
        String::from_utf8_lossy(&full.stderr)
    );

    make_codex_session(
        &codex_root,
        "2026/09/02",
        "rollout-liveness-gamma.jsonl",
        "fts liveness gamma budgetprobe",
    );
    make_codex_session(
        &codex_root,
        "2026/09/02",
        "rollout-liveness-delta.jsonl",
        "fts liveness delta budgetprobe",
    );
    let parked = base_cmd(home)
        .current_dir(home)
        .args(["index", "--json", "--no-progress-events", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_TEST_FTS_INLINE_FLUSH_PARK_MS", "1500")
        .env("CASS_FTS_INLINE_BUDGET_SECS", "1")
        .output()
        .expect("run an incremental index whose first shadow flush blows the budget");
    let stderr = String::from_utf8_lossy(&parked.stderr);
    let stdout = String::from_utf8_lossy(&parked.stdout);
    assert!(
        parked.status.success(),
        "a run whose shadow writes blew their budget must still complete; status={:?}\n\
         stdout={stdout}\nstderr={stderr}",
        parked.status
    );

    // Both new conversations landed: Quill (the lexical engine search uses)
    // finds them although the shadow skipped at least one of them.
    let search = base_cmd(home)
        .current_dir(home)
        .args(["search", "budgetprobe", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("search the sessions indexed by the suspended run");
    assert!(
        search.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&search.stdout).expect("search --json output is JSON");
    let hits = json["hits"].as_array().expect("hits array");
    assert_eq!(
        hits.len(),
        2,
        "both sessions from the suspended run must be searchable through Quill: {json}"
    );

    // The reason is persisted where status (and doctor's snapshot) read it.
    let status = base_cmd(home)
        .current_dir(home)
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("status after the suspended run");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status --json output is JSON");
    let pending = &status_json["index"]["fallback_fts_repair"];
    assert_eq!(
        pending["pending"],
        serde_json::json!(true),
        "status must carry the suspended-shadow marker: {status_json}"
    );
    assert!(
        pending["detail"].as_str().is_some_and(|detail| {
            detail.contains("shadow writes suspended") && detail.contains("1 s budget")
        }),
        "the persisted detail names the suspension and the budget: {pending}"
    );

    // Negative control: a fresh archive whose incremental run stays inside the
    // same 1 s budget (no park) carries no marker at all.
    let control_tmp = TempDir::new().unwrap();
    let control_home = control_tmp.path();
    let control_data_dir = control_home.join("cass_data");
    fs::create_dir_all(&control_data_dir).unwrap();
    seed_fts_liveness_sessions(control_home);
    let control_full = base_cmd(control_home)
        .current_dir(control_home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&control_data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("seed the control archive");
    assert!(
        control_full.status.success(),
        "control seed failed: {}",
        String::from_utf8_lossy(&control_full.stderr)
    );
    make_codex_session(
        &control_home.join(".codex"),
        "2026/09/02",
        "rollout-liveness-epsilon.jsonl",
        "fts liveness epsilon controlprobe",
    );
    let control = base_cmd(control_home)
        .current_dir(control_home)
        .args(["index", "--json", "--no-progress-events", "--data-dir"])
        .arg(&control_data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_FTS_INLINE_BUDGET_SECS", "1")
        .output()
        .expect("run an incremental index within budget");
    assert!(
        control.status.success(),
        "control run failed: {}",
        String::from_utf8_lossy(&control.stderr)
    );
    let control_status = base_cmd(control_home)
        .current_dir(control_home)
        .args(["status", "--json", "--data-dir"])
        .arg(&control_data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("status after the control run");
    let control_json: serde_json::Value =
        serde_json::from_slice(&control_status.stdout).expect("control status is JSON");
    assert!(
        control_json["index"].get("fallback_fts_repair").is_none(),
        "a run inside the budget must leave no marker: {control_json}"
    );
}

/// GH #413 follow-up (iify0): the paged post-publish shadow repair stops at
/// its per-page budget instead of wedging, the `index --full` run still exits
/// 0, and the reason is persisted where `status` reads it. The #439 PAGE_SLEEP
/// hook stands in for a slow engine page (3 s against a 1 s budget), so the
/// repair stops after its first page.
#[test]
fn gh413_paged_fts_shadow_repair_stops_at_its_page_budget_without_failing_the_run() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_fts_liveness_sessions(home);

    let started = std::time::Instant::now();
    // Not `fts_repair_liveness_index_cmd`: its 5 s / 20 s stall settings are
    // calibrated for the #439 tests and aborted this run's `preparing` phase
    // (exit 70) on a loaded debug worker before the repair began (verify35).
    // The stall watchdog is not under test here; the page budget is.
    let output = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_FTS_REBUILD_BATCH_SIZE", "1")
        .env("CASS_TEST_FTS_REPAIR_PAGE_SLEEP_MS", "3000")
        .env("CASS_FTS_REPAIR_PAGE_BUDGET_SECS", "1")
        .output()
        .expect("run cass index --full with a repair page over its budget");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a repair page over its budget must not fail the run; status={:?} elapsed={elapsed:?}\n\
         stdout={stdout}\nstderr={stderr}",
        output.status
    );

    let status = base_cmd(home)
        .current_dir(home)
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("status after the budget-stopped repair");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status --json output is JSON");
    let pending = &status_json["index"]["fallback_fts_repair"];
    assert_eq!(
        pending["pending"],
        serde_json::json!(true),
        "status must carry the stopped-repair marker: {status_json}\nstderr={stderr}"
    );
    assert!(
        pending["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("per-page budget")),
        "the persisted detail names the page budget: {pending}"
    );
}

/// GH #413 follow-up (iify0): with the shadow bound set below this fixture's
/// corpus, `index --full` refuses to (re)create the SQL-fallback shadow, says
/// so where `status` and `doctor` read it, and search still answers through
/// Quill. Raising the bound lets the next full run recreate it and clears the
/// marker.
#[test]
fn gh413_fts_shadow_over_its_corpus_bound_is_dropped_and_recreated_once_it_fits() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_fts_liveness_sessions(home);

    // Seed with an unbounded shadow first, so the bounded run below exercises
    // the preflight drop of an existing, populated shadow.
    let seed = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_FTS_SHADOW_MAX_CONTENT_BYTES", "0")
        .output()
        .expect("seed the archive with a shadow");
    assert!(
        seed.status.success(),
        "seed failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );

    let bounded = base_cmd(home)
        .current_dir(home)
        .args(["index", "--json", "--no-progress-events", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_FTS_SHADOW_MAX_CONTENT_BYTES", "1")
        .output()
        .expect("run an incremental index under a 1-byte shadow bound");
    assert!(
        bounded.status.success(),
        "dropping an oversized shadow must not fail the run: {}",
        String::from_utf8_lossy(&bounded.stderr)
    );

    let status = base_cmd(home)
        .current_dir(home)
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("status after the drop");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status --json output is JSON");
    let pending = &status_json["index"]["fallback_fts_repair"];
    assert_eq!(
        pending["pending"],
        serde_json::json!(true),
        "status must carry the dropped-shadow marker: {status_json}"
    );
    assert!(
        pending["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("not viable on this engine")),
        "the persisted detail names the bound: {pending}"
    );

    let doctor = base_cmd(home)
        .current_dir(home)
        .args(["doctor", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("doctor after the drop");
    let doctor_json: serde_json::Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor --json output is JSON");
    let fts_check = doctor_json["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == "fts_table"))
        .cloned()
        .unwrap_or_else(|| panic!("doctor must report an fts_table check: {doctor_json}"));
    assert_eq!(fts_check["status"], "pass", "{fts_check}");
    assert!(
        fts_check["message"]
            .as_str()
            .is_some_and(|message| message.contains("dropped on purpose")),
        "doctor says the drop was deliberate: {fts_check}"
    );

    let search = base_cmd(home)
        .current_dir(home)
        .args(["search", "liveness", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("search without a shadow");
    assert!(
        search.status.success(),
        "search failed without the shadow: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_json: serde_json::Value =
        serde_json::from_slice(&search.stdout).expect("search --json output is JSON");
    assert_eq!(
        search_json["hits"].as_array().map_or(0, Vec::len),
        2,
        "Quill answers the search with the shadow gone: {search_json}"
    );

    // The corpus fits again (bound lifted): the next full run recreates the
    // shadow and clears the marker.
    let recreate = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_FTS_SHADOW_MAX_CONTENT_BYTES", "0")
        .output()
        .expect("full index with the bound lifted");
    assert!(
        recreate.status.success(),
        "recreate run failed: {}",
        String::from_utf8_lossy(&recreate.stderr)
    );
    let status = base_cmd(home)
        .current_dir(home)
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("status after the recreate");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status --json output is JSON");
    assert!(
        status_json["index"].get("fallback_fts_repair").is_none(),
        "a recreated shadow leaves no marker: {status_json}"
    );
}

#[test]
fn gh439_slow_post_publish_fts_repair_is_not_aborted_while_it_heartbeats() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_fts_liveness_sessions(home);

    let started = std::time::Instant::now();
    // Discrimination: four pages at 12 s each = 48 s of repair work under a
    // 40 s abort window. With the per-page heartbeat every silent stretch is
    // 12 s (< 40 s) and the run survives; without it the 48 s stretch would
    // exceed the window and abort. The window is deliberately wider than the
    // parked variant's 20 s so a slow `preparing` phase on a loaded fleet
    // worker (debug build) cannot trip the abort before the repair starts.
    let output = fts_repair_liveness_index_cmd(home, &data_dir)
        .env("CASS_TEST_FTS_REPAIR_PAGE_SLEEP_MS", "12000")
        .env("CASS_INDEX_STALL_DETECT_SECS", "10")
        .env("CASS_INDEX_STALL_ABORT_SECS", "40")
        .env("CASS_INDEX_FINALIZE_ABORT_SECS", "40")
        .output()
        .expect("run cass index --full with a slow FTS repair");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a slow-but-heartbeating post-publish FTS repair must not be aborted (GH #439); \
         status={:?} elapsed={elapsed:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(48),
        "the repair must actually have paged with the injected sleep (four messages, one \
         per page, 12 s each); elapsed={elapsed:?}\nstderr={stderr}"
    );
    // `index --json` interleaves single-line liveness events with the run's
    // pretty-printed summary, so the summary is the last JSON *document* on
    // stdout, not the last line.
    let mut documents =
        serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    let mut payload = None;
    while let Some(Ok(document)) = documents.next() {
        payload = Some(document);
    }
    let payload = payload.unwrap_or_else(|| {
        panic!("index --json wrote no JSON summary\nstdout={stdout}\nstderr={stderr}")
    });
    assert_eq!(payload["success"].as_bool(), Some(true), "{payload}");
    assert!(
        payload["messages"].as_i64().unwrap_or_default() >= 4,
        "both seeded sessions must be ingested: {payload}"
    );
}

/// GH #382 / g3zyo: the index run's final `wal_checkpoint(TRUNCATE)` is bounded.
/// On an archive whose frankensqlite writable path loops, that checkpoint never
/// returned and every run hung after a successful publish. Positive observable:
/// with the checkpoint parked past a 1 s budget the run still exits 0 within
/// seconds and leaves the WAL sidecar in place (non-empty) for the next opener;
/// a plain `cass index` afterwards, unparked, truncates it. Planted negative:
/// the unparked run truncating the sidecar is what proves the parked run really
/// skipped the checkpoint rather than never issuing one. No-claim: this proves
/// the bound, not that the engine no longer loops (that is frankensqlite
/// 8d012706a, consumed with its release).
#[test]
fn gh382_final_wal_checkpoint_is_bounded_and_leaves_the_wal_for_the_next_run() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_fts_liveness_sessions(home);
    let wal_path = data_dir.join("agent_search.db-wal");

    let started = std::time::Instant::now();
    let output = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        // Discrimination by wall clock: a full run of this fixture takes up
        // to ~30 s on a loaded debug-build fleet worker, so the park is 60 s
        // against a 1 s budget — a run that waited for the parked checkpoint
        // cannot finish under 60 s; a bounded one finishes in the base time
        // plus one second.
        .env("CASS_TEST_WAL_CHECKPOINT_PARK_MS", "60000")
        .env("CASS_INDEX_FINAL_WAL_CHECKPOINT_TIMEOUT_SECS", "1")
        .output()
        .expect("run cass index --full with a parked final checkpoint");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a final checkpoint that outlives its budget must not fail the run (GH #382); \
         status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    let parked_elapsed = elapsed;
    // A WAL that still carries frames is larger than its 32-byte header; a
    // truncated one (frankensqlite keeps the header on TRUNCATE) is exactly 32.
    const WAL_HEADER_BYTES: u64 = 32;
    let wal_bytes_after_parked_run = fs::metadata(&wal_path).map(|meta| meta.len()).unwrap_or(0);
    assert!(
        wal_bytes_after_parked_run > WAL_HEADER_BYTES,
        "the skipped checkpoint must leave the WAL frames for the next opener; \
         wal_bytes={wal_bytes_after_parked_run}\nstderr={stderr}"
    );

    let started = std::time::Instant::now();
    let output = base_cmd(home)
        .current_dir(home)
        .args(["index", "--json", "--no-progress-events", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("run a plain cass index without the park");
    let plain_elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "the next plain run must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The bound, measured against the same worker's load: a run that waited
    // for the 60 s park would take at least the plain run plus 60 s; a
    // bounded one takes the plain run plus about a second (the budget) plus
    // the full run's own base cost, which reached 38 s over the plain
    // incremental run with two parking siblings on a loaded debug worker
    // (verify35). 50 s keeps the discrimination: the parked path is at least
    // 60 s over the base.
    assert!(
        parked_elapsed < plain_elapsed + std::time::Duration::from_secs(50),
        "the parked run must return once the 1 s checkpoint budget passes, not wait for \
         the 60 s park; parked={parked_elapsed:?} plain={plain_elapsed:?}\nstderr={stderr}"
    );
    let wal_bytes_after_plain_run = fs::metadata(&wal_path).map(|meta| meta.len()).unwrap_or(0);
    assert!(
        wal_bytes_after_plain_run <= WAL_HEADER_BYTES
            && wal_bytes_after_plain_run < wal_bytes_after_parked_run,
        "the next unparked run must truncate the WAL the bounded run left behind \
         (header only, at most {WAL_HEADER_BYTES} bytes); before={wal_bytes_after_parked_run} \
         after={wal_bytes_after_plain_run}"
    );
}

#[test]
fn gh439_parked_post_publish_fts_repair_still_aborts_with_the_index_stalled_envelope() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_fts_liveness_sessions(home);

    let output = fts_repair_liveness_index_cmd(home, &data_dir)
        .env("CASS_TEST_FTS_REPAIR_PARK_MS", "40000")
        .output()
        .expect("run cass index --full with a parked FTS repair");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(70),
        "a repair that parks without heartbeating is a wedge and must still abort; \
         status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    let envelope = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .find(|value| value["kind"].as_str() == Some("index-stalled"))
        .unwrap_or_else(|| panic!("no index-stalled envelope on stderr:\n{stderr}"));
    assert_eq!(envelope["success"].as_bool(), Some(false));
    assert_eq!(envelope["code"].as_i64(), Some(70));
    assert_eq!(envelope["retryable"].as_bool(), Some(true));
    assert!(
        envelope["stall_elapsed_ms"]
            .as_u64()
            .is_some_and(|ms| ms >= 20_000),
        "{envelope}"
    );
    assert!(envelope["phase"].is_string(), "{envelope}");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| !hint.is_empty()),
        "{envelope}"
    );
}

/// GH #440 / WS-B.3: a `--force-rebuild` interrupted after a staged engine
/// commit but before the checkpoint write leaves the staging MANIFEST ahead
/// of `.lexical-rebuild-state.json`. v0.7.1 then re-inserted already-live
/// identities on the next plain `cass index`, the engine refused the
/// duplicates, and the run exited 9. The resume path now reconciles the gap
/// through identity-idempotent upserts.
///
/// - Precondition, proven not assumed: the kill hook records the gap it
///   opened (`committed_indexed_docs > checkpoint_indexed_docs`) and the test
///   asserts it before killing, so a green run cannot come from an interrupt
///   that happened to land outside the window.
/// - Positive observable: the next plain `cass index` exits 0 and a lexical
///   search finds every seeded session exactly once (no duplicate identities,
///   nothing lost).
///
/// No-claim: this is a six-session fixture, not the reporter's archive; it
/// proves the resume contract, not its scale.
#[test]
fn gh440_plain_index_resumes_a_force_rebuild_killed_between_commit_and_checkpoint() {
    use std::process::{Command as StdCommand, Stdio};

    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let codex_root = home.join(".codex");
    let sessions = 6_usize;
    for n in 0..sessions {
        make_codex_session(
            &codex_root,
            "2026/09/02",
            &format!("rollout-resume-{n}.jsonl"),
            &format!("resumeprobe session {n}"),
        );
    }

    // A live generation first, so the force-rebuild builds into staging.
    let initial = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("initial full index");
    assert!(
        initial.status.success(),
        "initial index failed: stdout={} stderr={}",
        String::from_utf8_lossy(&initial.stdout),
        String::from_utf8_lossy(&initial.stderr)
    );

    // Force-rebuild with one commit per conversation and park after the
    // second commit, inside the commit-to-checkpoint window.
    let sentinel = data_dir.join("gh440-kill-sentinel.json");
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin!("cass"))
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--force-rebuild",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", &codex_root)
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
            &sentinel,
        )
        .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "2")
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
            "60000",
        )
        .env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "1",
        )
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            "1",
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn force-rebuild");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let payload: serde_json::Value = loop {
        if let Ok(raw) = fs::read(&sentinel)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw)
        {
            break value;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("force-rebuild exited ({status:?}) before reaching the kill window");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "force-rebuild never reached the second staged commit"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let checkpoint_docs = payload["checkpoint_indexed_docs"].as_u64().unwrap_or(0);
    let committed_docs = payload["committed_indexed_docs"].as_u64().unwrap_or(0);
    assert!(
        committed_docs > checkpoint_docs,
        "the kill window must have the authority ahead of the checkpoint: {payload}"
    );
    child.kill().expect("kill parked force-rebuild");
    let _ = child.wait();

    // The next plain index must resume through the gap, not exit 9.
    let resumed = base_cmd(home)
        .current_dir(home)
        .args(["index", "--json", "--no-progress-events", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("plain index after kill");
    let stdout = String::from_utf8_lossy(&resumed.stdout);
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "plain index after an interrupted force-rebuild must exit 0 (GH #440); \
         stdout={stdout}\nstderr={stderr}"
    );

    let search = base_cmd(home)
        .current_dir(home)
        .args([
            "search",
            "resumeprobe",
            "--json",
            "--limit",
            "50",
            "--mode",
            "lexical",
        ])
        .args(["--color=never", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("search after resume");
    let search_json: serde_json::Value =
        serde_json::from_slice(&search.stdout).unwrap_or_else(|err| {
            panic!(
                "search json: {err}\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&search.stdout),
                String::from_utf8_lossy(&search.stderr)
            )
        });
    let hits = search_json["hits"].as_array().expect("hits array");
    // Each hit is one message; every session has two messages carrying the
    // probe word. Identity = (source_path, line_number): a duplicate identity
    // surviving the resume shows up as the same pair twice.
    let mut identities: Vec<(String, i64)> = hits
        .iter()
        .map(|hit| {
            (
                hit["source_path"].as_str().unwrap_or_default().to_string(),
                hit["line_number"].as_i64().unwrap_or_default(),
            )
        })
        .collect();
    let total_hits = identities.len();
    identities.sort();
    identities.dedup();
    assert_eq!(
        identities.len(),
        total_hits,
        "no duplicate identities may survive the resume: {search_json}"
    );
    let mut sources: Vec<&str> = identities.iter().map(|(path, _)| path.as_str()).collect();
    sources.dedup();
    assert_eq!(
        sources.len(),
        sessions,
        "every seeded session must be found after resume: {search_json}"
    );
}

/// GH #441 / WS-B.1b: an archive that fragmented under v0.7.1 (one Quill
/// segment per session, hundreds of them) is repaired by an ordinary
/// `cass index`, and the observation surfaces tell the operator before and
/// after. The fragmented state is built deliberately with the
/// `CASS_TEST_SKIP_POST_RUN_LEXICAL_MAINTENANCE` hook (a full rebuild that
/// commits per conversation and skips the final merge), which is exactly the
/// on-disk shape of the reporter's and the owner's archives.
///
/// - Planted state, asserted not assumed: after the fragmenting build,
///   `status --json` reports `index.segment_files` above the pressure bound
///   and `doctor --json` reports `index_segments` as a warning that names
///   `cass index --full`.
/// - Positive observable: after one more session is ingested by a plain
///   `cass index` (no flags, no hook), the post-run maintenance folds the
///   generation below the bound, doctor's `index_segments` passes, and a
///   lexical search finds every session.
///
/// No-claim: the query-fuel exhaustion the reporter hit needs an archive far
/// larger than this fixture; this proves consolidation and its reporting.
#[test]
fn gh441_plain_index_consolidates_a_fragmented_generation_and_doctor_reports_it() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let codex_root = home.join(".codex");
    let fragmented_sessions = 40_usize;
    for n in 0..fragmented_sessions {
        make_codex_session(
            &codex_root,
            "2026/09/02",
            &format!("rollout-frag-{n}.jsonl"),
            &format!("fragmentprobe session {n}"),
        );
    }

    let status_segment_files = |data_dir: &std::path::Path| -> u64 {
        let out = base_cmd(home)
            .current_dir(home)
            .args(["status", "--json", "--data-dir"])
            .arg(data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .output()
            .expect("cass status --json");
        let payload: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
                panic!(
                    "status json: {err}\nstdout={}\nstderr={}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        payload["index"]["segment_files"]
            .as_u64()
            .unwrap_or_else(|| panic!("index.segment_files must be an integer: {payload}"))
    };
    let doctor_index_segments = |data_dir: &std::path::Path| -> serde_json::Value {
        let out = base_cmd(home)
            .current_dir(home)
            .args(["doctor", "--json", "--data-dir"])
            .arg(data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .output()
            .expect("cass doctor --json");
        let payload: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
                panic!(
                    "doctor json: {err}\nstdout={}\nstderr={}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        payload["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|check| check["name"].as_str() == Some("index_segments"))
            .cloned()
            .unwrap_or_else(|| panic!("index_segments check missing: {payload}"))
    };

    // Fragmenting build: one commit per conversation, final merge skipped.
    let fragment = base_cmd(home)
        .current_dir(home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_TEST_SKIP_POST_RUN_LEXICAL_MAINTENANCE", "1")
        .env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "1",
        )
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            "1",
        )
        .output()
        .expect("fragmenting full index");
    assert!(
        fragment.status.success(),
        "fragmenting build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&fragment.stdout),
        String::from_utf8_lossy(&fragment.stderr)
    );
    let before = status_segment_files(&data_dir);
    assert!(
        before > 32,
        "the planted fragmentation must exceed the pressure bound (segment_files={before})"
    );
    let warn = doctor_index_segments(&data_dir);
    assert_eq!(warn["status"].as_str(), Some("warn"), "{warn}");
    assert_eq!(warn["fix_available"].as_bool(), Some(true), "{warn}");
    assert!(
        warn["message"]
            .as_str()
            .is_some_and(|m| m.contains("cass index --full")),
        "the warning must name the remedy: {warn}"
    );

    // One more session, then an ordinary incremental run: the post-run
    // maintenance must fold the generation.
    make_codex_session(
        &codex_root,
        "2026/09/02",
        "rollout-frag-late.jsonl",
        "fragmentprobe late",
    );
    let consolidate = base_cmd(home)
        .current_dir(home)
        .args(["index", "--json", "--no-progress-events", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("plain incremental index");
    assert!(
        consolidate.status.success(),
        "plain index failed: stdout={} stderr={}",
        String::from_utf8_lossy(&consolidate.stdout),
        String::from_utf8_lossy(&consolidate.stderr)
    );
    // Files on disk are an upper bound: the engine keeps folded inputs around
    // for a while after a merge (observed: 40 files before, 42 after a fold
    // that left two live segments). The number a query pays for is the live
    // segment count from the engine's reader, so that is what consolidation
    // is judged on; `status.index.segment_files` stays the disk footprint.
    let index_dir = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
    let live_after = coding_agent_search::search::quill_bridge::live_segment_count(&index_dir)
        .expect("published Quill index after the incremental run");
    assert!(
        live_after <= 32,
        "post-run maintenance must consolidate the generation (files before={before}, live segments after={live_after})"
    );
    let pass = doctor_index_segments(&data_dir);
    assert_eq!(pass["status"].as_str(), Some("pass"), "{pass}");

    let search = base_cmd(home)
        .current_dir(home)
        .args([
            "search",
            "fragmentprobe",
            "--json",
            "--limit",
            "200",
            "--mode",
            "lexical",
        ])
        .args(["--color=never", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_AUTO_REFRESH", "0")
        .output()
        .expect("search after consolidation");
    let search_json: serde_json::Value =
        serde_json::from_slice(&search.stdout).expect("search json");
    let mut sources: Vec<&str> = search_json["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .filter_map(|hit| hit["source_path"].as_str())
        .collect();
    sources.sort_unstable();
    sources.dedup();
    assert_eq!(
        sources.len(),
        fragmented_sessions + 1,
        "every session must survive consolidation: {search_json}"
    );
}
