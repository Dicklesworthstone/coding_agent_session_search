use std::path::Path;
use std::time::Duration;

use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::compat::{ConnectionExt, RowExt};
use serde_json::Value;
use tempfile::TempDir;

fn cass_bin() -> String {
    std::env::var("CARGO_BIN_EXE_cass")
        .ok()
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_cass").to_string())
}

fn run_index_full(
    data_dir: &Path,
    home_dir: &Path,
    xdg_data: &Path,
    xdg_config: &Path,
) -> (std::process::Output, String, String) {
    let mut cmd = std::process::Command::new(cass_bin());
    cmd.arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(data_dir)
        .current_dir(home_dir)
        .env("HOME", home_dir)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("CODEX_HOME", data_dir.join(".codex"));
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run full index");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    (output, stdout, stderr)
}

fn run_watch_once(
    paths: &[&Path],
    data_dir: &Path,
    home_dir: &Path,
    xdg_data: &Path,
    xdg_config: &Path,
) -> (std::process::Output, String, String) {
    run_watch_once_with_env(paths, data_dir, home_dir, xdg_data, xdg_config, &[])
}

fn run_watch_once_with_env(
    paths: &[&Path],
    data_dir: &Path,
    home_dir: &Path,
    xdg_data: &Path,
    xdg_config: &Path,
    extra_env: &[(&str, &str)],
) -> (std::process::Output, String, String) {
    let mut cmd = std::process::Command::new(cass_bin());
    cmd.arg("index")
        .arg("--watch")
        .arg("--watch-once")
        .arg(
            paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .arg("--data-dir")
        .arg(data_dir)
        .env("HOME", home_dir)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("CODEX_HOME", data_dir.join(".codex"));
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run watch");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    (output, stdout, stderr)
}

fn run_robot_search(
    query: &str,
    data_dir: &Path,
    home_dir: &Path,
    xdg_data: &Path,
    xdg_config: &Path,
) -> Value {
    let mut cmd = std::process::Command::new(cass_bin());
    cmd.arg("search")
        .arg(query)
        .arg("--json")
        .arg("--data-dir")
        .arg(data_dir)
        .env("HOME", home_dir)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("CODEX_HOME", data_dir.join(".codex"));
    let output = cmd.output().expect("run search");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "search failed for query {query:?}\nstderr:\n{stderr}"
    );
    serde_json::from_slice(&output.stdout).expect("parse search json")
}

fn content_hit_count(search_json: &Value, needle: &str) -> usize {
    search_json["hits"].as_array().map_or(0, |hits| {
        hits.iter()
            .filter(|hit| {
                hit.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains(needle))
            })
            .count()
    })
}

fn write_codex_session(path: &Path, user_text: &str, session_id: &str) {
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
    std::fs::write(path, sample).expect("write codex session");
}

fn write_claude_session(path: &Path, user_text: &str) {
    let sample = format!(
        concat!(
            "{{\"type\":\"user\",\"cwd\":\"/workspace\",\"sessionId\":\"sess-1\",\"gitBranch\":\"main\",",
            "\"message\":{{\"role\":\"user\",\"content\":\"{user_text}\"}},",
            "\"timestamp\":\"2025-11-12T18:31:18.000Z\"}}\n",
            "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"model\":\"claude-opus-4\",",
            "\"content\":[{{\"type\":\"text\",\"text\":\"ready\"}}]}},",
            "\"timestamp\":\"2025-11-12T18:31:20.000Z\"}}\n"
        ),
        user_text = user_text
    );
    std::fs::write(path, sample).expect("write claude session");
}

/// E2E: targeted watch-once reindex should index the changed file without persisting daemon watermarks.
#[test]
fn watch_once_reindexes_targeted_file_without_persisting_watch_state() {
    // Temp sandbox to isolate all filesystem access
    let sandbox = TempDir::new().expect("temp dir");
    let data_dir = sandbox.path().join("data");
    let home_dir = sandbox.path().join("home");
    let xdg_data = sandbox.path().join("xdg-data");
    let xdg_config = sandbox.path().join("xdg-config");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::create_dir_all(&home_dir).expect("home dir");
    std::fs::create_dir_all(&xdg_data).expect("xdg data");
    std::fs::create_dir_all(&xdg_config).expect("xdg config");

    // Seed a tiny connector fixture under Codex path so watch can detect
    let codex_root = data_dir.join(".codex/sessions");
    std::fs::create_dir_all(&codex_root).expect("codex root");
    let rollout = codex_root.join("rollout-1.jsonl");
    write_codex_session(&rollout, "watchhello", "watch-hello");

    let (output, stdout, stderr) = run_watch_once(
        &[rollout.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        output.status.success(),
        "watch run failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let watch_state_path = data_dir.join("watch_state.json");
    assert!(
        !watch_state_path.exists(),
        "explicit watch-once indexing should not persist watch_state: {}",
        watch_state_path.display()
    );

    let search_json = run_robot_search("watchhello", &data_dir, &home_dir, &xdg_data, &xdg_config);
    assert!(
        content_hit_count(&search_json, "watchhello") >= 1,
        "expected indexed hit for targeted watch-once import: {search_json}"
    );
}

/// Ensure multiple targeted paths across connectors index successfully without mutating daemon watch state.
#[test]
fn watch_once_indexes_multiple_connectors_without_persisting_watch_state() {
    let sandbox = TempDir::new().expect("temp dir");
    let data_dir = sandbox.path().join("data");
    let home_dir = sandbox.path().join("home");
    let xdg_data = sandbox.path().join("xdg-data");
    let xdg_config = sandbox.path().join("xdg-config");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(&xdg_data).unwrap();
    std::fs::create_dir_all(&xdg_config).unwrap();

    // Codex fixture
    let codex_root = data_dir.join(".codex/sessions/2025/12/02");
    std::fs::create_dir_all(&codex_root).unwrap();
    let codex_file = codex_root.join("rollout-2.jsonl");
    write_codex_session(&codex_file, "codexunique", "watch-multi-codex");

    // Claude fixture lives under HOME/.claude/projects for detection
    let claude_root = home_dir.join(".claude/projects/demo");
    std::fs::create_dir_all(&claude_root).unwrap();
    let claude_file = claude_root.join("session.jsonl");
    write_claude_session(&claude_file, "claudeunique");

    let (output, stdout, stderr) = run_watch_once(
        &[codex_file.as_path(), claude_file.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        output.status.success(),
        "watch run failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(
        !data_dir.join("watch_state.json").exists(),
        "explicit watch-once indexing should not persist watch_state"
    );

    let codex_hits = run_robot_search("codexunique", &data_dir, &home_dir, &xdg_data, &xdg_config);
    assert!(
        content_hit_count(&codex_hits, "codexunique") >= 1,
        "expected codex hit after watch-once import: {codex_hits}"
    );

    let claude_hits =
        run_robot_search("claudeunique", &data_dir, &home_dir, &xdg_data, &xdg_config);
    assert!(
        content_hit_count(&claude_hits, "claudeunique") >= 1,
        "expected claude hit after watch-once import: {claude_hits}"
    );
}

/// If files change quickly in succession, targeted watch-once imports should refresh indexed content.
#[test]
fn watch_once_reindexes_updated_content_without_persisting_watch_state() {
    let sandbox = TempDir::new().expect("temp dir");
    let data_dir = sandbox.path().join("data");
    let home_dir = sandbox.path().join("home");
    let xdg_data = sandbox.path().join("xdg-data");
    let xdg_config = sandbox.path().join("xdg-config");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(&xdg_data).unwrap();
    std::fs::create_dir_all(&xdg_config).unwrap();

    let codex_root = data_dir.join(".codex/sessions");
    std::fs::create_dir_all(&codex_root).unwrap();
    let rollout = codex_root.join("rollout-rapid.jsonl");
    write_codex_session(&rollout, "firstunique", "watch-rapid");

    let (first, stdout1, stderr1) = run_watch_once(
        &[rollout.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        first.status.success(),
        "first watch failed\nstdout:\n{stdout1}\nstderr:\n{stderr1}"
    );

    let first_hits = run_robot_search("firstunique", &data_dir, &home_dir, &xdg_data, &xdg_config);
    assert_eq!(
        content_hit_count(&first_hits, "firstunique"),
        1,
        "expected a single indexed hit for initial content: {first_hits}"
    );

    // Rewrite the same file with different same-idx content. The storage layer
    // intentionally retains the canonical first variant for duplicate idx
    // replays, so the rerun must remain idempotent rather than replacing prior
    // searchable content in place.
    write_codex_session(&rollout, "secondunique", "watch-rapid");
    std::thread::sleep(Duration::from_millis(20));
    let (second, stdout2, stderr2) = run_watch_once(
        &[rollout.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        second.status.success(),
        "second watch failed\nstdout:\n{stdout2}\nstderr:\n{stderr2}"
    );

    assert!(
        !data_dir.join("watch_state.json").exists(),
        "explicit watch-once indexing should not persist watch_state"
    );

    let canonical_hits =
        run_robot_search("firstunique", &data_dir, &home_dir, &xdg_data, &xdg_config);
    assert_eq!(
        content_hit_count(&canonical_hits, "firstunique"),
        1,
        "expected canonical first-pass content to remain stable after reimport: {canonical_hits}"
    );

    let duplicate_variant_hits =
        run_robot_search("secondunique", &data_dir, &home_dir, &xdg_data, &xdg_config);
    assert_eq!(
        content_hit_count(&duplicate_variant_hits, "secondunique"),
        0,
        "expected conflicting duplicate-idx replay content to be ignored: {duplicate_variant_hits}"
    );
}

/// Corrupt inputs should not crash targeted watch-once imports or create daemon watch state.
#[test]
fn watch_once_survives_corrupt_file_without_persisting_watch_state() {
    let sandbox = TempDir::new().expect("temp dir");
    let data_dir = sandbox.path().join("data");
    let home_dir = sandbox.path().join("home");
    let xdg_data = sandbox.path().join("xdg-data");
    let xdg_config = sandbox.path().join("xdg-config");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(&xdg_data).unwrap();
    std::fs::create_dir_all(&xdg_config).unwrap();

    let codex_root = data_dir.join(".codex/sessions");
    std::fs::create_dir_all(&codex_root).unwrap();
    let rollout = codex_root.join("rollout-corrupt.jsonl");
    std::fs::write(&rollout, r#"{"role": "user", "content": bad json"#).unwrap();

    let (output, stdout, stderr) = run_watch_once(
        &[rollout.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        output.status.success(),
        "watch with corrupt file should not crash\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !data_dir.join("watch_state.json").exists(),
        "explicit watch-once indexing should not persist watch_state"
    );
}

/// Repeated idle incremental watch passes should stay healthy and still ingest later updates.
#[test]
fn watch_once_repeated_idle_cycles_stay_healthy_and_accept_new_content() {
    let sandbox = TempDir::new().expect("temp dir");
    let data_dir = sandbox.path().join("data");
    let home_dir = sandbox.path().join("home");
    let xdg_data = sandbox.path().join("xdg-data");
    let xdg_config = sandbox.path().join("xdg-config");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(&xdg_data).unwrap();
    std::fs::create_dir_all(&xdg_config).unwrap();

    let codex_root = data_dir.join(".codex/sessions/2025/12/03");
    std::fs::create_dir_all(&codex_root).unwrap();
    let rollout = codex_root.join("rollout-idle.jsonl");
    write_codex_session(&rollout, "watch_idle_baseline", "watch-idle-baseline");

    let (full_output, full_stdout, full_stderr) =
        run_index_full(&data_dir, &home_dir, &xdg_data, &xdg_config);
    assert!(
        full_output.status.success(),
        "full index should succeed before repeated incremental watch passes\nstdout:\n{full_stdout}\nstderr:\n{full_stderr}"
    );

    let db_path = data_dir.join("agent_search.db");
    let storage = SqliteStorage::open(&db_path).expect("open indexed db");
    let namespaced: i64 = storage
        .raw()
        .query_row_map("PRAGMA fsqlite.autocommit_retain;", &[], |row| {
            row.get_typed(0)
        })
        .expect("query fsqlite autocommit_retain");
    let alias: i64 = storage
        .raw()
        .query_row_map("PRAGMA autocommit_retain;", &[], |row| row.get_typed(0))
        .expect("query autocommit_retain alias");
    assert_eq!(
        namespaced, 0,
        "writer connections should disable retained autocommit"
    );
    assert_eq!(alias, 0, "autocommit_retain alias should also be disabled");

    for cycle in 1..=8 {
        let (output, stdout, stderr) = run_watch_once_with_env(
            &[rollout.as_path()],
            &data_dir,
            &home_dir,
            &xdg_data,
            &xdg_config,
            &[("CASS_WATCH_RECYCLE_INTERVAL", "1")],
        );
        assert!(
            output.status.success(),
            "idle watch cycle {cycle} should not fail or crash-loop\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    assert!(
        !data_dir.join("watch_state.json").exists(),
        "explicit watch-once indexing should not persist watch_state during repeated idle passes"
    );

    let baseline_hits = run_robot_search(
        "watch_idle_baseline",
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        content_hit_count(&baseline_hits, "watch_idle_baseline") >= 1,
        "baseline content should remain searchable after repeated idle watch passes: {baseline_hits}"
    );

    let followup = codex_root.join("rollout-idle-followup.jsonl");
    write_codex_session(&followup, "watch_idle_followup", "watch-idle-followup");
    std::thread::sleep(Duration::from_millis(20));

    let (output, stdout, stderr) = run_watch_once_with_env(
        &[followup.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
        &[("CASS_WATCH_RECYCLE_INTERVAL", "1")],
    );
    assert!(
        output.status.success(),
        "watch should still ingest a new session after repeated idle passes\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let followup_hits = run_robot_search(
        "watch_idle_followup",
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        content_hit_count(&followup_hits, "watch_idle_followup") >= 1,
        "new content should still be indexed after repeated idle watch cycles: {followup_hits}"
    );
}
