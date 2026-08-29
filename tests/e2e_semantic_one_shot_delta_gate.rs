//! Real-binary gate for bead `coding_agent_session_search-tpndx` (GH #394
//! remainder): a one-shot `cass index --semantic` whose embedding watermark
//! merely TRAILS the corpus must embed only the delta (WAL-append onto the
//! existing `.fsvi`) instead of re-embedding the whole corpus.
//!
//! 7f657026 already short-circuits the fully-covered case; this gate pins the
//! trailing case: seed one session, build the semantic artifact, add a second
//! session, run a plain one-shot `index --semantic`, and prove (a) the delta
//! path was taken (its tracing marker, human mode honours `RUST_LOG`), (b) the
//! new session is semantically searchable afterwards, and (c) the artifact was
//! appended to, not replaced (same file, larger).

mod util;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const INDEX_TIMEOUT: Duration = Duration::from_secs(180);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
const FIRST_KEYWORD: &str = "delta-gate-first-session-zxqv";
const SECOND_KEYWORD: &str = "delta-gate-second-session-plmk";
const DELTA_MARKER: &str = "one-shot semantic delta embed";

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn cass(fixture: &Fixture, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .arg("--data-dir")
        .arg(&fixture.data_dir)
        .current_dir(&fixture.home)
        .env("HOME", &fixture.home)
        .env("XDG_DATA_HOME", fixture.home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", fixture.home.join("xdg-config"))
        .env("XDG_CACHE_HOME", fixture.home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODEX_HOME", &fixture.codex_home)
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("RUST_LOG");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

fn run(fixture: &Fixture, label: &str, args: &[&str], env: &[(&str, &str)], t: Duration) -> Output {
    spawn_with_timeout_or_diag(cass(fixture, args, env), label, Some(&fixture.data_dir), t)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn vector_index_file(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("vector_index");
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "fsvi"))
        .ok_or_else(|| format!("no .fsvi under {}", dir.display()))
}

fn semantic_hits(fixture: &Fixture, label: &str, keyword: &str) -> Result<u64, String> {
    let out = run(
        fixture,
        label,
        &["search", keyword, "--json", "--mode", "semantic"],
        &[],
        SEARCH_TIMEOUT,
    );
    let value: Value = serde_json::from_str(text(&out.stdout).trim()).map_err(|e| {
        format!(
            "{label}: search stdout not JSON: {e}; stderr: {}",
            text(&out.stderr)
        )
    })?;
    value
        .get("total_matches")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{label}: total_matches missing: {value}"))
}

fn check() -> Result<(), String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-delta-one.jsonl",
        FIRST_KEYWORD,
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home,
    };

    // 1. Build lexical + semantic artifacts for the first session.
    let full = run(
        &fixture,
        "delta_gate_full_semantic",
        &[
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--json",
            "--no-progress-events",
        ],
        &[],
        INDEX_TIMEOUT,
    );
    let full_json: Value = serde_json::from_str(text(&full.stdout).trim()).map_err(|e| {
        format!(
            "full index stdout not JSON: {e}; stderr: {}",
            text(&full.stderr)
        )
    })?;
    if full_json.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!("full semantic index did not succeed: {full_json}"));
    }
    let fsvi = vector_index_file(&fixture.data_dir)?;
    let before_len = std::fs::metadata(&fsvi)
        .map_err(|e| format!("stat fsvi: {e}"))?
        .len();
    if semantic_hits(&fixture, "semantic_first_before", FIRST_KEYWORD)? < 1 {
        return Err("first session not semantically searchable after full build".to_string());
    }

    // 2. A second session arrives: the watermark now trails the corpus.
    util::seed_codex_session(
        &fixture.codex_home,
        "rollout-2026-04-24T10-00-00-delta-two.jsonl",
        SECOND_KEYWORD,
        true,
    );

    // 3. Plain one-shot `index --semantic` (human mode so RUST_LOG=info reaches
    //    stderr) must take the delta path.
    let delta = run(
        &fixture,
        "delta_gate_one_shot",
        &[
            "index",
            "--semantic",
            "--embedder",
            "hash",
            "--no-progress-events",
        ],
        &[("RUST_LOG", "info")],
        INDEX_TIMEOUT,
    );
    let delta_err = text(&delta.stderr);
    if !delta.status.success() {
        return Err(format!(
            "one-shot semantic index failed; stderr: {delta_err}"
        ));
    }
    if !delta_err.contains(DELTA_MARKER) {
        return Err(format!(
            "one-shot index --semantic did not take the delta path (marker '{DELTA_MARKER}' absent); stderr: {delta_err}"
        ));
    }
    if delta_err.contains("falling back to the bulk semantic pass") {
        return Err(format!(
            "delta embed fell back to bulk; stderr: {delta_err}"
        ));
    }

    // 4. Artifact appended (same file, not smaller) and both sessions searchable.
    let fsvi_after = vector_index_file(&fixture.data_dir)?;
    if fsvi_after.cmp(&fsvi).is_ne() {
        return Err(format!(
            "vector index was replaced ({} -> {}), expected an in-place append",
            fsvi.display(),
            fsvi_after.display()
        ));
    }
    let after_len = std::fs::metadata(&fsvi_after)
        .map_err(|e| format!("stat fsvi: {e}"))?
        .len();
    if after_len < before_len {
        return Err(format!(
            "vector index shrank after delta embed: {before_len} -> {after_len}"
        ));
    }
    if semantic_hits(&fixture, "semantic_second_after", SECOND_KEYWORD)? < 1 {
        return Err("second session not semantically searchable after delta embed".to_string());
    }
    if semantic_hits(&fixture, "semantic_first_after", FIRST_KEYWORD)? < 1 {
        return Err("first session lost from semantic search after delta embed".to_string());
    }
    Ok(())
}

#[test]
fn one_shot_semantic_index_embeds_only_the_delta_when_watermark_trails() -> Result<(), String> {
    check()
}
