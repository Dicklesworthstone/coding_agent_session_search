//! Golden-file tests for `cass robot-docs <topic>` plain-text output.
//!
//! Bead `3pjoy` (u9osp follow-up): the LLM-facing `robot-docs` surface
//! emits bounded plain text per topic. Some topics (`exit-codes`, `env`,
//! `schemas`) are host-independent. Others (`paths`) embed the resolved
//! data-dir, so we pin `XDG_DATA_HOME` / `HOME` and then scrub the test
//! home prefix to `[TEST_HOME]` before comparison.
//!
//! ## Regenerate
//!
//! ```bash
//! UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_golden_docs cargo test --test golden_robot_docs
//! git diff tests/golden/robot_docs/
//! ```

use assert_cmd::Command;
use std::path::{Path, PathBuf};

/// Build a `cass` invocation with knobs pinned for deterministic text.
fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("HOME", test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1");
    cmd
}

/// Scrub host-specific bits. Today that's only the test-home path — the
/// remaining fields (`exit-codes`, `env`, `schemas`) are host-independent
/// constants emitted by the topic generator.
fn scrub_robot_docs(input: &str, test_home: &Path) -> String {
    let home_str = test_home.display().to_string();
    if home_str.is_empty() {
        input.to_string()
    } else {
        input.replace(&home_str, "[TEST_HOME]")
    }
}

/// `assert_golden` mirrors the helper in `tests/golden_robot_json.rs`:
/// `UPDATE_GOLDENS=1` regenerates the file; otherwise diff against it.
fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).expect("mkdir goldens");
        std::fs::write(&golden_path, actual).expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden missing: {}\n{err}\n\n\
             UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=${{TMPDIR:-/tmp}}/rch_target_cass_golden_docs cargo test --test golden_robot_docs\n\
             git diff tests/golden/ && git add tests/golden/",
            golden_path.display(),
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        std::fs::write(&actual_path, actual).expect("write .actual");
        panic!(
            "GOLDEN MISMATCH: {name}\nExpected: {}\nActual:   {}",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn capture_docs(topic: &str) -> String {
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = cass_cmd(test_home.path())
        .args(["robot-docs", topic])
        .output()
        .unwrap_or_else(|err| panic!("run cass robot-docs {topic}: {err}"));
    assert!(
        out.status.success(),
        "cass robot-docs {topic} exited non-zero: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    scrub_robot_docs(&stdout, test_home.path())
}

#[test]
fn robot_docs_exit_codes_matches_golden() {
    assert_golden(
        "robot_docs/exit-codes.txt.golden",
        &capture_docs("exit-codes"),
    );
}

#[test]
fn robot_docs_env_matches_golden() {
    assert_golden("robot_docs/env.txt.golden", &capture_docs("env"));
}

#[test]
fn robot_docs_paths_matches_golden() {
    assert_golden("robot_docs/paths.txt.golden", &capture_docs("paths"));
}

#[test]
fn robot_docs_schemas_matches_golden() {
    assert_golden("robot_docs/schemas.txt.golden", &capture_docs("schemas"));
}

// `coding_agent_session_search-5fiqq`: pre-fix, only 4 of the 11
// `RobotTopic` enum variants (src/lib.rs:1552) had frozen goldens —
// exit-codes, env, paths, schemas. The other 7 surfaces (commands,
// guide, examples, contracts, wrap, sources, analytics) emit bounded
// plain text via `print_robot_docs` / `render_*_docs` helpers in
// src/lib.rs but were unfrozen, so silent reword/reorder/drop
// regressions on any of those surfaces would slip through CI.
//
// These 7 tests close that gap by freezing every remaining topic.
// Together with the original 4 they pin the full LLM-facing
// `robot-docs <topic>` contract (11/11) so any drift on a
// machine-readable agent surface fails loudly.

#[test]
fn robot_docs_commands_matches_golden() {
    assert_golden("robot_docs/commands.txt.golden", &capture_docs("commands"));
}

#[test]
fn robot_docs_guide_matches_golden() {
    assert_golden("robot_docs/guide.txt.golden", &capture_docs("guide"));
}

#[test]
fn robot_docs_examples_matches_golden() {
    assert_golden("robot_docs/examples.txt.golden", &capture_docs("examples"));
}

#[test]
fn robot_docs_contracts_matches_golden() {
    assert_golden(
        "robot_docs/contracts.txt.golden",
        &capture_docs("contracts"),
    );
}

#[test]
fn robot_docs_wrap_matches_golden() {
    assert_golden("robot_docs/wrap.txt.golden", &capture_docs("wrap"));
}

#[test]
fn robot_docs_sources_matches_golden() {
    assert_golden("robot_docs/sources.txt.golden", &capture_docs("sources"));
}

#[test]
fn robot_docs_analytics_matches_golden() {
    assert_golden(
        "robot_docs/analytics.txt.golden",
        &capture_docs("analytics"),
    );
}

#[test]
fn robot_docs_recipes_matches_golden() {
    // Bead .11.3: canonical recovery + workflow recipes topic. Body lives in
    // src/recipes_robot_docs.rs; the safety/completeness invariants are pinned
    // in tests/spec_robot_docs_topics.rs.
    assert_golden("robot_docs/recipes.txt.golden", &capture_docs("recipes"));
}

/// Capture plain-text `--robot-help` output in an isolated home. The
/// robot-help string is the top-level "start here" contract surface
/// agents read on first contact; keeping it stable is load-bearing.
fn capture_robot_help() -> String {
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = cass_cmd(test_home.path())
        .args(["--robot-help"])
        .output()
        .unwrap_or_else(|err| panic!("run cass --robot-help: {err}"));
    assert!(
        out.status.success(),
        "cass --robot-help exited non-zero: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    scrub_robot_docs(&stdout, test_home.path())
}

#[test]
fn robot_help_matches_golden() {
    // ibuuh.36 verification-matrix row: --robot-help is the top-level
    // LLM onboarding surface. Topics, subcommand list, exit codes, and
    // example invocations are all printed here as a bounded static
    // block. Silent removal of any line breaks agent workflows — freeze
    // the contract so drift fails CI loudly.
    assert_golden("robot_docs/robot_help.txt.golden", &capture_robot_help());
}

#[test]
fn pack_robot_docs_contract_matrix_is_current() {
    let commands = capture_docs("commands");
    let examples = capture_docs("examples");
    let guide = capture_docs("guide");
    let schemas = capture_docs("schemas");
    let robot_help = capture_robot_help();

    for (surface, text, snippets) in [
        (
            "commands",
            commands.as_str(),
            &[
                "cass pack <query> [--robot] [--max-tokens N] [--limit N]",
                "--sessions-from FILE|-",
                "--freshness-window-seconds N",
                "--max-excerpt-chars N",
                "--explain-selection",
            ][..],
        ),
        (
            "examples",
            examples.as_str(),
            &[
                "cass pack \"why did checkout fail\" --robot --max-tokens 12000 --limit 40",
                "--freshness-policy strict --freshness-window-seconds 604800 --require-evidence",
                "--max-tokens 4000 --max-evidence 8 --max-sessions 3 --max-excerpt-chars 600",
                "--fields summary,health,freshness,privacy,warnings",
                "rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_answer_pack_docs cargo test --test golden_robot_docs",
            ][..],
        ),
        (
            "guide",
            guide.as_str(),
            &[
                "cass pack \"query\" --robot",
                "external model",
                "Pack vs search",
                "Pack vs export-html",
                "Pack warnings",
                "Pack budgets",
            ][..],
        ),
        (
            "schemas",
            schemas.as_str(),
            &[
                "pack:",
                "schema_version:",
                "evidence:",
                "warnings:",
                "stale_evidence_count:",
                "redaction_applied:",
                "max_excerpt_chars:",
            ][..],
        ),
        (
            "robot_help",
            robot_help.as_str(),
            &[
                "cass pack \"your query\" --robot --max-tokens 12000",
                "cass status --json",
                "Pack warnings expose freshness, semantic fallback, and privacy redactions.",
                "Core subcommands: search | pack | sessions",
            ][..],
        ),
    ] {
        assert!(
            !text.contains("stale_evidence_selected"),
            "pack robot-docs contract documents nonexistent stale_evidence_selected warning in {surface}"
        );

        for snippet in snippets {
            assert!(
                text.contains(snippet),
                "pack robot-docs contract missing `{snippet}` from {surface}"
            );
        }
    }
}

#[test]
fn answer_pack_workflow_docs_cover_agent_and_operator_paths() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let docs = [
        (
            "README.md",
            std::fs::read_to_string(root.join("README.md")).expect("read README.md"),
            &[
                "cass pack \"checkout timeout after redirect\" --robot",
                "--freshness-policy strict --freshness-window-seconds 604800",
                "--max-tokens 4000 --max-evidence 8 --max-sessions 3 --max-excerpt-chars 600",
                "privacy_redactions_applied",
                "Use `search` when you are still exploring candidate sessions. Use `pack`",
                "export-html",
                "rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_answer_pack_docs",
            ][..],
        ),
        (
            "docs/ROBOT_MODE.md",
            std::fs::read_to_string(root.join("docs/ROBOT_MODE.md"))
                .expect("read docs/ROBOT_MODE.md"),
            &[
                "## Pack handoff workflow",
                "Pack vs search/export-html/doctor/status",
                "does not call an external summarizer",
                "source logs.",
                "privacy_redactions_applied",
                "err.kind=\"not-found\"",
                "Do not run bare `cass` in automation.",
            ][..],
        ),
        (
            "docs/planning/ANSWER_PACKS_CONTRACT.md",
            std::fs::read_to_string(root.join("docs/planning/ANSWER_PACKS_CONTRACT.md"))
                .expect("read docs/planning/ANSWER_PACKS_CONTRACT.md"),
            &["| `not-found` | 13 | false | No evidence and `--require-evidence` is set. |"][..],
        ),
        (
            "docs/planning/ANSWER_PACKS_CONFORMANCE_MATRIX.md",
            std::fs::read_to_string(root.join("docs/planning/ANSWER_PACKS_CONFORMANCE_MATRIX.md"))
                .expect("read docs/planning/ANSWER_PACKS_CONFORMANCE_MATRIX.md"),
            &[
                "`--require-evidence` turns an empty pack into `err.kind=\"not-found\"` with code 13.",
            ][..],
        ),
    ];

    for (surface, text, snippets) in docs {
        assert!(
            !text.contains("stale_evidence_selected"),
            "answer-pack workflow docs document nonexistent stale_evidence_selected warning in {surface}"
        );

        for snippet in snippets {
            assert!(
                text.contains(snippet),
                "answer-pack workflow docs missing `{snippet}` from {surface}"
            );
        }
    }
}

#[test]
fn doctor_runbook_documents_archive_first_safety_contract() {
    let runbook_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("planning")
        .join("RECOVERY_RUNBOOK.md");
    let runbook = std::fs::read_to_string(&runbook_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", runbook_path.display()));

    for snippet in [
        "cass doctor check --json",
        "cass doctor archive-scan --json",
        "cass doctor repair --dry-run --json",
        "cass doctor repair --yes --plan-fingerprint <plan_fingerprint> --json",
        "cass doctor --fix --json",
        "cass doctor backups list --json",
        "cass doctor backups verify <backup_id> --json",
        "cass doctor backups restore <backup_id> --json",
        "cass doctor backups restore <backup_id> --yes --plan-fingerprint <plan_fingerprint> --json",
        "cass doctor cleanup --json",
        "cass doctor archive-normalize --dry-run --json",
        "cass doctor archive export /absolute/target/cass-archive-export --json",
        "cass doctor archive export verify /absolute/target/cass-archive-export --json",
        "cass doctor archive relocate /absolute/target/cass-archive --json",
        "cass doctor baseline save --json",
        "cass doctor baseline diff <baseline_id> --json",
        "cass doctor baseline update <baseline_id> --json",
        "cass doctor support-bundle --json",
        "cass doctor support-bundle verify <bundle_or_manifest_path> --json",
        "scripts/e2e/doctor_v2.sh list --json",
        "scripts/e2e/doctor_v2.sh describe <scenario_id> --json",
        "scripts/e2e/doctor_v2.sh run <scenario_id> --json --artifact-dir /tmp/cass-doctor-e2e",
    ] {
        assert!(
            runbook.contains(snippet),
            "doctor runbook missing command example: {snippet}"
        );
    }

    for field in [
        "operation_outcome.kind",
        "operation_outcome.exit_code_kind",
        "coverage_risk.status",
        "source_authority.authority_level",
        "raw_mirror.status",
        "remote_source_sync.status",
        "storage_pressure.status",
        "repair_failure_marker.status",
        "artifact_manifest_path",
        "event_log_path",
        "plan_fingerprint",
        "failure_context.json",
    ] {
        assert!(
            runbook.contains(field),
            "doctor runbook missing branchable field or artifact name: {field}"
        );
    }

    for promise in [
        "Do not hand-remove cass data directories",
        "Do not remove lock files by hand",
        "Default bundles are redacted diagnostic handoffs, not backups.",
        "no raw session logs, no full SQLite archive copy, and no private source files",
        "unless the user explicitly opts into sensitive evidence attachment",
        "Any artifact path shown in docs should either come from one",
        "be clearly marked illustrative.",
    ] {
        assert!(
            runbook.contains(promise),
            "doctor runbook missing safety promise: {promise}"
        );
    }

    for forbidden in [
        "rm -rf",
        "git clean",
        "delete the data dir",
        "delete data directories",
        "remove the data dir",
        "hand-remove index directories",
    ] {
        assert!(
            !runbook.to_ascii_lowercase().contains(forbidden),
            "doctor runbook contains dangerous recovery recipe phrase: {forbidden}"
        );
    }
}
