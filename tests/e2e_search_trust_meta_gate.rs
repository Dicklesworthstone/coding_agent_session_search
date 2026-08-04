//! Real-binary E2E gate for the per-hit trust/provenance verdict on `cass
//! search` (bead `coding_agent_session_search-guided-ops-repro-trust-5u82n.3`,
//! "Trust-score search results and answer packs with provenance signals").
//!
//! Why this gate exists
//! --------------------
//! `src/search/trust_scoring.rs` is the pure, unit-tested verdict core
//! (metadata-only signals → `TrustAssessment`). This gate proves the live
//! projection: that the real `cass` binary attaches a per-hit `trust` block to
//! `search --json --robot-meta` output, that the verdict reflects the live
//! recency signal (an aged session → `stale`/`aged_out`; a fresh, unlinked
//! session → `unverified`), and — critically — that the layer is *advisory
//! metadata* that never leaks into the default fast paths:
//!   * no `trust` key without `--robot-meta` (the fast paths stay byte-identical),
//!   * no `trust` key under the minimal projection (source_path/line_number/agent),
//!   * the hit ordering is identical with and without the verdict.
//!
//! Fixtures
//! --------
//! Two Codex sessions are seeded under one isolated `CODEX_HOME` and indexed
//! once: an *old* session (the shared seed helper's fixed 2024 timestamp →
//! older than the stale window) and a *recent* session (a fresh timestamp).
//! Both carry the same query keyword, so a single search returns both and the
//! gate can compare their verdicts.
//!
//! Isolation mirrors the readiness gates: a fresh `tempdir` with
//! `HOME`/`XDG_*`/cwd redirected into it, `CASS_SEMANTIC_EMBEDDER=hash` to keep
//! semantic acquisition offline, and `NO_COLOR=1`. The `.12.2` bounded runner
//! (`spawn_with_timeout_or_diag`) turns a hang into a loud diagnostic instead of
//! a silent pass.
//!
//! The test is written panic-free (Result-returning + an `ensure` helper, no
//! `unwrap`/`expect`/`panic!`/`assert!`) so a new file stays UBS 0-critical.

mod util;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assert_cmd::cargo::cargo_bin;
use serde_json::{Value, json};

use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;

/// Bound for the one-time index that builds the fixture (two tiny seeded
/// sessions index in well under a second; the bound is slack for CI contention).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound for a single bounded `search` invocation.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Distinctive single token both seeded sessions carry, so one search returns
/// both.
const KEYWORD: &str = "trustscorefixtureunique";
/// The old session (indexed via the shared helper's fixed 2024 timestamp).
const OLD_SESSION: &str = "rollout-2024-04-24T10-00-00-trust-old.jsonl";
/// The recent session (seeded inline with a fresh timestamp).
const RECENT_SESSION: &str = "rollout-2026-recent-trust-fresh.jsonl";

const DAY_MS: i64 = 86_400_000;

/// Return `Err(msg)` when `cond` is false; the message closure is only paid for
/// on failure. Keeps the gate panic-free (no `assert!` panic surface).
fn ensure(cond: bool, msg: impl FnOnce() -> String) -> TestResult {
    if cond { Ok(()) } else { Err(msg().into()) }
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

/// A fresh isolated `(tempdir guard, home, data_dir)`.
fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let tmp = tempfile::TempDir::new()?;
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&data_dir)?;
    Ok((tmp, home, data_dir))
}

/// Current wall-clock epoch milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Seed a Codex rollout session whose message timestamp is `created_ms`, so the
/// indexed `created_at` lands at a caller-chosen age. Mirrors the shape of
/// `util::seed_codex_session` but with a controllable timestamp (the shared
/// helper hardcodes a fixed 2024 timestamp, which we reuse for the *old* case).
fn seed_codex_session_at(
    codex_home: &Path,
    filename: &str,
    keyword: &str,
    created_ms: i64,
) -> TestResult {
    let sessions = codex_home.join("sessions/2026/04/23");
    std::fs::create_dir_all(&sessions)?;
    let iso = |ms: i64| -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    };
    let workspace = codex_home.to_string_lossy().into_owned();
    let lines = [
        json!({
            "timestamp": iso(created_ms),
            "type": "session_meta",
            "payload": { "id": filename, "cwd": workspace, "cli_version": "0.42.0" },
        }),
        json!({
            "timestamp": iso(created_ms + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": keyword }],
            },
        }),
    ];
    let body = serialize_jsonl(&lines)?;
    std::fs::write(sessions.join(filename), body)?;
    Ok(())
}

/// Serialize JSON values to newline-delimited text (flat helper so the
/// per-line `to_string` allocation is not inside a loop in the caller).
fn serialize_jsonl(lines: &[Value]) -> Result<String, Box<dyn Error>> {
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(line)?);
        body.push('\n');
    }
    Ok(body)
}

/// Build a `cass` command with the fixture's isolation env.
fn cass_cmd(home: &Path, codex_home: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env("CODEX_HOME", codex_home)
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

/// Append the shared `--data-dir <dir>` tail to a base argv.
fn argv(base: &[&str], data_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
    v.push("--data-dir".to_string());
    v.push(data_dir.to_string_lossy().into_owned());
    v
}

/// Run a bounded search and return the parsed JSON payload.
fn run_search(
    home: &Path,
    codex_home: &Path,
    data_dir: &Path,
    base: &[&str],
    label: &str,
) -> Result<Value, Box<dyn Error>> {
    let args = argv(base, data_dir);
    let cmd = cass_cmd(home, codex_home, &args);
    let out = spawn_with_timeout_or_diag(cmd, label, Some(data_dir), SEARCH_TIMEOUT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("{label}: stdout not JSON: {e}; head: {}", head(&stdout)))?;
    Ok(value)
}

/// The hits array of a search payload.
fn hits(payload: &Value) -> Vec<Value> {
    payload
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Find the hit whose `source_path` contains `needle`.
fn hit_with_path<'a>(hits: &'a [Value], needle: &str) -> Option<&'a Value> {
    hits.iter().find(|h| {
        h.get("source_path")
            .and_then(Value::as_str)
            .is_some_and(|p| p.contains(needle))
    })
}

/// Ordered list of hit source paths (for ordering-stability checks).
fn source_paths(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .filter_map(|h| {
            h.get("source_path")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Check the structural invariants every live trust verdict must satisfy. Flat
/// helper (not called inside a loop body's textual scope) so the diagnostic
/// allocations stay out of the caller's loop.
fn check_trust_shape(hit: &Value, label: &str) -> TestResult {
    let trust = hit
        .get("trust")
        .ok_or_else(|| format!("{label}: hit missing trust block under --robot-meta"))?;
    ensure(
        trust.get("schema_version").and_then(Value::as_u64) == Some(1),
        || format!("{label}: trust.schema_version must be 1"),
    )?;
    let tier = trust
        .get("trust_tier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        matches!(
            tier,
            "trusted" | "likely" | "unverified" | "stale" | "failed"
        ),
        || format!("{label}: unexpected trust_tier `{tier}`"),
    )?;
    let confidence = trust
        .get("confidence")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(matches!(confidence, "low" | "medium" | "high"), || {
        format!("{label}: unexpected confidence `{confidence}`")
    })?;
    // Commit/bead/release correlation (q4pau) is project-scoped: it only fires
    // for hits whose workspace is the project the agent is in now. This fixture
    // runs from a throwaway tempdir that is not a git repo and whose seeded
    // workspace does not match, so correlation cannot fire and refs stay empty.
    // (The on-project case is proved by `search_on_project_correlation_links_bead`.)
    let refs_empty = trust
        .get("provenance_refs")
        .and_then(Value::as_array)
        .is_some_and(|a| a.is_empty());
    ensure(refs_empty, || {
        format!("{label}: provenance_refs should be empty for off-project hits")
    })?;
    // Not fully trusted in the live layer → an advisory follow-up is set.
    ensure(
        trust
            .get("recommended_followup")
            .and_then(Value::as_str)
            .is_some(),
        || format!("{label}: recommended_followup should be present for a non-trusted verdict"),
    )?;
    Ok(())
}

/// Assert a hit carries NO `trust` key. Flat helper so the diagnostic
/// allocation stays out of the caller's loop body.
fn ensure_no_trust(hit: &Value, context: &str) -> TestResult {
    ensure(hit.get("trust").is_none(), || {
        format!(
            "{context}: must not carry a trust key: {}",
            head(&hit.to_string())
        )
    })
}

/// The trust tier string of a hit (empty when absent).
fn tier_of(hit: &Value) -> &str {
    hit.get("trust")
        .and_then(|t| t.get("trust_tier"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn search_robot_meta_carries_trust_and_default_paths_do_not() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let codex_home = home.join(".codex");

    // Old session: the shared helper's fixed 2024 timestamp → older than the
    // stale window. Recent session: a couple of days old → fresh.
    util::seed_codex_session(&codex_home, OLD_SESSION, KEYWORD, false);
    seed_codex_session_at(&codex_home, RECENT_SESSION, KEYWORD, now_ms() - 2 * DAY_MS)?;

    // Index once.
    let index_args = argv(
        &["index", "--full", "--json", "--no-progress-events"],
        &data_dir,
    );
    let index_cmd = cass_cmd(&home, &codex_home, &index_args);
    let index_out = spawn_with_timeout_or_diag(
        index_cmd,
        "trust_fixture_index",
        Some(&data_dir),
        INDEX_TIMEOUT,
    );
    let index_stdout = String::from_utf8_lossy(&index_out.stdout);
    let index_json: Value = serde_json::from_str(index_stdout.trim())
        .map_err(|e| format!("index stdout not JSON: {e}; head: {}", head(&index_stdout)))?;
    ensure(
        index_json.get("success").and_then(Value::as_bool) == Some(true),
        || {
            format!(
                "index did not report success=true: {}",
                head(&index_json.to_string())
            )
        },
    )?;

    // --- with --robot-meta: every hit carries a well-formed trust verdict -----
    let meta = run_search(
        &home,
        &codex_home,
        &data_dir,
        &["search", KEYWORD, "--json", "--robot-meta", "--limit", "10"],
        "search_robot_meta",
    )?;
    let meta_hits = hits(&meta);
    ensure(meta_hits.len() >= 2, || {
        format!(
            "expected both seeded sessions indexed and returned, got {} hit(s): {}",
            meta_hits.len(),
            head(&meta.to_string())
        )
    })?;
    for hit in &meta_hits {
        let path = hit
            .get("source_path")
            .and_then(Value::as_str)
            .unwrap_or("<no-path>");
        check_trust_shape(hit, path)?;
    }

    // The aged session is `stale`/`aged_out`; the fresh one is `unverified`.
    let old_hit = hit_with_path(&meta_hits, "trust-old")
        .ok_or_else(|| format!("old session hit not found in {}", head(&meta.to_string())))?;
    ensure(tier_of(old_hit) == "stale", || {
        format!(
            "aged session should score `stale`, got `{}`",
            tier_of(old_hit)
        )
    })?;
    let old_reason = old_hit
        .get("trust")
        .and_then(|t| t.get("stale_reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(old_reason == "aged_out", || {
        format!("aged session stale_reason should be `aged_out`, got `{old_reason}`")
    })?;

    let recent_hit = hit_with_path(&meta_hits, "recent-trust").ok_or_else(|| {
        format!(
            "recent session hit not found in {}",
            head(&meta.to_string())
        )
    })?;
    // ubs:ignore — compares a public trust-tier label, not a secret or token.
    ensure(tier_of(recent_hit) == "unverified", || {
        format!(
            "fresh unlinked session should score `unverified`, got `{}`",
            tier_of(recent_hit)
        )
    })?;

    // --- without --robot-meta: the fast path carries NO trust key -------------
    let plain = run_search(
        &home,
        &codex_home,
        &data_dir,
        &["search", KEYWORD, "--json", "--limit", "10"],
        "search_plain",
    )?;
    let plain_hits = hits(&plain);
    ensure(!plain_hits.is_empty(), || {
        format!(
            "plain search returned no hits: {}",
            head(&plain.to_string())
        )
    })?;
    for hit in &plain_hits {
        ensure_no_trust(hit, "default (no --robot-meta) output")?;
    }

    // --- minimal projection: no trust even with --robot-meta ------------------
    let minimal = run_search(
        &home,
        &codex_home,
        &data_dir,
        &[
            "search",
            KEYWORD,
            "--json",
            "--robot-meta",
            "--fields",
            "minimal",
            "--limit",
            "10",
        ],
        "search_minimal_meta",
    )?;
    for hit in &hits(&minimal) {
        ensure_no_trust(
            hit,
            "minimal projection (source_path/line_number/agent only)",
        )?;
    }

    // --- ordering is identical with and without the advisory verdict ----------
    ensure(
        source_paths(&meta_hits) == source_paths(&plain_hits),
        || "trust verdict must not change hit ordering".to_string(),
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// q4pau: live source fs/archive probe and on-project commit/bead correlation.
// ---------------------------------------------------------------------------

/// Seed a Codex rollout session with a controllable workspace (`cwd`) and body
/// text, so a single search returns it and (for the on-project case) its content
/// references a known bead id.
fn seed_codex_session_full(
    codex_home: &Path,
    filename: &str,
    workspace: &str,
    body_text: &str,
    created_ms: i64,
) -> TestResult {
    let sessions = codex_home.join("sessions/2026/04/23");
    std::fs::create_dir_all(&sessions)?;
    let iso = |ms: i64| -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    };
    let lines = [
        json!({
            "timestamp": iso(created_ms),
            "type": "session_meta",
            "payload": { "id": filename, "cwd": workspace, "cli_version": "0.42.0" },
        }),
        json!({
            "timestamp": iso(created_ms + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": body_text }],
            },
        }),
    ];
    let body = serialize_jsonl(&lines)?;
    std::fs::write(sessions.join(filename), body)?;
    Ok(())
}

/// Build a `cass` command rooted at an explicit working directory (used so the
/// correlation index builds against a chosen git repo). Mirrors `cass_cmd` but
/// overrides `current_dir`.
fn cass_cmd_in(cwd: &Path, home: &Path, codex_home: &Path, args: &[String]) -> Command {
    let mut cmd = cass_cmd(home, codex_home, args);
    cmd.current_dir(cwd);
    cmd
}

/// Run a bounded git command, returning trimmed stdout on success.
fn git_run(repo: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()?;
    ensure(out.status.success(), || {
        format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Read the `trust` block of a hit, or an error.
fn trust_of<'a>(hit: &'a Value, label: &str) -> Result<&'a Value, Box<dyn Error>> {
    hit.get("trust")
        .ok_or_else(|| format!("{label}: hit missing trust block").into())
}

/// Whether a trust verdict carries one exact sanitized provenance reference.
fn has_provenance_ref(trust: &Value, expected: &str) -> bool {
    trust
        .get("provenance_refs")
        .and_then(Value::as_array)
        .is_some_and(|refs| refs.iter().filter_map(Value::as_str).any(|r| r == expected))
}

/// Resolve the current content-stable lesson id carrying `source_ref` from a
/// live `cass lessons list` payload.
fn lesson_id_for_source(payload: &Value, source_ref: &str) -> Option<String> {
    payload
        .get("lessons")
        .and_then(Value::as_array)?
        .iter()
        .find(|lesson| {
            lesson
                .get("source_refs")
                .and_then(Value::as_array)
                .is_some_and(|refs| {
                    refs.iter()
                        .filter_map(Value::as_str)
                        .any(|candidate| candidate == source_ref)
                })
        })?
        .get("lesson_id")?
        .as_str()
        .map(str::to_string)
}

/// The `stale_reason` string of a hit's trust verdict (empty when absent).
fn stale_reason_of(hit: &Value) -> String {
    hit.get("trust")
        .and_then(|t| t.get("stale_reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// q4pau Part 3: a local hit whose source file no longer exists on disk is
/// archive-only, so its verdict must report `source_unhealthy` at low confidence
/// — instead of overtrusting a dead path. Deterministic: index a session, delete
/// its source file, then re-search the unchanged index.
#[test]
fn search_source_probe_marks_deleted_source_unhealthy() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let codex_home = home.join(".codex");
    let keyword = "sourceprobefixtureunique";
    let filename = "rollout-2026-source-probe.jsonl";

    // A fresh session so recency alone would score `unverified` — the deleted
    // source is what must drive the verdict to `source_unhealthy`.
    seed_codex_session_full(
        &codex_home,
        filename,
        &codex_home.to_string_lossy(),
        keyword,
        now_ms() - 2 * DAY_MS,
    )?;

    let index_args = argv(
        &["index", "--full", "--json", "--no-progress-events"],
        &data_dir,
    );
    let index_cmd = cass_cmd(&home, &codex_home, &index_args);
    let index_out = spawn_with_timeout_or_diag(
        index_cmd,
        "source_probe_index",
        Some(&data_dir),
        INDEX_TIMEOUT,
    );
    ensure(
        index_out.status.success() || !index_out.stdout.is_empty(),
        || "index produced no output".to_string(),
    )?;

    // Delete the source file; the archive DB row + lexical index survive.
    let source_file = codex_home.join("sessions/2026/04/23").join(filename);
    std::fs::remove_file(&source_file)?;

    let payload = run_search(
        &home,
        &codex_home,
        &data_dir,
        &["search", keyword, "--json", "--robot-meta", "--limit", "5"],
        "source_probe_search",
    )?;
    let found = hits(&payload);
    let hit = hit_with_path(&found, "source-probe")
        .ok_or_else(|| format!("seeded hit not found: {}", head(&payload.to_string())))?;
    check_trust_shape(hit, "source_probe")?;
    let trust = trust_of(hit, "source_probe")?;
    ensure(stale_reason_of(hit) == "source_unhealthy", || {
        format!(
            "deleted-source hit should report stale_reason=source_unhealthy, got `{}`",
            stale_reason_of(hit)
        )
    })?;
    ensure(
        trust.get("confidence").and_then(Value::as_str) == Some("low"),
        || "deleted-source hit confidence should be low".to_string(),
    )?;
    Ok(())
}

/// q4pau Part 1+2: from inside a project (a git repo with a closed bead in
/// `.beads/issues.jsonl`), a hit from that same workspace whose indexed text
/// references the bead id is correlated to it: the verdict reaches `likely` and
/// carries a sanitized `bead:` provenance ref. Proves the live join + cwd-
/// relative workspace match end to end.
#[test]
fn search_on_project_correlation_links_bead() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let codex_home = home.join(".codex");

    // A throwaway project repo with a closed bead. `source_repo` drives the
    // id prefix (`corrproj-`); the bead id must start with it.
    let proj = home.join("proj");
    std::fs::create_dir_all(&proj)?;
    git_run(&proj, &["init", "-q"])?;
    let git_root = git_run(&proj, &["rev-parse", "--show-toplevel"])?;
    let beads_dir = proj.join(".beads");
    std::fs::create_dir_all(&beads_dir)?;
    let bead_id = "corrproj-corrbeadxyz";
    let bead_line = json!({
        "id": bead_id,
        "title": "Preserve correlated lesson provenance",
        "status": "closed",
        "source_repo": "corrproj",
        "issue_type": "task",
        "labels": ["trust-correlation"],
        "updated_at": "2026-07-20T12:00:00Z",
        "closed_at": "2026-07-21T12:00:00Z",
        "close_reason": "Use the corroborated closed-Bead decision in search and pack citations",
    });
    std::fs::write(
        beads_dir.join("issues.jsonl"),
        format!("{}\n", serde_json::to_string(&bead_line)?),
    )?;

    // Resolve the actual lesson id from the real live lessons surface. Trust
    // must reuse this content-stable id, not synthesize a parallel identifier.
    let lessons_args = ["lessons", "list", "--status", "all", "--json"].map(str::to_string);
    let lessons_cmd = cass_cmd_in(&proj, &home, &codex_home, &lessons_args);
    let lessons_out = spawn_with_timeout_or_diag(
        lessons_cmd,
        "correlation_lessons",
        Some(&data_dir),
        SEARCH_TIMEOUT,
    );
    let lessons_stdout = String::from_utf8_lossy(&lessons_out.stdout);
    let lessons_payload: Value = serde_json::from_str(lessons_stdout.trim()).map_err(|e| {
        format!(
            "correlation lessons stdout not JSON: {e}; head: {}",
            head(&lessons_stdout)
        )
    })?;
    let bead_source_ref = format!("bead:{bead_id}");
    let lesson_id = lesson_id_for_source(&lessons_payload, &bead_source_ref).ok_or_else(|| {
        format!(
            "live lessons did not derive a lesson for `{bead_source_ref}`: {}",
            head(&lessons_payload.to_string())
        )
    })?;
    ensure(
        lesson_id.starts_with("lsn-") && lesson_id.len() == "lsn-".len() + 16,
        || format!("live lessons returned malformed lesson id `{lesson_id}`"),
    )?;
    let lesson_provenance_ref = format!("lesson:{lesson_id}");

    // A session whose workspace is the project root and whose body references
    // the closed bead alongside the search keyword.
    let keyword = "corrfixtureuniqueword";
    let body = format!("{keyword} — landed in {bead_id} closeout");
    seed_codex_session_full(
        &codex_home,
        "rollout-2026-correlation.jsonl",
        &git_root,
        &body,
        now_ms() - 3 * DAY_MS,
    )?;

    let index_args = argv(
        &["index", "--full", "--json", "--no-progress-events"],
        &data_dir,
    );
    let index_cmd = cass_cmd_in(&proj, &home, &codex_home, &index_args);
    let index_out = spawn_with_timeout_or_diag(
        index_cmd,
        "correlation_index",
        Some(&data_dir),
        INDEX_TIMEOUT,
    );
    ensure(
        index_out.status.success() || !index_out.stdout.is_empty(),
        || "correlation index produced no output".to_string(),
    )?;

    // Search from inside the project so the correlation index builds against it.
    let args = argv(
        &["search", keyword, "--json", "--robot-meta", "--limit", "5"],
        &data_dir,
    );
    let cmd = cass_cmd_in(&proj, &home, &codex_home, &args);
    let out =
        spawn_with_timeout_or_diag(cmd, "correlation_search", Some(&data_dir), SEARCH_TIMEOUT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "correlation search stdout not JSON: {e}; head: {}",
            head(&stdout)
        )
    })?;
    let found = hits(&payload);
    let hit = hit_with_path(&found, "correlation")
        .ok_or_else(|| format!("correlation hit not found: {}", head(&payload.to_string())))?;
    check_trust_shape_on_project(hit, "correlation")?;

    // The closed-bead link lifts the verdict to `likely` with a sanitized ref.
    ensure(tier_of(hit) == "likely", || {
        format!(
            "on-project closed-bead hit should be `likely`, got `{}`",
            tier_of(hit)
        )
    })?;
    let trust = trust_of(hit, "correlation")?;
    let has_bead_ref = has_provenance_ref(trust, &bead_source_ref);
    ensure(has_bead_ref, || {
        format!(
            "expected provenance ref `{bead_source_ref}`, got {}",
            trust
                .get("provenance_refs")
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    })?;
    ensure(has_provenance_ref(trust, &lesson_provenance_ref), || {
        format!(
            "expected live lesson provenance ref `{lesson_provenance_ref}`, got {}",
            trust
                .get("provenance_refs")
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    })?;
    // On-project: the workspace match must NOT report a mismatch.
    ensure(stale_reason_of(hit) != "workspace_mismatch", || {
        "on-project hit must not report workspace_mismatch".to_string()
    })?;

    // The answer-pack path must attach the exact same corroborated lesson ref
    // while retaining the existing `likely` trust semantics.
    let pack_args = argv(
        &[
            "pack", keyword, "--json", "--mode", "lexical", "--limit", "5",
        ],
        &data_dir,
    );
    let pack_cmd = cass_cmd_in(&proj, &home, &codex_home, &pack_args);
    let pack_out = spawn_with_timeout_or_diag(
        pack_cmd,
        "correlation_pack",
        Some(&data_dir),
        SEARCH_TIMEOUT,
    );
    let pack_stdout = String::from_utf8_lossy(&pack_out.stdout);
    let pack_payload: Value = serde_json::from_str(pack_stdout.trim()).map_err(|e| {
        format!(
            "correlation pack stdout not JSON: {e}; head: {}",
            head(&pack_stdout)
        )
    })?;
    let pack_evidence = pack_payload
        .get("evidence")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find(|item| {
                item.pointer("/citation/source_path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| path.contains("correlation"))
            })
        })
        .ok_or_else(|| {
            format!(
                "correlation pack evidence not found: {}",
                head(&pack_payload.to_string())
            )
        })?;
    ensure(tier_of(pack_evidence) == "likely", || {
        format!(
            "on-project pack evidence should remain `likely`, got `{}`",
            tier_of(pack_evidence)
        )
    })?;
    let pack_trust = trust_of(pack_evidence, "correlation pack evidence")?;
    ensure(has_provenance_ref(pack_trust, &bead_source_ref), || {
        format!(
            "pack evidence missing `{bead_source_ref}`: {}",
            pack_trust
                .get("provenance_refs")
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    })?;
    ensure(
        has_provenance_ref(pack_trust, &lesson_provenance_ref),
        || {
            format!(
                "pack evidence missing `{lesson_provenance_ref}`: {}",
                pack_trust
                    .get("provenance_refs")
                    .map(ToString::to_string)
                    .unwrap_or_default()
            )
        },
    )?;
    Ok(())
}

/// Structural invariants for an on-project verdict (like [`check_trust_shape`]
/// but allows non-empty provenance refs and a `likely`/`trusted` tier).
fn check_trust_shape_on_project(hit: &Value, label: &str) -> TestResult {
    let trust = trust_of(hit, label)?;
    ensure(
        trust.get("schema_version").and_then(Value::as_u64) == Some(1),
        || format!("{label}: trust.schema_version must be 1"),
    )?;
    let tier = trust
        .get("trust_tier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        matches!(
            tier,
            "trusted" | "likely" | "unverified" | "stale" | "failed"
        ),
        || format!("{label}: unexpected trust_tier `{tier}`"),
    )?;
    Ok(())
}
