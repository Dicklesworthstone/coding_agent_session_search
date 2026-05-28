//! INV-cass-8 — `cass search` filter contracts: `--limit 0`, `--agent`,
//! and `--days` honor the documented semantics agents rely on.
//!
//! Four invariants are locked here:
//!
//!   1. **`--limit 0` is the "no cap" sentinel.** Project memory records a
//!      v0.6.x regression (`19d79089`) where `--limit 0` ran
//!      `take(0)` and silently dropped all results. The README's pack
//!      help says `0` uses the planner default; the same convention
//!      applies to search. This test guards against re-introducing the
//!      drop-all-results regression by asserting `--limit 0` returns at
//!      least as many hits as `--limit 1`.
//!
//!   2. **`--agent X` projects only matching hits.** Agents pass user-
//!      supplied agent names verbatim; a regression that quietly stops
//!      filtering would mix corpora across providers and silently break
//!      downstream automation. Locked against the fixture's known
//!      `aider`-only contents.
//!
//!   3. **`--agent X_unknown` is graceful zero-hit, not error.** The
//!      filter must never raise: agents catalog the supported agents
//!      via `cass capabilities --json` and may pass a name that has
//!      gone stale. Returning `hits == []` with exit 0 is the contract.
//!
//!   4. **`--days N` is monotone in N.** Strictly: `hits(--days N1) ⊆
//!      hits(--days N2)` whenever `N1 ≤ N2`. Equivalent practical form
//!      that this test asserts: a far-past horizon returns at least as
//!      many hits as a recent horizon. A regression breaking date-filter
//!      monotonicity (e.g., inclusive/exclusive boundary inversion) is
//!      caught immediately.
//!
//! Verified against the checked-in `search_demo_data` fixture with the
//! query `"the"` (yields 2 aider hits). The fixture's age is "before
//! today", which the monotonicity test relies on (`--days 1` returns 0
//! hits; `--days 36500` returns the full set).

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

/// Run `cass search "<q>" --robot --data-dir <fixture> <extra...>` and return
/// the parsed JSON. Asserts exit 0 — every contract below assumes a successful
/// filter application, not an error path.
fn run_search(data_dir: &Path, extra_args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search", "the", "--robot"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .args(extra_args)
        .output()?;
    let code = output.status.code().ok_or_else(|| {
        test_error("cass search was killed by signal — violates 'never panic' contract")
    })?;
    if code != 0 {
        return Err(test_error(format!(
            "cass search exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

fn hit_count(response: &Value) -> Result<usize, Box<dyn Error>> {
    response
        .get("hits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| test_error("response missing `hits` array"))
}

#[test]
fn search_limit_0_returns_all_available_hits_not_empty_set() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let limit_zero = run_search(&data_dir, &["--limit", "0"])?;
    let limit_one = run_search(&data_dir, &["--limit", "1"])?;

    let n_zero = hit_count(&limit_zero)?;
    let n_one = hit_count(&limit_one)?;

    // The regression caught by this assertion: --limit 0 used to run
    // `.take(0)` and drop every result. The current contract treats 0 as
    // "use the planner default" / no cap, so 0 must yield at least as many
    // hits as the explicit `--limit 1`.
    ensure(
        n_zero >= n_one,
        format!(
            "--limit 0 must mean 'no cap' (return all available hits), not 'return zero'.\n\
             got --limit 0 = {n_zero} hits; --limit 1 = {n_one} hits\n\
             A regression here re-introduces the take(0) drop-all bug."
        ),
    )?;
    Ok(())
}

#[test]
fn search_agent_filter_projects_only_matching_agent() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // The fixture's hits all have agent="aider". The filter must:
    //   - Return only those hits (positive correctness).
    //   - Mark every returned hit's `agent` field as "aider" (no leaks).
    let response = run_search(&data_dir, &["--agent", "aider"])?;
    let hits = response
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("missing `hits` array"))?;

    ensure(
        !hits.is_empty(),
        "--agent aider should return at least 1 hit against the fixture",
    )?;
    for (idx, hit) in hits.iter().enumerate() {
        check_hit_agent_matches(idx, hit, "aider")?;
    }
    Ok(())
}

/// Per-hit agent check, extracted so the diagnostic `format!` calls do not
/// live syntactically inside the caller's loop (UBS heuristic).
fn check_hit_agent_matches(idx: usize, hit: &Value, expected: &str) -> TestResult {
    let agent = hit
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error(format!("hit[{idx}] missing string `agent` field: {hit}")))?;
    ensure(
        agent == expected,
        format!("hit[{idx}] has agent={agent:?}, expected {expected:?} — --agent filter leaked"),
    )
}

#[test]
fn search_agent_filter_unknown_agent_returns_empty_set_not_error() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // Agents pass user-supplied / stale agent names; an unknown name must
    // produce a clean empty result, never an error envelope. The
    // run_search helper already asserts exit 0.
    let response = run_search(&data_dir, &["--agent", "zzz_nonexistent_agent_xyz"])?;
    let n = hit_count(&response)?;
    ensure(
        n == 0,
        format!("--agent <unknown> should produce 0 hits; got {n}"),
    )?;
    Ok(())
}

#[test]
fn search_days_filter_is_monotone_in_days() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // Monotonicity: a wider time horizon must never drop hits a narrower
    // one returned. The fixture's content predates "today" so:
    //   - `--days 1` returns 0 hits (nothing within the last 24h).
    //   - `--days 36500` (~100y) returns the full set.
    // The strict property is hits(N_small) ⊆ hits(N_large); the practical
    // form is the count comparison, which catches the same class of bugs
    // (inverted boundary, off-by-one date math, sign error).
    let n_small = hit_count(&run_search(&data_dir, &["--days", "1"])?)?;
    let n_large = hit_count(&run_search(&data_dir, &["--days", "36500"])?)?;
    ensure(
        n_large >= n_small,
        format!(
            "--days monotonicity violated: --days 36500 returned {n_large} hits, \
             which is less than --days 1's {n_small} hits"
        ),
    )?;
    // Additional sanity: a 100-year horizon must return at least one fixture hit.
    ensure(
        n_large >= 1,
        format!("--days 36500 should return at least 1 hit against the fixture; got {n_large}"),
    )?;
    Ok(())
}
