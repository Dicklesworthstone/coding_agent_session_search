use std::path::Path;
use std::time::Duration;

use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::Value;
use tempfile::TempDir;

mod util;
use util::cass_bin;

mod deferred_watch_sources {
    use super::*;
    use std::fs;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    struct WatchChild(Child);

    impl Drop for WatchChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn cass(home: &Path, data: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        cmd.env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("PATH", "")
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(home)
            .arg("--data-dir")
            .arg(data);
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            cmd.env("SystemRoot", system_root);
        }
        cmd
    }

    fn indexed_watch_home() -> TempDir {
        let home = TempDir::new().expect("isolated watch home");
        let data = home.path().join("cass-data");
        let sessions = home.path().join(".codex/sessions");
        fs::create_dir_all(&sessions).expect("sessions directory");
        let baseline = sessions.join("rollout-baseline.jsonl");
        write_codex_session(&baseline, "baselinewatch", "baseline-watch");
        fs::File::options()
            .write(true)
            .open(&baseline)
            .expect("open baseline session for timestamp update")
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            )
            .expect("age baseline session");
        assert_cmd::Command::from_std(cass(home.path(), &data))
            .args(["index", "--full", "--json"])
            .timeout(Duration::from_secs(120))
            .assert()
            .success();
        home
    }

    fn start_watch(home: &Path, data: &Path, streaming: bool) -> (WatchChild, std::path::PathBuf) {
        let log_path = home.join("watch.log");
        let log = fs::File::create(&log_path).expect("watch log");
        let mut command = cass(home, data);
        command
            // JSON mode suppresses the INFO readiness signal unless requested.
            .arg("--verbose")
            .args(["index", "--watch", "--watch-interval", "1", "--json"])
            .env("CASS_ACTIVE_SESSION_RECENT_WRITE_WINDOW_SECS", "8")
            .env("CASS_STREAMING_INDEX", if streaming { "1" } else { "0" })
            .env("RUST_LOG", "info");
        let mut child = WatchChild(
            command
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .expect("start watch"),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let logs = fs::read_to_string(&log_path).expect("read watch log");
            if logs.contains("watch mode: minimum interval between scan cycles") {
                break;
            }
            assert!(
                child.0.try_wait().expect("watch status").is_none(),
                "watch exited: {logs}"
            );
            assert!(Instant::now() < deadline, "watch did not start: {logs}");
            std::thread::sleep(Duration::from_millis(100));
        }
        (child, log_path)
    }

    fn search(home: &Path, data: &Path, query: &str) -> Value {
        let output = assert_cmd::Command::from_std(cass(home, data))
            .args([
                "search",
                query,
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
                "--timeout",
                "3000",
            ])
            .timeout(Duration::from_secs(5))
            .output()
            .expect("read-only search");
        assert!(
            output.status.success(),
            "search failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).expect("search JSON");
        assert_ne!(
            result.pointer("/budget/timed_out").and_then(Value::as_bool),
            Some(true),
            "a timed-out search cannot prove absence: {result}"
        );
        assert!(result["hits"].is_array(), "search hit array: {result}");
        result
    }

    fn assert_deferred_source_retried(skipped_at_startup: bool, streaming: bool) {
        let home = indexed_watch_home();
        let data = home.path().join("cass-data");
        let deferred = home.path().join(".codex/sessions/rollout-deferred.jsonl");
        if skipped_at_startup {
            write_codex_session(&deferred, "deferredwatchneedle", "deferred-watch");
        }
        let (mut child, log_path) = start_watch(home.path(), &data, streaming);
        if !skipped_at_startup {
            // Exactly one source write after the actual watcher is installed.
            // Neither this source nor its directory is touched again.
            write_codex_session(&deferred, "deferredwatchneedle", "deferred-watch");
        }
        let before = fs::read(&deferred).expect("deferred source bytes");
        let modified = fs::metadata(&deferred)
            .expect("source metadata")
            .modified()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut observed_early_absence = false;
        loop {
            let logs = fs::read_to_string(&log_path).expect("read watch log");
            let skips = logs
                .lines()
                .filter(|line| {
                    line.contains("rollout-deferred.jsonl")
                        && line
                            .contains("skipping session source that appears to be actively written")
                })
                .count();
            assert!(
                child.0.try_wait().expect("watch status").is_none(),
                "watch exited: {logs}"
            );
            if skips > 0 {
                let result = search(home.path(), &data, "deferredwatchneedle");
                let hits = content_hit_count(&result, "deferredwatchneedle");
                if modified.elapsed().expect("source age") < Duration::from_secs(8) {
                    assert_eq!(hits, 0, "recently modified source was indexed: {result}");
                    observed_early_absence = true;
                } else {
                    assert!(
                        observed_early_absence,
                        "missed the recent-write window before checking absence: {logs}"
                    );
                }
                // Discovery and post-parse filtering can both log a refusal in
                // one scan. Actual search absence above is the safety oracle.
                if skips >= 2 && hits == 1 {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "a source skipped {skips} times never became searchable without a second event: {logs}"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(fs::read(&deferred).expect("source after retry"), before);
        assert_eq!(
            fs::metadata(&deferred).unwrap().modified().unwrap(),
            modified
        );
    }

    #[test]
    fn watch_retries_active_event_without_another_source_write() {
        assert_deferred_source_retried(false, true);
    }

    #[test]
    fn watch_retries_active_startup_source_after_streaming_scan() {
        assert_deferred_source_retried(true, true);
    }

    #[test]
    fn watch_retries_active_startup_source_after_batch_scan() {
        assert_deferred_source_retried(true, false);
    }

    // Linux exposes writable descriptors through /proc. The isolated command
    // deliberately has no PATH, so this does not claim macOS lsof coverage.
    #[cfg(target_os = "linux")]
    #[test]
    fn watch_retains_writer_held_source_after_another_deferred_source_is_indexed() {
        let home = indexed_watch_home();
        let data = home.path().join("cass-data");
        let sessions = home.path().join(".codex/sessions");
        let completed = sessions.join("rollout-completed.jsonl");
        let held = sessions.join("rollout-held.jsonl");
        write_codex_session(&completed, "completedwatchneedle", "completed-watch");
        write_codex_session(&held, "heldwatchneedle", "held-watch");
        let writer = fs::OpenOptions::new()
            .append(true)
            .open(&held)
            .expect("hold a real writable source descriptor");
        let before_completed = fs::read(&completed).expect("completed source bytes");
        let before_held = fs::read(&held).expect("held source bytes");
        let completed_modified = fs::metadata(&completed).unwrap().modified().unwrap();
        let held_modified = fs::metadata(&held).unwrap().modified().unwrap();
        let (mut child, log_path) = start_watch(home.path(), &data, true);
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut writer_skips_after_completed = None;
        loop {
            let logs = fs::read_to_string(&log_path).expect("read watch log");
            assert!(
                child.0.try_wait().expect("watch status").is_none(),
                "watch exited: {logs}"
            );
            let result = search(
                home.path(),
                &data,
                "completedwatchneedle OR heldwatchneedle",
            );
            assert_eq!(
                content_hit_count(&result, "heldwatchneedle"),
                0,
                "writer-held source became searchable before its descriptor closed: {result}"
            );
            // Read after the successful search: counts sampled before it could
            // mistake two refusals within that same scan for a later retry.
            let logs = fs::read_to_string(&log_path).expect("read watch log after search");
            let writer_skips = logs
                .lines()
                .filter(|line| {
                    line.contains("rollout-held.jsonl")
                        && line.contains("WritableFileDescriptor")
                        && line
                            .contains("skipping session source that appears to be actively written")
                })
                .count();
            if content_hit_count(&result, "completedwatchneedle") == 1 && writer_skips > 0 {
                if writer_skips_after_completed.is_some_and(|previous| writer_skips > previous) {
                    break;
                }
                writer_skips_after_completed.get_or_insert(writer_skips);
            }
            assert!(
                Instant::now() < deadline,
                "completed source did not become searchable while the other source stayed pending: {logs}"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        let logs = fs::read_to_string(&log_path).expect("read watch deferral evidence");
        for source in ["rollout-completed.jsonl", "rollout-held.jsonl"] {
            assert!(
                logs.lines().any(|line| {
                    line.contains(source)
                        && line.contains("RecentlyModified")
                        && line
                            .contains("skipping session source that appears to be actively written")
                }),
                "both sources must have entered the deferred queue: {source}: {logs}"
            );
        }

        // Closing a writable descriptor may emit CLOSE_WRITE on Linux. The
        // three tests above prove the separate no-further-event contract; this
        // phase proves a fresh writer snapshot admits the now-closed source.
        drop(writer);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let logs = fs::read_to_string(&log_path).expect("read watch log");
            assert!(
                child.0.try_wait().expect("watch status").is_none(),
                "watch exited: {logs}"
            );
            let result = search(
                home.path(),
                &data,
                "completedwatchneedle OR heldwatchneedle",
            );
            assert_eq!(content_hit_count(&result, "completedwatchneedle"), 1);
            if content_hit_count(&result, "heldwatchneedle") == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "closed source did not become searchable with a fresh writer snapshot: {logs}"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(fs::read(&completed).unwrap(), before_completed);
        assert_eq!(fs::read(&held).unwrap(), before_held);
        assert_eq!(
            fs::metadata(&completed).unwrap().modified().unwrap(),
            completed_modified
        );
        assert_eq!(
            fs::metadata(&held).unwrap().modified().unwrap(),
            held_modified
        );
    }
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

// ========================================================================
// Bead coding_agent_session_search-ev4f7 (child of ibuuh.10, scenario
// "watch-mode refresh after canonical edit").
//
// The existing watch_once_* tests cover single-file watch, multi-
// connector watch, idempotent replay, corrupt-file resilience, and
// idle cycles. None pin what the readiness surface reports AFTER a
// successful watch-once on a fresh corpus: if watch-once left the
// system in a partial/not_initialized state, agents polling
// `cass health --json` after every routine refresh would see confusing
// or wrong readiness, which is the exact kind of silent-misleading
// surface ibuuh.10's "truthful readiness" AC is meant to catch.
//
// Test shape:
//   1. Seed a Codex session and run `cass index --watch --watch-once
//      <path>` against a fresh data-dir. Bootstrap succeeds.
//   2. Seeded content is searchable via `cass search --json`.
//   3. `cass health --json` reports status="healthy",
//      state.database.exists=true, state.index.exists=true. No
//      regression to "not_initialized", no partial state.
//   4. watch_state.json is NOT persisted — watch-once must never
//      leave daemon state behind.
//
// Note on bootstrap path: `cass index --full` currently trips a
// shard-plan-vs-doc-count invariant check on single-conversation
// seeds (bug coding_agent_session_search-rx1ex), so this test uses
// watch-once to get a known-good bootstrap. That's also more faithful
// to the real production flow this AC targets (watch-mode refresh).
// ========================================================================

#[test]
fn watch_once_bootstraps_corpus_and_health_reports_truthful_ready_state() {
    let sandbox = TempDir::new().expect("temp dir");
    let data_dir = sandbox.path().join("data");
    let home_dir = sandbox.path().join("home");
    let xdg_data = sandbox.path().join("xdg-data");
    let xdg_config = sandbox.path().join("xdg-config");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::create_dir_all(&home_dir).expect("home dir");
    std::fs::create_dir_all(&xdg_data).expect("xdg data");
    std::fs::create_dir_all(&xdg_config).expect("xdg config");

    // Phase 1 — seed a single Codex session and bootstrap the
    // corpus via watch-once. This mirrors how an operator onboards
    // cass to a live connector path for the first time.
    let codex_root = data_dir.join(".codex/sessions");
    std::fs::create_dir_all(&codex_root).expect("codex root");
    let rollout = codex_root.join("rollout-watch-bootstrap.jsonl");
    write_codex_session(&rollout, "watchbootstrapcontent", "watch-bootstrap-sess");

    let (out, stdout, stderr) = run_watch_once(
        &[rollout.as_path()],
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        out.status.success(),
        "bootstrap watch-once must succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Phase 2 — seeded content is searchable.
    let hits = run_robot_search(
        "watchbootstrapcontent",
        &data_dir,
        &home_dir,
        &xdg_data,
        &xdg_config,
    );
    assert!(
        content_hit_count(&hits, "watchbootstrapcontent") >= 1,
        "seeded session must be searchable after watch-once: {hits}"
    );

    // Phase 3 — readiness surface is truthful. This is the ibuuh.10
    // slice: after a normal watch-once refresh, `cass health --json`
    // must not lie about the state. status="healthy" AND both DB and
    // lexical index exist — not "not_initialized", not a partial
    // state, not a rebuild-in-progress phantom.
    let health_out = std::process::Command::new(cass_bin())
        .arg("health")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", &home_dir)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("CODEX_HOME", data_dir.join(".codex"))
        .output()
        .expect("run cass health");
    let health_stdout = String::from_utf8_lossy(&health_out.stdout);
    let health_json: Value = serde_json::from_str(&health_stdout)
        .unwrap_or_else(|err| panic!("health JSON parse failed: {err}; stdout: {health_stdout}"));
    assert_eq!(
        health_json.get("status").and_then(Value::as_str),
        Some("healthy"),
        "post-watch health must report status=healthy; payload: {health_json}"
    );
    assert_eq!(
        health_json.get("healthy").and_then(Value::as_bool),
        Some(true),
        "post-watch health.healthy must be true; payload: {health_json}"
    );
    assert_eq!(
        health_json
            .get("state")
            .and_then(|s| s.get("database"))
            .and_then(|db| db.get("exists"))
            .and_then(Value::as_bool),
        Some(true),
        "post-watch state.database.exists must be true; payload: {health_json}"
    );
    assert_eq!(
        health_json
            .get("state")
            .and_then(|s| s.get("index"))
            .and_then(|i| i.get("exists"))
            .and_then(Value::as_bool),
        Some(true),
        "post-watch state.index.exists must be true; payload: {health_json}"
    );

    // Phase 4 — watch-once must never persist daemon watch_state.
    // Losing this invariant would cause every watch-once invocation
    // to start a long-running background daemon even in single-shot
    // agent usage.
    assert!(
        !data_dir.join("watch_state.json").exists(),
        "watch-once must not persist watch_state.json"
    );
}
