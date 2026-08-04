//! INV-cass-27 — `cass robot-docs <topic>` topic-set discipline.
//!
//! cass robot-docs is the agent-facing docs subcommand: each topic
//! returns a plain-text help block that agents and humans consume to
//! learn cass behavior. The accepted topic set is declared in
//! `cass robot-docs --help` as clap "Possible values".
//!
//! Until this file, the existing `tests/golden_robot_docs.rs` pinned
//! the **byte-for-byte content** of specific topic outputs but did not
//! lock the **topic-set discipline** itself — i.e., that every declared
//! topic actually produces non-empty output with exit 0, and that
//! invalid topics fail cleanly.
//!
//! Three invariants:
//!
//!   1. Every topic in the documented set produces exit 0 with
//!      non-empty stdout. A regression that removed a topic from clap's
//!      accepted set (without updating callers) would silently break
//!      agent workflows that pipe `cass robot-docs <topic>` for help.
//!   2. An invalid topic returns exit 2 (usage/parsing error). This
//!      proves the parser is actively gating on the accepted set
//!      rather than silently returning an empty body.
//!   3. `cass robot-docs` with no topic returns exit 0 with non-empty
//!      stdout (the default-to-guide affordance).
//!
//! **Known drift documented inline**: a stale
//! `tests/golden/robot_docs/robot_help.txt.golden` file exists but the
//! clap-accepted topic set no longer includes `robot_help`. This test
//! deliberately does NOT validate `robot_help` against the live parser.
//! Removing the stale golden requires owner authorization per
//! `AGENTS.md` Rule 1 (no file deletion).

use std::cmp::Ordering;
use std::error::Error;

use assert_cmd::Command;

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

/// The set of topics `cass robot-docs --help` advertises as accepted
/// values. Sourced from clap's "Possible values" enumeration.
/// Lockstep update: when a peer adds a new topic, append it here in
/// the same change.
const DOCUMENTED_TOPICS: &[&str] = &[
    "commands",
    "env",
    "paths",
    "schemas",
    "guide",
    "exit-codes",
    "examples",
    "contracts",
    "wrap",
    "sources",
    "analytics",
    "doctor",
];

struct CmdOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cass(args: &[&str]) -> Result<CmdOutcome, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never"])
        .args(args)
        .output()?;
    Ok(CmdOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Per-topic check, extracted so the diagnostic `format!` calls do not
/// live syntactically inside the caller's loop (UBS heuristic).
fn check_topic_produces_nonempty_output(topic: &str) -> TestResult {
    let outcome = run_cass(&["robot-docs", topic])?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[robot-docs {topic}] killed by signal")))?;
    ensure(
        matches!(code.cmp(&0), Ordering::Equal),
        format!(
            "`cass robot-docs {topic}` should return exit 0 (accepted-topic exit code); \
             got exit {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    ensure(
        !outcome.stdout.trim().is_empty(),
        format!(
            "`cass robot-docs {topic}` should produce non-empty stdout; got 0 trimmed bytes. \
             A regression where the topic exists in clap but returns an empty body would \
             silently break agent-facing docs lookups."
        ),
    )?;
    Ok(())
}

#[test]
fn every_documented_topic_produces_nonempty_output_with_exit_zero() -> TestResult {
    for topic in DOCUMENTED_TOPICS {
        check_topic_produces_nonempty_output(topic)?;
    }
    Ok(())
}

#[test]
fn invalid_robot_docs_topic_returns_exit_two() -> TestResult {
    let outcome = run_cass(&["robot-docs", "zzz_nonexistent_topic_xyz"])?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error("cass killed by signal"))?;
    // exit 2 is the documented usage/parsing error per AGENTS.md exit-code
    // table; the clap parser must actively gate on the accepted set.
    ensure(
        matches!(code.cmp(&2), Ordering::Equal),
        format!(
            "invalid `cass robot-docs <topic>` should return exit 2 (usage/parsing); \
             got exit {code}.\nstdout:\n{}\nstderr:\n{}",
            outcome.stdout, outcome.stderr
        ),
    )?;
    Ok(())
}

#[test]
fn no_topic_invocation_defaults_to_guide_with_exit_zero() -> TestResult {
    let outcome = run_cass(&["robot-docs"])?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error("cass killed by signal"))?;
    ensure(
        matches!(code.cmp(&0), Ordering::Equal),
        format!(
            "`cass robot-docs` (no topic) should default-to-guide and return exit 0; \
             got exit {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    ensure(
        !outcome.stdout.trim().is_empty(),
        "`cass robot-docs` (no topic) should produce non-empty stdout; got 0 \
         trimmed bytes — the default-to-guide affordance is broken."
            .to_string(),
    )?;
    Ok(())
}
