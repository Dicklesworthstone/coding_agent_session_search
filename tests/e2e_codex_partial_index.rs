//! GH #484 / #533: a failed middle source must not suppress valid neighbors or
//! become a successful CLI/idempotency receipt. Uses the real archive and CLI.
use assert_cmd::Command;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const LIMIT: u64 = 100 * 1024 * 1024;

fn cass(home: &Path, streaming: &str) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_STREAMING_INDEX", streaming)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .timeout(Duration::from_secs(240));
    command
}

fn index(home: &Path, streaming: &str) -> std::process::Output {
    cass(home, streaming)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(home.join("archive"))
        .args(["--idempotency-key", "partial-must-not-cache"])
        .output()
        .expect("run index command")
}

fn assert_searchable(home: &Path, streaming: &str, marker: &str, source: &Path) {
    let output = cass(home, streaming)
        .args([
            "search",
            marker,
            "--json",
            "--mode",
            "lexical",
            "--no-maintenance",
            "--no-daemon",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(home.join("archive"))
        .output()
        .expect("run search command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("search JSON");
    let hits = result["hits"].as_array().expect("search hits");
    assert_eq!(hits.len(), 1, "{result}");
    assert_eq!(hits[0]["source_path"], source.to_string_lossy().as_ref());
}

fn exercise(streaming: &str) {
    let dir = tempfile::tempdir().expect("isolated archive");
    let home = dir.path();
    let sessions = home.join(".codex/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let markers = [
        "partialfirstsentinel",
        "partialrejectedsentinel",
        "partiallastsentinel",
    ];
    let paths = ["a", "b", "c"].map(|name| sessions.join(format!("rollout-{name}.jsonl")));
    let originals: Vec<_> = paths.iter().zip(markers).enumerate().map(|(i,(path,marker))| {
        let rows = [
            json!({"type":"session_meta","payload":{"id":format!("partial-{i}"),"cwd":home.join("project")}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":marker}]}}),
        ];
        let bytes = format!("{}\n{}\n", rows[0], rows[1]).into_bytes();
        std::fs::write(path, &bytes).unwrap();
        bytes
    }).collect();
    // Valid JSONL throughout, not a corruption failure dressed as oversize.
    let mut middle = std::fs::OpenOptions::new()
        .append(true)
        .open(&paths[1])
        .unwrap();
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(65536)
    );
    while middle.metadata().unwrap().len() <= LIMIT {
        middle.write_all(padding.as_bytes()).unwrap();
    }
    drop(middle);
    let oversized_hash = blake3::hash(&std::fs::read(&paths[1]).unwrap());
    for _ in 0..2 {
        let output = index(home, streaming);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(output.status.code(), Some(9), "{result}");
        assert_eq!(result["success"], false, "{result}");
        assert_eq!(result["partial"], true, "{result}");
        assert_eq!(result["coverage_status"], "incomplete");
        assert_eq!(result["indexing_stats"]["scan_had_errors"], true);
        assert_ne!(result["cached"], true);
        assert!(
            result["failed_connectors"]
                .as_array()
                .unwrap()
                .contains(&json!("codex"))
        );
        let stats = result["indexing_stats"]["connectors"].as_array().unwrap();
        let codex = stats.iter().find(|c| c["name"] == "codex").unwrap();
        let detail = codex["error"].as_str().expect("explicit rejected source");
        assert!(
            detail.contains("enrichment_read_budget_exceeded"),
            "{detail}"
        );
        assert!(detail.contains("rollout-b.jsonl"), "{detail}");
        assert_searchable(home, streaming, markers[0], &paths[0]);
        assert_searchable(home, streaming, markers[2], &paths[2]);
    }
    assert_eq!(
        blake3::hash(&std::fs::read(&paths[1]).unwrap()),
        oversized_hash
    );
    assert_eq!(std::fs::read(&paths[0]).unwrap(), originals[0]);
    assert_eq!(std::fs::read(&paths[2]).unwrap(), originals[2]);
    // Repair only the synthetic test fixture. Reusing the identical request key
    // must execute again rather than replaying the earlier partial invocation.
    std::fs::write(&paths[1], &originals[1]).unwrap();
    let output = index(home, streaming);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let clean: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(clean["success"], true, "{clean}");
    assert_eq!(clean["cached"], false, "{clean}");
    assert_eq!(clean["indexing_stats"]["scan_had_errors"], false);
    assert_searchable(home, streaming, markers[1], &paths[1]);
    let output = index(home, streaming);
    assert!(output.status.success());
    let cached: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cached["cached"], true, "{cached}");
}

/// Per-source skip decisions from the production trace sink.
fn source_skips(trace: &Path) -> Vec<bool> {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let event: Value = serde_json::from_str(line).expect("trace JSONL");
            (event["fields"]["message"] == "source_ingest_observation")
                .then(|| event["fields"]["skipped"].as_bool().expect("skip flag"))
        })
        .collect()
}

/// GH #489: a rejected rollout keeps every incremental run partial (exit 9),
/// but it must not make each run re-parse the healthy rollouts beside it.
fn incremental_runs_stay_partial_without_reparsing(streaming: &str) {
    let dir = tempfile::tempdir().expect("isolated archive");
    let home = dir.path();
    let sessions = home.join(".codex/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let paths = ["a", "b", "c"].map(|name| sessions.join(format!("rollout-{name}.jsonl")));
    for (i, path) in paths.iter().enumerate() {
        let rows = [
            json!({"type":"session_meta","payload":{"id":format!("incremental-{i}"),"cwd":home.join("project")}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("incrementalsentinel{i}z")}]}}),
        ];
        std::fs::write(path, format!("{}\n{}\n", rows[0], rows[1])).unwrap();
    }
    let mut middle = std::fs::OpenOptions::new()
        .append(true)
        .open(&paths[1])
        .unwrap();
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(65536)
    );
    while middle.metadata().unwrap().len() <= LIMIT {
        middle.write_all(padding.as_bytes()).unwrap();
    }
    drop(middle);
    for run in 0..3 {
        let trace = home.join(format!("trace-{run}.jsonl"));
        let output = cass(home, streaming)
            .env(
                "CASS_TRACE_FILTER",
                "warn,coding_agent_search::indexer=debug",
            )
            .arg("--trace-file")
            .arg(&trace)
            .args(["index", "--json", "--data-dir"])
            .arg(home.join("archive"))
            .output()
            .expect("run index command");
        let result: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("stderr={}", String::from_utf8_lossy(&output.stderr)));
        assert_eq!(output.status.code(), Some(9), "run {run}: {result}");
        let codex = result["indexing_stats"]["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "codex")
            .unwrap()
            .clone();
        let detail = codex["error"].as_str().expect("explicit rejected source");
        assert!(detail.contains("rollout-b.jsonl"), "run {run}: {detail}");
        // The first run parses all three; later runs re-examine only the
        // rejected rollout (a size check, no parse) and both neighbors reuse
        // their committed ledger entries.
        let mut skips = source_skips(&trace);
        skips.sort_unstable();
        let expected = if run == 0 {
            [false, false, false]
        } else {
            [false, true, true]
        };
        assert_eq!(skips, expected, "run {run}");
        assert_searchable(home, streaming, "incrementalsentinel0z", &paths[0]);
        assert_searchable(home, streaming, "incrementalsentinel2z", &paths[2]);
    }
}

#[test]
fn streaming_incremental_runs_stay_partial_without_reparsing_neighbors() {
    incremental_runs_stay_partial_without_reparsing("1");
}

#[test]
fn batch_incremental_runs_stay_partial_without_reparsing_neighbors() {
    incremental_runs_stay_partial_without_reparsing("0");
}

#[test]
fn streaming_keeps_neighbors_and_reports_partial_without_caching() {
    exercise("1");
}

#[test]
fn batch_mode_keeps_neighbors_and_reports_partial_without_caching() {
    exercise("0");
}

/// The GH #533 synthetic reproduction, plus damaged-input and repair controls.
/// Ordinary incremental indexing must certify metadata-only compressed history
/// without inserting a conversation, and must keep failures visible on retry.
fn compressed_empty_rollout_cli(streaming: &str) {
    let dir = tempfile::tempdir().expect("isolated archive");
    let home = dir.path();
    let sessions = home.join(".codex/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let paths = [
        sessions.join("rollout-a.jsonl"),
        sessions.join("rollout-b-empty.jsonl.zst"),
        sessions.join("rollout-c.jsonl"),
    ];
    let originals = ["compressedfirstsentinel", "compressedlastsentinel"].map(|text| {
        format!(
            "{}\n",
            json!({"type":"response_item","payload":{"role":"user","content":text}})
        )
    });
    std::fs::write(&paths[0], &originals[0]).unwrap();
    std::fs::write(&paths[2], &originals[1]).unwrap();
    let metadata = b"{\"type\":\"session_meta\",\"payload\":{}}\n";
    let encoded = zstd::stream::encode_all(metadata.as_slice(), 0).unwrap();
    let cases = [
        (encoded.clone(), None),
        (
            encoded[..encoded.len() - 1].to_vec(),
            Some("unfinished_source"),
        ),
        (
            zstd::stream::encode_all(b"\xff\n".as_slice(), 0).unwrap(),
            Some("invalid_source_data"),
        ),
        (
            zstd::stream::encode_all(b"{\"private_marker\":".as_slice(), 0).unwrap(),
            Some("unfinished_source"),
        ),
        (
            zstd::stream::encode_all(" \n".repeat(2049).as_bytes(), 0).unwrap(),
            Some("read_budget_exceeded"),
        ),
        (encoded, None),
    ];
    for (bytes, failure) in cases {
        std::fs::write(&paths[1], &bytes).unwrap();
        // Every case runs twice: a failed source must never be certified by
        // the durable source ledger, while successful neighbors remain intact.
        for _ in 0..2 {
            let mut command = cass(home, streaming);
            command.env_clear();
            for key in ["PATH", "SystemRoot", "WINDIR"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            let output = command
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("CODEX_HOME", home.join(".codex"))
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("XDG_CACHE_HOME", home.join(".cache"))
                .env("APPDATA", home.join("AppData/Roaming"))
                .env("LOCALAPPDATA", home.join("AppData/Local"))
                .env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env("CASS_STREAMING_INDEX", streaming)
                .env("CASS_RAW_MIRROR", "0")
                .env("CASS_AUTO_REFRESH", "0")
                .env("CASS_CODEX_MAX_SOURCE_BYTES", "4096")
                .env("RUST_MIN_STACK", "134217728")
                .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
                .env("NO_COLOR", "1")
                .args(["index", "--json", "--data-dir"])
                .arg(home.join("archive"))
                .output()
                .expect("run ordinary index");
            let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
                panic!(
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            assert_eq!(
                output.status.code(),
                Some(if failure.is_some() { 9 } else { 0 }),
                "{result}"
            );
            assert_eq!(result["success"], failure.is_none(), "{result}");
            assert_eq!(
                result["indexing_stats"]["scan_had_errors"],
                failure.is_some(),
                "{result}"
            );
            if let Some(reason) = failure {
                assert_eq!(result["coverage_status"], "incomplete", "{result}");
                let codex = result["indexing_stats"]["connectors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|connector| connector["name"] == "codex")
                    .unwrap();
                let detail = codex["error"].as_str().expect("actionable source failure");
                assert!(detail.contains(reason), "{detail}");
                assert!(detail.contains("rollout-b-empty.jsonl.zst"), "{detail}");
                assert!(!detail.contains("private_marker"), "{detail}");
            }
            let storage =
                SqliteStorage::open_readonly(&home.join("archive/agent_search.db")).unwrap();
            let conversations = storage.list_conversations(10, 0).unwrap();
            let mut sources: Vec<_> = conversations
                .iter()
                .map(|row| row.source_path.clone())
                .collect();
            sources.sort();
            assert_eq!(sources, [paths[0].clone(), paths[2].clone()]);
            for conversation in conversations {
                assert_eq!(
                    storage
                        .fetch_messages(conversation.id.unwrap())
                        .unwrap()
                        .len(),
                    1
                );
            }
            assert_eq!(std::fs::read(&paths[1]).unwrap(), bytes);
            assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), originals[0]);
            assert_eq!(std::fs::read_to_string(&paths[2]).unwrap(), originals[1]);
        }
    }
    assert_searchable(home, streaming, "compressedfirstsentinel", &paths[0]);
    assert_searchable(home, streaming, "compressedlastsentinel", &paths[2]);
}

#[test]
fn streaming_compressed_empty_rollout_preserves_coverage_and_neighbors() {
    compressed_empty_rollout_cli("1");
}

#[test]
fn batch_compressed_empty_rollout_preserves_coverage_and_neighbors() {
    compressed_empty_rollout_cli("0");
}
