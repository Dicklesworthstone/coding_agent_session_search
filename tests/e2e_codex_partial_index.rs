//! GH #484: an over-budget middle source must not suppress valid neighbors or
//! become a successful CLI/idempotency receipt. Uses the real archive and CLI.
use assert_cmd::Command;
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

#[test]
fn streaming_keeps_neighbors_and_reports_partial_without_caching() {
    exercise("1");
}

#[test]
fn batch_mode_keeps_neighbors_and_reports_partial_without_caching() {
    exercise("0");
}
