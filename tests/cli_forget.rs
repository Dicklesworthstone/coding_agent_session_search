//! `cass forget --apply` must stop every search surface from returning the
//! forgotten conversations (bead coding_agent_session_search-2l1b0.50).
//!
//! Before the fix, forget deleted the canonical rows and rebuilt FTS and
//! analytics, but left the Quill lexical generation untouched. Quill documents
//! store message content, so a plain search kept returning the forgotten text
//! until some later `cass index` rebuilt the index.

use assert_cmd::Command;
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod util;
use util::cass_bin;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FORGOTTEN_MARKER: &str = "forgetmarkeralpha";
const KEPT_MARKER: &str = "keepmarkerbeta";

struct Archive {
    home: TempDir,
    data_dir: PathBuf,
}

impl Archive {
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(cass_bin());
        cmd.env("HOME", self.home.path())
            .env("CODEX_HOME", self.home.path().join(".codex"))
            .env("CASS_DATA_DIR", &self.data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
        cmd
    }

    fn search_hit_paths(&self, marker: &str) -> TestResult<Vec<String>> {
        let output = self
            .cmd()
            .args([
                "search", marker, "--mode", "lexical", "--robot", "--limit", "10",
            ])
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "[cli_forget] search {marker}: exit={:?} stderr={}",
            output.status.code(),
            stderr.trim()
        );
        if !output.status.success() {
            return Err(format!("search {marker} failed: {stderr}").into());
        }
        let payload: Value = serde_json::from_str(stdout.trim())?;
        let hits = payload["hits"]
            .as_array()
            .ok_or("search payload has no hits array")?;
        Ok(hits
            .iter()
            .filter_map(|hit| hit["source_path"].as_str().map(str::to_string))
            .collect())
    }
}

/// A real-format Codex rollout (the connector only reads `rollout-*.jsonl`).
fn write_codex_rollout(codex_home: &Path, name: &str, marker: &str) -> TestResult<PathBuf> {
    let dir = codex_home.join("sessions/2026/09/20");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rollout-2026-09-20T10-00-00-{name}.jsonl"));
    let lines = [
        format!(
            r#"{{"timestamp":"2026-09-20T10:00:00.000Z","type":"session_meta","payload":{{"id":"{name}","cwd":"/work/forget-test","cli_version":"0.42.0"}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-20T10:00:01.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"please remember {marker} for later"}}]}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-20T10:00:02.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"text","text":"noted {marker} in the notes"}}]}}}}"#
        ),
    ];
    fs::write(&path, lines.join("\n") + "\n")?;
    Ok(path)
}

fn indexed_archive() -> TestResult<(Archive, PathBuf)> {
    let home = TempDir::new()?;
    let data_dir = home.path().join("cass-data");
    fs::create_dir_all(&data_dir)?;
    let codex_home = home.path().join(".codex");
    let forgotten = write_codex_rollout(&codex_home, "forget-a", FORGOTTEN_MARKER)?;
    write_codex_rollout(&codex_home, "keep-b", KEPT_MARKER)?;
    let archive = Archive { home, data_dir };
    let index = archive
        .cmd()
        .args(["index", "--full", "--json", "--no-progress-events"])
        .output()?;
    eprintln!("[cli_forget] index --full: exit={:?}", index.status.code());
    if !index.status.success() {
        return Err(format!("index failed: {}", String::from_utf8_lossy(&index.stderr)).into());
    }
    Ok((archive, forgotten))
}

#[test]
fn forget_apply_removes_forgotten_text_from_lexical_search() -> TestResult {
    let (archive, forgotten) = indexed_archive()?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;

    // Negative control: the marker is searchable before forget, so an empty
    // result afterwards is caused by forget and not by a broken fixture.
    let before = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !before.iter().any(|path| path == forgotten_str) {
        return Err(format!("fixture not indexed; hits before forget: {before:?}").into());
    }

    // A dry run changes nothing.
    let dry_run = archive
        .cmd()
        .args(["forget", "--source-glob", forgotten_str, "--json"])
        .output()?;
    if !dry_run.status.success() {
        return Err(format!(
            "forget dry-run failed: {}",
            String::from_utf8_lossy(&dry_run.stderr)
        )
        .into());
    }
    if archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty() {
        return Err("a forget dry run must not change search results".into());
    }

    let apply = archive
        .cmd()
        .args([
            "forget",
            "--source-glob",
            forgotten_str,
            "--apply",
            "--json",
        ])
        .output()?;
    let apply_stderr = String::from_utf8_lossy(&apply.stderr);
    eprintln!(
        "[cli_forget] forget --apply: exit={:?} stderr={}",
        apply.status.code(),
        apply_stderr.trim()
    );
    if !apply.status.success() {
        return Err(format!("forget --apply failed: {apply_stderr}").into());
    }
    let report: Value = serde_json::from_slice(&apply.stdout)?;
    if report["conversations_deleted"] != 1 {
        return Err(format!("expected one deleted conversation: {report}").into());
    }

    let after = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !after.is_empty() {
        return Err(format!("forgotten text is still searchable: {after:?}").into());
    }
    let kept = archive.search_hit_paths(KEPT_MARKER)?;
    if kept.is_empty() {
        return Err("forget removed an unrelated conversation from search".into());
    }
    Ok(())
}
