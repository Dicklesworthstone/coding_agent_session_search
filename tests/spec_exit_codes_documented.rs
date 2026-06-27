//! INV-cass-6 — every exit code emitted by a `CliError` construction must
//! appear in the documented exit-code table.
//!
//! Regression guard for a real defect shipped 2026-05-25: a doctor quarantine
//! I/O path constructed `CliError { code: 73, kind: "io", .. }`. Code 73 is not
//! in the documented table (0-15, 20-24), so agents branching on the numeric
//! exit code received an undocumented value. The fix changed it to `code: 14`
//! (the documented `io | mapping` code); this test prevents recurrence.
//!
//! The check is intentionally one-directional: it asserts
//! `emitted ⊆ documented`. The reverse (every documented code is emitted) is
//! NOT asserted — code 8 ("partial result") is documented but currently has no
//! emission site, which is a known, separately-tracked doc/impl gap, not a
//! safety problem. Shipping an *undocumented* code is the dangerous direction,
//! and that is what this guards.
//!
//! Source of truth for the documented set: `cass robot-docs exit-codes`
//! (mirrored here as `DOCUMENTED_EXIT_CODES`).

use std::collections::BTreeSet;
use std::error::Error;

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

/// The documented exit-code table (`cass robot-docs exit-codes`).
/// 0-9 are the core semantic codes; 10-15 are domain-specific (branch on
/// `err.kind`, not the number); 20-24 are model-acquisition/IO codes.
const DOCUMENTED_EXIT_CODES: &[i32] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 20, 21, 22, 23, 24,
];

/// Every source file that constructs `CliError { code: N, kind: .. }`.
/// Embedded at compile time so the test has no filesystem dependency at run
/// time and stays correct under `rch` remote execution.
const SOURCES: &[(&str, &str)] = &[
    ("src/lib.rs", include_str!("../src/lib.rs")),
    ("src/doctor.rs", include_str!("../src/doctor.rs")),
    ("src/doctor_undo.rs", include_str!("../src/doctor_undo.rs")),
    ("src/doctor_runs.rs", include_str!("../src/doctor_runs.rs")),
];

/// Extract the integer from every `code: N,` line that is immediately followed
/// (next non-blank line) by a `kind:` line — the `CliError` field signature.
/// This scopes the scan to `CliError` constructions and ignores unrelated
/// `code:` fields, positional constructor calls, and non-numeric `code:` values.
fn emitted_cli_error_codes(src: &str) -> BTreeSet<i32> {
    let lines: Vec<&str> = src.lines().collect();
    let mut codes = BTreeSet::new();

    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("code: ") else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            continue; // e.g. `code: err.code` or `code: SOME_CONST`
        }
        // Confirm the next non-blank line is the `kind:` field — the CliError shape.
        // `skip` (not slice-index) keeps this panic-free regardless of `idx`.
        let next_non_blank = lines
            .iter()
            .skip(idx + 1)
            .map(|l| l.trim())
            .find(|l| !l.is_empty());
        if next_non_blank.is_some_and(|l| l.starts_with("kind:"))
            && let Ok(code) = digits.parse::<i32>()
        {
            codes.insert(code);
        }
    }

    codes
}

#[test]
fn every_emitted_exit_code_is_documented() -> TestResult {
    let documented: BTreeSet<i32> = DOCUMENTED_EXIT_CODES.iter().copied().collect();

    let mut undocumented: BTreeSet<i32> = BTreeSet::new();
    for (_path, src) in SOURCES {
        for code in emitted_cli_error_codes(src) {
            if !documented.contains(&code) {
                undocumented.insert(code);
            }
        }
    }

    ensure(
        undocumented.is_empty(),
        format!(
            "CliError emits undocumented exit code(s) {undocumented:?}. Every emitted code must be in \
         the documented table (cass robot-docs exit-codes: 0-15, 20-24). Either add the code to \
         the documented table + ERROR_CODES.md, or fix the construction site to use a documented \
         code. (Regression guard for the shipped `code: 73` defect.)"
        ),
    )
}

/// Sanity: the extractor must actually find the bulk of the CliError surface.
/// If this drops toward zero, the `code:`/`kind:` heuristic has drifted from
/// the real construction shape and `every_emitted_exit_code_is_documented`
/// would silently pass by finding nothing.
#[test]
fn extractor_finds_the_cli_error_surface() -> TestResult {
    let total: usize = SOURCES
        .iter()
        .map(|(_, src)| emitted_cli_error_codes(src).len())
        .sum::<usize>();
    // Distinct codes per file collapse to a small set; assert we at least see
    // the core spread (0-9 plus several domain codes) rather than nothing.
    let distinct: BTreeSet<i32> = SOURCES
        .iter()
        .flat_map(|(_, src)| emitted_cli_error_codes(src))
        .collect();
    ensure(
        distinct.len() >= 10 && total >= 10,
        format!(
            "exit-code extractor found only {} distinct codes ({} file-totals); the code:/kind: \
         heuristic has likely drifted from the CliError construction shape",
            distinct.len(),
            total
        ),
    )
}
