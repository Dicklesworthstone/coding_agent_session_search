//! INV-cass-11: `cass search --robot-format jsonl|compact`
//! emits output that satisfies the format's canonical line/JSON contract.
//!
//! Agents consume these streaming formats by splitting on newlines and parsing
//! each line. The contract that matters, independent of the volatile `_meta`
//! payload, is:
//!
//!   - `jsonl`:   every non-empty stdout line is independently valid JSON.
//!   - `compact`: stdout is exactly one line of valid JSON.
//!
//! Both invariants are corpus-independent, verified against the checked-in
//! search-demo fixture with a deliberately non-matching query for a
//! deterministic 0-hit envelope. They do NOT freeze the `_meta` content,
//! which is heavily host/time-dependent (host parallelism, loadavg, elapsed_ms,
//! age_seconds, paths, timestamps) and inappropriate to lock at the line-shape
//! level. The existing `golden_robot_json` harness owns content goldens via
//! `scrub_robot_json`.

use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use tempfile::TempDir;
use walkdir::WalkDir;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    std::io::Error::other(message.into()).into()
}

fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(test_error(message))
    }
}

fn safe_fixture_destination(dst_root: &Path, rel: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let mut dst = dst_root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => dst.push(part),
            _ => return Err(test_error("fixture path escaped source root")),
        }
    }
    Ok(dst)
}

/// Copy the checked-in search-demo fixture into a fresh temp data-dir so the
/// test reads from an isolated, byte-identical copy of the canonical DB +
/// lexical index (mirrors `tests/golden_robot_json::isolated_search_demo_data`).
fn copy_search_demo_fixture(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data");
    let dst_root = test_home.join("search_demo_data");
    for entry in WalkDir::new(&src) {
        let entry = entry?;
        // Machine-local frankensqlite namespace lock sidecars (created by
        // any local run that opens the fixture DB) must not reach the
        // clone: their foreign lock state fails the copied DB's open.
        if entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.ends_with("-fsqlite-ns-gate") || name.ends_with("-fsqlite-ns-use")
            })
        {
            continue;
        }
        let rel = entry.path().strip_prefix(&src)?;
        let dst = safe_fixture_destination(&dst_root, rel)?;
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &dst)?;
        }
    }
    // The lexical checkpoint intentionally binds a generation to its source
    // database. This isolated copy is byte-identical except for location, so
    // rewrite only that copied locator; otherwise the production robot
    // no-repair guard correctly refuses the mismatch before search begins.
    let copied_db_path = dst_root.join("agent_search.db");
    for entry in WalkDir::new(dst_root.join("index")) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.file_name() == std::ffi::OsStr::new(".lexical-rebuild-state.json")
        {
            let mut checkpoint: serde_json::Value =
                serde_json::from_slice(&fs::read(entry.path())?)?;
            checkpoint["db"]["db_path"] =
                serde_json::Value::String(copied_db_path.display().to_string());
            fs::write(entry.path(), serde_json::to_vec_pretty(&checkpoint)?)?;
        }
    }
    Ok(dst_root)
}

/// Run `cass search <args>` against the seeded fixture and return stdout.
fn run_search(data_dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search"])
        .args(args)
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    if !output.status.success() {
        return Err(test_error(format!(
            "cass search exited with {:?}; stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Split stdout into non-empty trimmed lines (the unit consumers parse).
fn output_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| !l.trim().is_empty()).collect()
}

fn parse_each_line_as_json(lines: &[&str]) -> Result<(), Box<dyn Error>> {
    if let Some((i, line)) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| serde_json::from_str::<serde_json::Value>(line).is_err())
    {
        let snippet: String = line.chars().take(120).collect();
        return Err(test_error(format!(
            "jsonl line {i} failed to parse as independent JSON: {snippet}..."
        )));
    }
    Ok(())
}

/// A deliberately non-matching query yields a deterministic 0-hit envelope:
/// the only fully corpus-independent shape we can rely on for a structural
/// contract test (we do NOT know the fixture's exact term inventory).
const NO_MATCH_QUERY: &str = "zzznomatchquery_xyz_unique_token_99";

#[test]
fn jsonl_every_line_is_independent_valid_json_with_robot_meta() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(
        &data_dir,
        &[NO_MATCH_QUERY, "--robot-format", "jsonl", "--robot-meta"],
    )?;
    let lines = output_lines(&stdout);
    ensure(
        !lines.is_empty(),
        "jsonl with --robot-meta must emit at least one envelope line",
    )?;
    parse_each_line_as_json(&lines)?;
    Ok(())
}

#[test]
fn jsonl_every_line_is_independent_valid_json_without_robot_meta() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(&data_dir, &[NO_MATCH_QUERY, "--robot-format", "jsonl"])?;
    let lines = output_lines(&stdout);
    parse_each_line_as_json(&lines)?;
    Ok(())
}

#[test]
fn compact_format_is_exactly_one_line_of_valid_json() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(&data_dir, &[NO_MATCH_QUERY, "--robot-format", "compact"])?;
    let lines = output_lines(&stdout);
    ensure(
        lines.len() == 1,
        format!(
            "compact format must be exactly one line; got {} non-empty lines",
            lines.len()
        ),
    )?;
    parse_each_line_as_json(&lines)?;
    Ok(())
}

#[test]
fn json_format_parses_as_a_single_json_document() -> TestResult {
    // The default `--robot` (pretty JSON) output is one document across
    // possibly-many pretty-printed lines. Concatenated stdout must parse as a
    // single JSON value; this is the contract distinct from jsonl/compact.
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(&data_dir, &[NO_MATCH_QUERY, "--robot"])?;
    let payload = serde_json::from_str::<serde_json::Value>(stdout.trim()).map_err(|err| {
        test_error(format!(
            "pretty --robot output is not a single JSON doc: {err}"
        ))
    })?;
    ensure(
        payload["budget"]["timed_out"] == false,
        "ordinary robot search unexpectedly exhausted its budget",
    )?;
    ensure(
        payload["budget"]["skipped_sections"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "ordinary robot search unexpectedly shed work",
    )?;
    Ok(())
}

#[test]
fn timed_out_search_returns_before_slow_operation_and_names_shed_sections() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let started = Instant::now();
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_TEST_SEARCH_SLOW_MS", "2000")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--robot-meta",
            "--mode",
            "lexical",
            "--rerank",
            "--explain",
            "--aggregate",
            "agent",
            "--timeout",
            "120",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    let wall_time = started.elapsed();
    ensure(
        output.status.success(),
        format!(
            "timed-out search failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let budget = &payload["budget"];
    let skipped = budget["skipped_sections"]
        .as_array()
        .ok_or_else(|| test_error("search budget skipped_sections is not an array"))?;

    ensure(
        budget["timed_out"] == true,
        format!("search timeout was not reported: {budget}"),
    )?;
    ensure(
        payload["hits"].as_array().is_some(),
        "search timeout did not return a valid partial hits array",
    )?;
    for section in [
        "search",
        "reranking",
        "explanation",
        "aggregations",
        "state_meta",
    ] {
        ensure(
            skipped.iter().any(|value| value == section),
            format!("search timeout omitted skipped section {section}: {budget}"),
        )?;
    }
    ensure(
        budget["recommended_next_probe"]
            .as_str()
            .is_some_and(|probe| probe.contains("cass search") && probe.contains("--timeout")),
        "search timeout did not recommend a bounded search retry",
    )?;
    ensure(
        payload.get("aggregations").is_none() && payload.get("explanation").is_none(),
        "search timeout serialized work that its budget says was skipped",
    )?;
    ensure(
        wall_time < Duration::from_millis(1_500),
        format!(
            "120ms search deadline waited for the 2000ms operation delay: wall_time={wall_time:?}"
        ),
    )?;
    Ok(())
}

#[test]
fn timed_out_search_setup_returns_partial_before_asset_validation_finishes() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let started = Instant::now();
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_TEST_SEARCH_SETUP_SLOW_MS", "2000")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--mode",
            "lexical",
            "--timeout",
            "120",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    ensure(
        output.status.success(),
        format!(
            "timed-out search setup failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    ensure(
        started.elapsed() < Duration::from_millis(1_500),
        "search setup escaped the configured hard deadline",
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let skipped = payload["budget"]["skipped_sections"]
        .as_array()
        .ok_or_else(|| test_error("search setup skipped_sections is not an array"))?;
    ensure(payload["budget"]["timed_out"] == true, payload.to_string())?;
    for section in ["search_setup", "search"] {
        ensure(
            skipped.iter().any(|value| value == section),
            format!("search setup timeout omitted {section}: {payload}"),
        )?;
    }
    ensure(
        payload["hits"].as_array().is_some_and(Vec::is_empty),
        "timed-out search setup fabricated hits",
    )?;
    Ok(())
}

#[test]
fn timed_out_search_meta_preserves_completed_hits_and_names_metadata_gaps() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let started = Instant::now();
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_TEST_SEARCH_META_SLOW_MS", "6000")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--robot-meta",
            "--mode",
            "lexical",
            "--timeout",
            "3000",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    ensure(
        output.status.success(),
        format!(
            "timed-out search metadata failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    ensure(
        started.elapsed() < Duration::from_millis(4_200),
        "search metadata escaped the configured hard deadline",
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let skipped = payload["budget"]["skipped_sections"]
        .as_array()
        .ok_or_else(|| test_error("search metadata skipped_sections is not an array"))?;
    ensure(
        payload["budget"]["timed_out"] == true,
        format!("search metadata timeout omitted timed_out=true: {payload}"),
    )?;
    for section in ["state_meta", "search_completeness"] {
        ensure(
            skipped.iter().any(|value| value == section),
            format!("search metadata timeout omitted {section}: {payload}"),
        )?;
    }
    ensure(
        payload["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "advisory metadata timeout discarded completed search evidence",
    )?;
    Ok(())
}

#[test]
fn timed_out_search_trust_projection_is_shed_without_discarding_hits() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let started = Instant::now();
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_TEST_SEARCH_TRUST_SLOW_MS", "6000")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--robot-meta",
            "--mode",
            "lexical",
            "--timeout",
            "3000",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    ensure(
        output.status.success(),
        format!(
            "timed-out trust projection failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    ensure(
        started.elapsed() < Duration::from_millis(4_200),
        "trust projection escaped the configured hard deadline",
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure(
        payload["budget"]["timed_out"] == true,
        format!("trust timeout omitted timed_out=true: {payload}"),
    )?;
    ensure(
        payload["budget"]["skipped_sections"]
            .as_array()
            .is_some_and(|sections| sections.iter().any(|value| value == "trust_correlation")),
        format!("trust timeout was not named: {payload}"),
    )?;
    ensure(
        payload["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "advisory trust timeout discarded completed search evidence",
    )?;
    Ok(())
}

#[test]
fn timed_out_sessions_format_fails_closed_with_empty_stdout() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_TEST_SEARCH_SLOW_MS", "2000")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot-format",
            "sessions",
            "--mode",
            "lexical",
            "--timeout",
            "120",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    ensure(
        !output.status.success(),
        "partial sessions stream must fail closed",
    )?;
    ensure(
        output.stdout.is_empty(),
        "partial sessions stream corrupted stdout pipeline input",
    )?;
    let diagnostic: serde_json::Value = serde_json::from_slice(&output.stderr)?;
    ensure(
        diagnostic["budget"]["timed_out"] == true,
        format!("sessions timeout diagnostic omitted its budget: {diagnostic}"),
    )?;
    ensure(
        diagnostic["budget"]["skipped_sections"]
            .as_array()
            .is_some_and(|sections| sections.iter().any(|value| value == "search")),
        format!("sessions timeout diagnostic omitted search: {diagnostic}"),
    )?;
    Ok(())
}

#[test]
fn robot_search_refresh_is_deferred_to_explicit_index_command() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--mode",
            "lexical",
            "--refresh",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    ensure(
        output.status.success(),
        format!(
            "robot refresh deferral failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let skipped = payload["budget"]["skipped_sections"]
        .as_array()
        .ok_or_else(|| test_error("robot refresh skipped_sections is not an array"))?;
    ensure(
        matches!(payload["budget"]["timed_out"].as_bool(), Some(false)),
        format!("deferred refresh was misreported as a timeout: {payload}"),
    )?;
    ensure(
        matches!(
            skipped.as_slice(),
            [section] if section.as_str().is_some_and(|value| value.eq("refresh"))
        ),
        format!("robot search did not isolate deferred refresh work: {payload}"),
    )?;
    let retry = payload["budget"]["recommended_next_probe"]
        .as_str()
        .ok_or_else(|| test_error("robot refresh omitted explicit index recommendation"))?;
    ensure(
        retry.contains("index") && retry.contains("--json") && retry.contains("--data-dir"),
        format!("robot refresh recommendation is not dataset-scoped: {retry}"),
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn blocking_sessions_file_search_returns_bounded_partial_without_broadening_scope() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let sessions_fifo = tmp.path().join("sessions.fifo");
    let mkfifo = std::process::Command::new("mkfifo")
        .arg(&sessions_fifo)
        .status()?;
    ensure(
        mkfifo.success(),
        format!("mkfifo failed with status {mkfifo:?}"),
    )?;

    let started = Instant::now();
    let output = Command::cargo_bin("cass")?
        .timeout(Duration::from_secs(3))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--mode",
            "lexical",
            "--timeout",
            "120",
            "--sessions-from",
        ])
        .arg(&sessions_fifo)
        .args(["--data-dir"])
        .arg(&data_dir)
        .output()?;
    let wall_time = started.elapsed();
    ensure(
        output.status.success(),
        format!(
            "FIFO-scoped search failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    ensure(
        wall_time < Duration::from_millis(1_500),
        format!("FIFO-scoped search exceeded its hard wall: {wall_time:?}"),
    )?;

    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let budget = &payload["budget"];
    let skipped = budget["skipped_sections"]
        .as_array()
        .ok_or_else(|| test_error("search FIFO skipped_sections is not an array"))?;
    ensure(
        budget["timed_out"] == true,
        format!("search FIFO timeout was not reported: {budget}"),
    )?;
    for section in ["sessions_from", "search"] {
        ensure(
            skipped.iter().any(|value| value == section),
            format!("search FIFO timeout omitted {section}: {budget}"),
        )?;
    }
    ensure(
        payload["hits"].as_array().is_some_and(Vec::is_empty),
        format!("search FIFO timeout broadened into archive hits: {payload}"),
    )?;
    let recommendation = budget["recommended_next_probe"]
        .as_str()
        .ok_or_else(|| test_error("search FIFO timeout omitted its file-scope retry"))?;
    ensure(
        recommendation.contains("--sessions-from")
            && recommendation.contains(&sessions_fifo.display().to_string()),
        format!("search FIFO retry lost its session file scope: {recommendation}"),
    )?;
    Ok(())
}

#[test]
fn explicit_semantic_timeout_never_substitutes_lexical_hits() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_TEST_SEARCH_SEMANTIC_SETUP_SLOW_MS", "2000")
        .args([
            "--color=never",
            "search",
            "hello",
            "--robot",
            "--robot-meta",
            "--mode",
            "semantic",
            "--timeout",
            "120",
            "--data-dir",
            data_dir.to_str().ok_or("non-utf8 path")?,
        ])
        .output()?;
    ensure(
        output.status.success(),
        format!(
            "semantic timeout failed: status={:?}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure(
        payload["_meta"]["requested_search_mode"] == "semantic",
        "semantic timeout lost requested mode",
    )?;
    ensure(
        payload["_meta"]["search_mode"] == "semantic",
        "semantic timeout silently substituted lexical mode",
    )?;
    ensure(
        payload["_meta"]["fallback_tier"].is_null(),
        "semantic timeout reported a lexical fallback tier",
    )?;
    ensure(
        payload["_meta"]["semantic_refinement"] == false,
        "semantic timeout claimed incomplete semantic work was completed",
    )?;
    ensure(
        payload["hits"].as_array().is_some_and(Vec::is_empty),
        "semantic timeout must not return lexical hits",
    )?;
    Ok(())
}

#[test]
fn timeout_budget_contract_is_present_in_compact_jsonl_and_toon() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    for format in ["compact", "jsonl", "toon"] {
        let output = Command::cargo_bin("cass")?
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_TEST_SEARCH_SLOW_MS", "2000")
            .args([
                "--color=never",
                "search",
                "hello",
                "--robot-format",
                format,
                "--mode",
                "lexical",
                "--timeout",
                "120",
                "--data-dir",
                data_dir.to_str().ok_or("non-utf8 path")?,
            ])
            .output()?;
        ensure(
            output.status.success(),
            format!(
                "{format} timeout failed: status={:?}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        )?;
        let stdout = String::from_utf8(output.stdout)?;
        if format == "toon" {
            ensure(
                stdout.contains("budget")
                    && stdout.contains("timed_out")
                    && stdout.contains("skipped_sections")
                    && stdout.contains("search")
                    && stdout.contains("--robot-format toon")
                    && !stdout.contains("&&"),
                format!("TOON timeout omitted the budget contract:\n{stdout}"),
            )?;
        } else {
            let document = if format == "jsonl" {
                stdout
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .ok_or_else(|| test_error("JSONL timeout emitted no header"))?
            } else {
                stdout.trim()
            };
            let payload: serde_json::Value = serde_json::from_str(document)?;
            ensure(
                payload["budget"]["timed_out"] == true,
                format!("{format} timeout omitted budget.timed_out"),
            )?;
            ensure(
                payload["budget"]["skipped_sections"]
                    .as_array()
                    .is_some_and(|sections| sections.iter().any(|section| section == "search")),
                format!("{format} timeout omitted the skipped search section"),
            )?;
            let retry = payload["budget"]["recommended_next_probe"]
                .as_str()
                .ok_or_else(|| test_error(format!("{format} timeout omitted retry command")))?;
            ensure(
                retry.contains(&format!("--robot-format {format}")) && !retry.contains("&&"),
                format!("{format} retry did not preserve its encoding: {retry}"),
            )?;
        }
    }
    Ok(())
}
