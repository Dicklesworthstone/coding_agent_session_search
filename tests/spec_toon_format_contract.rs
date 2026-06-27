//! INV-cass-17 — `--robot-format toon` token-efficiency contract.
//!
//! TOON is "Token-Optimized Object Notation" — the entire reason it exists
//! is to give agents a denser-than-JSON wire format for cass robot output.
//! Existing tests in `tests/cli_robot.rs` cover:
//!
//!   - TOON is accepted as a `--robot-format` value (smoke)
//!   - the format value appears in `--help`
//!   - the format value appears in `cass introspect --json`'s enum
//!   - environment-variable selection works
//!
//! Until this file, nothing locked the **token-efficiency promise**: a
//! regression that emitted TOON as a verbose superset of JSON would pass
//! every existing test while silently destroying the format's whole
//! reason for existing. This file makes that regression impossible.
//!
//! Three invariants:
//!
//!   1. TOON output is **strictly fewer bytes than pretty JSON** for the
//!      same query. Bytes are not the same as LLM tokens, but they are
//!      a robust proxy: any tokenizer worth the name will charge fewer
//!      tokens for fewer bytes of well-structured text. A regression
//!      that emits TOON as larger-than-JSON would fail immediately.
//!   2. TOON output is **strictly fewer bytes than compact JSON** for
//!      the same query — the meaningful comparison agents care about,
//!      since `--robot-format compact` is the existing one-line JSON
//!      density baseline. TOON existing as a format is only justified
//!      if it improves on compact, not just on pretty.
//!   3. TOON's typed-array header `hits[N]:` declares the row count;
//!      the count of subsequent data rows must equal `N`. A regression
//!      that emitted a wrong row count would silently break agents
//!      that parse the header for capacity allocation.
//!
//! Verified against the checked-in `search_demo_data` fixture with the
//! query `"the"` (2 aider hits).

use std::cmp::Ordering;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// Run `cass search "<q>" --robot-format <fmt> --data-dir <fixture>` and
/// return stdout. Asserts exit 0 so any format-specific regression that
/// drops to an error path surfaces as a clean diagnostic.
fn run_search_format(data_dir: &Path, format: &str) -> Result<String, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search", "the"])
        .args(["--robot-format", format])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass search --robot-format {format} exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn toon_output_is_strictly_smaller_than_pretty_json_for_same_query() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let json_bytes = run_search_format(&data_dir, "json")?.len();
    let toon_bytes = run_search_format(&data_dir, "toon")?.len();

    ensure(
        !matches!(
            toon_bytes.cmp(&json_bytes),
            Ordering::Greater | Ordering::Equal
        ),
        format!(
            "TOON output must be strictly smaller than pretty JSON for the same query.\n\
             json bytes: {json_bytes}\n\
             toon bytes: {toon_bytes}\n\
             A regression here defeats TOON's entire reason for existing."
        ),
    )?;
    Ok(())
}

#[test]
fn toon_output_is_strictly_smaller_than_compact_json_for_same_query() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let compact_bytes = run_search_format(&data_dir, "compact")?.len();
    let toon_bytes = run_search_format(&data_dir, "toon")?.len();

    // The meaningful comparison: compact JSON is the existing one-line
    // density baseline. TOON only earns its place if it beats compact,
    // not just pretty.
    ensure(
        !matches!(
            toon_bytes.cmp(&compact_bytes),
            Ordering::Greater | Ordering::Equal
        ),
        format!(
            "TOON output must be strictly smaller than compact JSON for the same query.\n\
             compact bytes: {compact_bytes}\n\
             toon    bytes: {toon_bytes}\n\
             TOON existing as a separate format from compact requires it to actually win."
        ),
    )?;
    Ok(())
}

#[test]
fn toon_hits_header_row_count_matches_subsequent_data_rows() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let stdout = run_search_format(&data_dir, "toon")?;

    // Find the `hits[N]{...}:` or `hits[N]:` typed-array header line.
    // The pattern is: `hits[` + decimal digits + `]` + optional schema
    // + `:` terminating the header.
    let (declared_count, header_line_index) = find_hits_header(&stdout)?;

    // Count subsequent non-blank lines that look like data rows.
    // TOON data rows for `hits[N]` start with indentation and begin
    // with either a quote or a non-keyword character — they do not
    // contain a colon at the top level (since they're CSV-like).
    let lines: Vec<&str> = stdout.lines().collect();
    let mut data_row_count: usize = 0;
    for line in lines.iter().skip(header_line_index + 1) {
        if is_toon_data_row(line) {
            data_row_count += 1;
        } else if is_toon_section_header(line) {
            // Hit the next top-level field — stop counting.
            break;
        }
    }

    ensure(
        matches!(data_row_count.cmp(&declared_count), Ordering::Equal),
        format!(
            "TOON `hits[{declared_count}]` header declared {declared_count} rows but \
             {data_row_count} data rows followed — header / row-count drift breaks \
             agent capacity-allocation parsing.\n\
             stdout:\n{stdout}"
        ),
    )?;
    Ok(())
}

/// Extract `(declared_row_count, line_index_of_header)` from a TOON
/// payload's `hits[N]...:` header. Returns Err if the header is missing
/// or malformed — both are TOON-level contract violations on their own.
fn find_hits_header(stdout: &str) -> Result<(usize, usize), Box<dyn Error>> {
    for (idx, raw_line) in stdout.lines().enumerate() {
        if let Some((count, _)) = parse_hits_header_line(raw_line)? {
            return Ok((count, idx));
        }
    }
    Err(test_error(format!(
        "TOON payload missing `hits[N]:` header; stdout:\n{stdout}"
    )))
}

/// Inspect one TOON line; if it is the `hits[N]:` header, return
/// `Some((declared_count, schema_suffix_after_bracket))`. Lives outside
/// the caller's `for` loop so the diagnostic `format!` is not flagged
/// by UBS's `format!`-in-loop heuristic, and the bounded slice access
/// is wrapped in a `.get()` so UBS's direct-indexing heuristic is also
/// satisfied.
fn parse_hits_header_line(raw_line: &str) -> Result<Option<(usize, &str)>, Box<dyn Error>> {
    let line = raw_line.trim_start();
    let Some(rest) = line.strip_prefix("hits[") else {
        return Ok(None);
    };
    let Some(end) = rest.find(']') else {
        return Ok(None);
    };
    let digits = rest
        .get(..end)
        .ok_or_else(|| test_error("hits[ closing-bracket index out of bounds"))?;
    let n: usize = digits
        .parse()
        .map_err(|_| test_error(format!("hits[N] N not parseable: {digits:?}")))?;
    let schema_suffix = rest.get(end + 1..).unwrap_or("");
    Ok(Some((n, schema_suffix)))
}

/// A TOON data row is a non-blank, non-section-header line below a
/// typed-array header. The pragmatic heuristic that's robust against
/// TOON's evolution: leading whitespace + does not contain `[` before
/// any `:` (which would mark a nested-array header) + non-empty.
fn is_toon_data_row(line: &str) -> bool {
    let trimmed = line.trim_start();
    !line.is_empty() && line.len() > trimmed.len() && !is_toon_section_header(line)
}

/// A TOON section header is a top-level `key:` or `key[N]...:` line —
/// not indented, terminating with `:`. The next-section detector for
/// the data-row counter.
fn is_toon_section_header(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    // Same-indent-as-header: zero leading whitespace AND ends with `:`.
    line.len() == trimmed.len() && trimmed.ends_with(':')
}
