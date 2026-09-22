//! GH #494: an EOF cursor proves ingestion, not publication. Resume must
//! finish the candidate, certify it, and leave subsequent searches read-only.

use assert_cmd::Command;
use coding_agent_search::search::tantivy::expected_index_dir;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod util;

const PROBE: &str = "publishresumeneedle";
const CHECKPOINT: &str = ".lexical-rebuild-state.json";
const GENERATION: &str = "lexical-generation-manifest.json";
const OLD_GENERATION_ID: &str = "gh494-prior-published-generation";
const SESSIONS: usize = 3;

fn cass(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.current_dir(home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env_remove("CASS_AUTO_REFRESH")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", home.join(".codex"))
        .timeout(Duration::from_secs(240));
    cmd
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read fixture JSON"))
        .expect("parse fixture JSON")
}

fn write_json(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn full_index(home: &Path, data_dir: &Path) -> std::process::Output {
    cass(home)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(data_dir)
        .output()
        .expect("run full index")
}

fn assert_index_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("index JSON");
    assert_eq!(json["success"], true, "{json}");
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("cass_data");
    std::fs::create_dir_all(&data_dir).unwrap();
    for n in 0..SESSIONS {
        util::seed_codex_session(
            &tmp.path().join(".codex"),
            &format!("rollout-publish-resume-{n:02}.jsonl"),
            &format!("{PROBE} session{n}"),
            true,
        );
    }
    assert_index_success(&full_index(tmp.path(), &data_dir));
    let index = expected_index_dir(&data_dir);
    (tmp, data_dir, index)
}

fn copy_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            assert!(kind.is_file(), "unexpected non-file in isolated test index");
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn stage_published_index(index: &Path) -> PathBuf {
    let name = index.file_name().unwrap().to_str().unwrap();
    let staged = index.with_file_name(format!(".{name}.rebuild-staging"));
    copy_directory(index, &staged);
    std::fs::write(staged.join("gh494-candidate-marker"), "candidate").unwrap();
    staged
}

fn plant_eof_checkpoint(index: &Path) {
    let mut checkpoint = read_json(&index.join(CHECKPOINT));
    assert_eq!(checkpoint["completed"], true);
    assert_eq!(
        checkpoint["processed_conversations"],
        checkpoint["db"]["total_conversations"]
    );
    assert!(checkpoint["committed_conversation_id"].as_i64().is_some());
    checkpoint["completed"] = Value::Bool(false);
    checkpoint["pending"] = Value::Null;
    write_json(&index.join(CHECKPOINT), &checkpoint);
    let mut generation = read_json(&index.join(GENERATION));
    generation["generation_id"] = serde_json::json!(OLD_GENERATION_ID);
    write_json(&index.join(GENERATION), &generation);
}

fn assert_certified_and_searches_are_read_only(home: &Path, data_dir: &Path, index: &Path) {
    let checkpoint = read_json(&index.join(CHECKPOINT));
    assert_eq!(checkpoint["completed"], true, "{checkpoint}");
    assert_eq!(checkpoint["pending"], Value::Null, "{checkpoint}");
    assert_eq!(checkpoint["indexed_docs"], SESSIONS * 2, "{checkpoint}");
    assert!(
        checkpoint["db"]["storage_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("content-v1:")
    );
    let generation = read_json(&index.join(GENERATION));
    assert_ne!(generation["generation_id"], OLD_GENERATION_ID);
    assert_eq!(generation["publish_state"], "published");
    assert_eq!(generation["indexed_doc_count"], SESSIONS * 2);
    let manifest_before = std::fs::read(index.join("MANIFEST")).unwrap();
    let checkpoint_before = std::fs::read(index.join(CHECKPOINT)).unwrap();
    let generation_before = std::fs::read(index.join(GENERATION)).unwrap();
    for _ in 0..2 {
        let output = cass(home)
            .args([
                "search",
                PROBE,
                "--json",
                "--mode",
                "lexical",
                "--limit",
                "100",
                "--data-dir",
            ])
            .arg(data_dir)
            .output()
            .expect("search completed generation");
        assert!(
            output.status.success(),
            "search status={}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let payload: Value = serde_json::from_slice(&output.stdout).expect("search JSON");
        assert_eq!(
            payload["hits"].as_array().unwrap().len(),
            SESSIONS * 2,
            "{payload}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr)
                .contains("rebuilding from canonical database before running query"),
            "search must not enter inline rebuild"
        );
        assert_eq!(
            std::fs::read(index.join("MANIFEST")).unwrap(),
            manifest_before
        );
        assert_eq!(
            std::fs::read(index.join(CHECKPOINT)).unwrap(),
            checkpoint_before
        );
        assert_eq!(
            std::fs::read(index.join(GENERATION)).unwrap(),
            generation_before
        );
    }
}

#[test]
fn gh494_staged_eof_checkpoint_finishes_publish_instead_of_returning_early() {
    let (tmp, data_dir, index) = fixture();
    let staged = stage_published_index(&index);
    plant_eof_checkpoint(&index);
    assert_index_success(&full_index(tmp.path(), &data_dir));
    assert!(
        !staged.exists(),
        "candidate must have been consumed by publication"
    );
    assert!(index.join("gh494-candidate-marker").is_file());
    assert_certified_and_searches_are_read_only(tmp.path(), &data_dir, &index);
}

#[test]
fn gh494_live_eof_checkpoint_finishes_certification_instead_of_returning_early() {
    let (tmp, data_dir, index) = fixture();
    plant_eof_checkpoint(&index);
    assert_index_success(&full_index(tmp.path(), &data_dir));
    assert_certified_and_searches_are_read_only(tmp.path(), &data_dir, &index);
}

#[test]
fn gh494_search_budget_refuses_inline_work_but_explicit_full_index_recovers() {
    let (tmp, data_dir, index) = fixture();
    plant_eof_checkpoint(&index);
    // No independent proof exists in this refusal fixture. A completed prior
    // generation with a valid receipt is covered by the availability control.
    std::fs::rename(
        index.join(".lexical-published-state.json"),
        tmp.path().join("saved-publication-receipt.json"),
    )
    .unwrap();
    let checkpoint_before = std::fs::read(index.join(CHECKPOINT)).unwrap();
    let generation_before = std::fs::read(index.join(GENERATION)).unwrap();
    let manifest_before = std::fs::read(index.join("MANIFEST")).unwrap();
    for robot in [false, true] {
        let mut command = cass(tmp.path());
        command
            .env(
                "CASS_INCREMENTAL_AUTHORITATIVE_LEXICAL_REPAIR_MAX_DB_BYTES",
                "0",
            )
            .timeout(Duration::from_secs(30))
            .args(["search", PROBE, "--mode", "lexical", "--data-dir"])
            .arg(&data_dir);
        if robot {
            command.arg("--json");
        }
        let output = command.output().expect("bounded search refusal");
        assert!(!output.status.success(), "a refused repair cannot succeed");
        let rendered = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for expected in ["was not started", "archive_bytes=", "maximum_bytes=0"] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
        assert!(
            !rendered.contains("rebuilding from canonical database before running query"),
            "refusal must precede inline rebuild: {rendered}"
        );
        assert_eq!(
            std::fs::read(index.join(CHECKPOINT)).unwrap(),
            checkpoint_before
        );
        assert_eq!(
            std::fs::read(index.join(GENERATION)).unwrap(),
            generation_before
        );
        assert_eq!(
            std::fs::read(index.join("MANIFEST")).unwrap(),
            manifest_before
        );
    }
    let output = cass(tmp.path())
        .env(
            "CASS_INCREMENTAL_AUTHORITATIVE_LEXICAL_REPAIR_MAX_DB_BYTES",
            "0",
        )
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("explicit full indexing remains available");
    assert_index_success(&output);
    assert_certified_and_searches_are_read_only(tmp.path(), &data_dir, &index);
}

#[test]
fn gh494_readable_generation_remains_searchable_with_a_zero_repair_budget() {
    let (tmp, data_dir, index) = fixture();
    let checkpoint_before = std::fs::read(index.join(CHECKPOINT)).unwrap();
    let generation_before = std::fs::read(index.join(GENERATION)).unwrap();
    for _ in 0..2 {
        let output = cass(tmp.path())
            .env(
                "CASS_INCREMENTAL_AUTHORITATIVE_LEXICAL_REPAIR_MAX_DB_BYTES",
                "0",
            )
            .args([
                "search",
                PROBE,
                "--json",
                "--mode",
                "lexical",
                "--limit",
                "100",
                "--data-dir",
            ])
            .arg(&data_dir)
            .output()
            .expect("query a readable generation without maintenance");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(payload["hits"].as_array().unwrap().len(), SESSIONS * 2);
        assert_eq!(
            std::fs::read(index.join(CHECKPOINT)).unwrap(),
            checkpoint_before
        );
        assert_eq!(
            std::fs::read(index.join(GENERATION)).unwrap(),
            generation_before
        );
    }
}

#[test]
fn gh494_interrupted_rebuild_keeps_certified_prior_searches_available() {
    let (tmp, data_dir, index) = fixture();
    plant_eof_checkpoint(&index);
    let paths = [
        CHECKPOINT,
        GENERATION,
        "MANIFEST",
        ".lexical-published-state.json",
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(index.join(path)).unwrap())
        .collect();
    for no_maintenance in [false, true] {
        for robot in [false, true] {
            let mut command = cass(tmp.path());
            command
                .env(
                    "CASS_INCREMENTAL_AUTHORITATIVE_LEXICAL_REPAIR_MAX_DB_BYTES",
                    "0",
                )
                .timeout(Duration::from_secs(30))
                .args([
                    "search",
                    PROBE,
                    "--mode",
                    "lexical",
                    "--limit",
                    "100",
                    "--data-dir",
                ])
                .arg(&data_dir);
            if no_maintenance {
                command.arg("--no-maintenance");
            }
            if robot {
                command.arg("--json");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if robot {
                let json: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(json["hits"].as_array().unwrap().len(), SESSIONS * 2);
            } else {
                assert!(String::from_utf8_lossy(&output.stdout).contains(PROBE));
            }
            assert!(
                !String::from_utf8_lossy(&output.stderr)
                    .contains("rebuilding from canonical database before running query")
            );
            for (path, bytes) in paths.iter().zip(&before) {
                assert_eq!(std::fs::read(index.join(path)).unwrap(), *bytes, "{path}");
            }
            assert_eq!(read_json(&index.join(CHECKPOINT))["completed"], false);
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn gh494_killed_after_atomic_swap_remains_searchable_without_inline_rebuild() {
    use std::fs::File;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    // A failed assertion must not strand the deliberately parked indexer.
    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let (tmp, data_dir, index) = fixture();
    let staged = stage_published_index(&index);
    plant_eof_checkpoint(&index);
    let sentinel = tmp.path().join("swap-committed.json");
    let log_path = tmp.path().join("killed-indexer.log");
    let stdout = File::create(&log_path).unwrap();
    let stderr = stdout.try_clone().unwrap();
    // Use the existing production crash-window seam, not a fabricated
    // checkpoint: the child parks after the real exchange and sidecar park.
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .current_dir(tmp.path())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("HOME", tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .env("CODEX_HOME", tmp.path().join(".codex"))
        .env(
            "CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL",
            &sentinel,
        )
        .env("CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SLEEP_MS", "120000")
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(120);
    while !sentinel.is_file() {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!(
                "indexer exited before the atomic-swap crash window: {status}\n{}",
                std::fs::read_to_string(&log_path).unwrap()
            );
        }
        assert!(
            Instant::now() < deadline,
            "indexer did not reach the atomic-swap crash window\n{}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let receipt = read_json(&sentinel);
    assert_eq!(receipt["stage"], "linux_swap_committed_prior_live_parked");
    assert_eq!(receipt["pid"], child.0.id());
    assert!(!staged.exists());
    assert!(index.join("gh494-candidate-marker").is_file());

    // Kill before publish_staged_lexical_index returns to its caller. No
    // post-publish checkpoint write or destructor can rescue the fixture.
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert_certified_and_searches_are_read_only(tmp.path(), &data_dir, &index);
}
