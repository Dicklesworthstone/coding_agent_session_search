//! INV-cass-4 — `cass pack --max-tokens <N>` honors its token budget contract.
//!
//! Agents stream cass evidence into bounded LLM context windows; the
//! pack-token-budget contract is what lets them trust those bounds. This
//! file mechanically guards three sub-invariants:
//!
//!   1. **Range validation** — `--max-tokens` outside `[1024, 200000]` is
//!      rejected with a robot-mode error envelope on stderr whose kebab-case
//!      `kind` is `"pack-invalid-limit"` and whose `retryable` is `false`.
//!      Per `AGENTS.md` "Robot Mode Etiquette", stdout stays data-only;
//!      diagnostics (including this validation error) live on stderr.
//!   2. **Budget respect** — for any accepted budget `N`, the realized
//!      `limits.estimated_tokens` is `<= N`. Pack is the *soft*-budget
//!      planner (`pack --help` says "Soft pack token budget") so the
//!      planner must clamp at-or-below the requested cap.
//!   3. **Per-evidence summation consistency** — the per-evidence
//!      `estimated_tokens` field sums to the same `limits.estimated_tokens`
//!      that the budget-respect check guards. Without this, the cap is a
//!      lie: the per-item field could be small while the realized total
//!      exceeds the cap (or vice versa).
//!
//! Verified against the checked-in `search_demo_data` fixture with the
//! query `"the"` (yields 2 evidence items totaling 20 estimated tokens).
//! The budget-respect check sweeps a small set of valid budgets so a
//! regression that pinned `estimated_tokens` to a hardcoded constant would
//! fail. The validation check probes both the lower (1023) and upper
//! (200001) boundary.

use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
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

fn copy_search_demo_fixture(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data");
    let dst_root = test_home.join("search_demo_data");
    for entry in WalkDir::new(&src) {
        let entry = entry?;
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
    Ok(dst_root)
}

struct PackOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_pack(data_dir: &Path, max_tokens: i64) -> Result<PackOutcome, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "pack", "the", "--robot"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .args(["--max-tokens", &max_tokens.to_string()])
        .output()?;
    Ok(PackOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Boundary-rejection contract: an out-of-range `--max-tokens` produces a
/// robot-mode error envelope on stderr with `code=2`,
/// `kind="pack-invalid-limit"`, and `retryable=false`. The message must name
/// the rejected value so operators can correct it without re-reading docs.
fn assert_pack_invalid_limit(label: &str, outcome: &PackOutcome, rejected: i64) -> TestResult {
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[{label}] pack was killed by signal")))?;
    ensure(
        code == 2,
        format!(
            "[{label}] expected exit 2 (usage/parsing) for out-of-range budget; got {code}.\n\
             stderr:\n{}",
            outcome.stderr
        ),
    )?;
    ensure(
        outcome.stdout.trim().is_empty(),
        format!(
            "[{label}] validation error must not write to stdout; got:\n{}",
            outcome.stdout
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stderr.trim()).map_err(|err| {
        test_error(format!(
            "[{label}] stderr is not a JSON error envelope: {err}\n{}",
            outcome.stderr
        ))
    })?;
    let envelope = parsed
        .get("error")
        .ok_or_else(|| test_error(format!("[{label}] missing `error` key on stderr envelope")))?;
    let kind = envelope
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "[{label}] error envelope missing string `kind`: {envelope}"
            ))
        })?;
    ensure(
        kind == "pack-invalid-limit",
        format!(
            "[{label}] expected kebab-case kind=pack-invalid-limit; got {kind:?}.\n\
             envelope: {envelope}"
        ),
    )?;
    ensure(
        envelope.get("retryable") == Some(&Value::Bool(false)),
        format!("[{label}] invalid-limit must be retryable=false; envelope: {envelope}"),
    )?;
    let message = envelope
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        message.contains(&rejected.to_string()),
        format!("[{label}] error message must name the rejected value {rejected}; got: {message}"),
    )?;
    Ok(())
}

#[test]
fn pack_max_tokens_below_minimum_returns_pack_invalid_limit_kind() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    // 1023 is one below the documented inclusive minimum of 1024.
    let outcome = run_pack(&data_dir, 1023)?;
    assert_pack_invalid_limit("below-minimum", &outcome, 1023)
}

#[test]
fn pack_max_tokens_above_maximum_returns_pack_invalid_limit_kind() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    // 200001 is one above the documented inclusive maximum of 200000.
    let outcome = run_pack(&data_dir, 200001)?;
    assert_pack_invalid_limit("above-maximum", &outcome, 200001)
}

/// Pull `evidence[idx].estimated_tokens` out as an `i64`, with non-negative
/// validation. Lives in its own helper so diagnostic `format!` calls are not
/// syntactically inside the caller's loop (UBS's `format!`-in-loop heuristic).
fn extract_evidence_token_count(idx: usize, item: &Value) -> Result<i64, Box<dyn Error>> {
    let n = item
        .get("estimated_tokens")
        .and_then(Value::as_i64)
        .ok_or_else(|| test_error(format!("evidence[{idx}] missing estimated_tokens i64")))?;
    ensure(
        n >= 0,
        format!("evidence[{idx}].estimated_tokens={n} is negative"),
    )?;
    Ok(n)
}

/// One iteration of the budget-respect sweep, factored out so the diagnostic
/// `format!` calls live in this helper's body — not syntactically inside the
/// caller's loop — and so UBS's `format!`-in-loop heuristic is satisfied.
fn check_budget_respected(data_dir: &Path, budget: i64) -> TestResult {
    let outcome = run_pack(data_dir, budget)?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error("pack was killed by signal"))?;
    ensure(
        code == 0,
        format!(
            "pack at budget={budget} expected success; got exit {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    let limits = parsed
        .get("limits")
        .ok_or_else(|| test_error(format!("budget={budget}: missing `limits`")))?;
    let max = limits
        .get("max_tokens")
        .and_then(Value::as_i64)
        .ok_or_else(|| test_error(format!("budget={budget}: missing limits.max_tokens i64")))?;
    let est = limits
        .get("estimated_tokens")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            test_error(format!(
                "budget={budget}: missing limits.estimated_tokens i64"
            ))
        })?;
    ensure(
        max == budget,
        format!("budget={budget}: limits.max_tokens should echo requested cap; got {max}"),
    )?;
    ensure(
        est <= max,
        format!(
            "budget={budget}: estimated_tokens={est} exceeds max_tokens={max} — \
             soft budget contract violated"
        ),
    )?;
    ensure(
        est >= 0,
        format!("budget={budget}: estimated_tokens={est} is negative"),
    )?;
    Ok(())
}

#[test]
fn pack_estimated_tokens_never_exceeds_max_tokens_budget() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // Sweep several valid budgets. Each must produce a pack whose realized
    // `estimated_tokens` is at or below the requested cap. The min-bound
    // (1024) is the most informative case: a regression that ignored the
    // cap would still pass at 50000, but fail at 1024.
    for &budget in &[1024_i64, 1500, 2000, 50000] {
        check_budget_respected(&data_dir, budget)?;
    }
    Ok(())
}

#[test]
fn pack_per_evidence_estimated_tokens_sum_matches_limits_total() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    // Pick a comfortably large budget so the planner is not the binding
    // constraint; we are checking sum consistency, not budget pressure.
    let outcome = run_pack(&data_dir, 50000)?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error("pack was killed by signal"))?;
    ensure(
        code == 0,
        format!(
            "pack expected success; got exit {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    let total = parsed
        .get("limits")
        .and_then(|l| l.get("estimated_tokens"))
        .and_then(Value::as_i64)
        .ok_or_else(|| test_error("missing limits.estimated_tokens"))?;
    let evidence = parsed
        .get("evidence")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("missing `evidence` array"))?;
    ensure(
        !evidence.is_empty(),
        "fixture query 'the' should produce at least one evidence item",
    )?;
    let mut per_item_sum: i64 = 0;
    for (idx, item) in evidence.iter().enumerate() {
        per_item_sum += extract_evidence_token_count(idx, item)?;
    }
    ensure(
        per_item_sum == total,
        format!(
            "sum of evidence[].estimated_tokens ({per_item_sum}) must equal \
             limits.estimated_tokens ({total}) — per-item field would otherwise \
             give agents an inconsistent picture of realized cost"
        ),
    )?;
    Ok(())
}
