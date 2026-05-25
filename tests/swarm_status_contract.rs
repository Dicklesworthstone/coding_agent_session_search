//! Deterministic fixture and golden checks for the planned `cass swarm status`
//! robot contract.
//!
//! These tests intentionally do not run live Agent Mail, git remotes, rch jobs,
//! cargo, cass indexing, or private session-log reads. They pin the fixture
//! surface that implementation beads can consume once the command exists.
//!
//! ## Regenerate
//!
//! ```bash
//! UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/tmp/cass-swarm-status-golden-target cargo test --test swarm_status_contract
//! git diff -- tests/fixtures/swarm_status tests/golden/swarm_status tests/swarm_status_contract.rs
//! ```

use assert_cmd::Command;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const FIXTURE_ROOT: &str = "tests/fixtures/swarm_status";
const MANIFEST_PATH: &str = "tests/fixtures/swarm_status/manifest.json";
const GOLDEN_UPDATE_COMMAND_SHAPE: &str = "UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/tmp/cass-swarm-status-golden-target cargo test --test swarm_status_contract";
const GOLDEN_REVIEW_COMMAND_SHAPE: &str = "git diff -- tests/fixtures/swarm_status tests/golden/swarm_status tests/swarm_status_contract.rs";
const STRESS_SAMPLE_COUNT: usize = 5;

const REQUIRED_SCENARIOS: &[&str] = &[
    "healthy",
    "busy",
    "stale_advisory",
    "reservation_conflict",
    "unrelated_reservation",
    "build_pressure",
    "no_ready_work",
    "privacy_guardrails",
];

const REQUIRED_TOP_LEVEL_KEYS: &[&str] = &[
    "_meta",
    "agents",
    "beads",
    "build_pressure",
    "cass",
    "evidence",
    "git",
    "privacy",
    "providers",
    "recommendations",
    "reservations",
    "schema_version",
    "status",
    "summary",
];

const REQUIRED_PROVIDER_NAMES: &[&str] = &[
    "agent_mail",
    "beads",
    "cass_health",
    "cass_status",
    "evidence",
    "git",
    "process",
];

#[test]
fn swarm_status_manifest_hashes_are_current() {
    let manifest = read_json(repo_path(MANIFEST_PATH));
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["contract"], "cass.swarm.status.v1");

    for scenario in scenarios(&manifest) {
        let fixture_id = string_field(scenario, "fixture_id");
        let input_path = repo_path(string_field(scenario, "input_path"));
        let golden_path = repo_path(string_field(scenario, "golden_path"));

        assert_eq!(
            sha256_hex(&input_path),
            string_field(scenario, "input_sha256"),
            "{fixture_id} input hash drifted"
        );
        assert_eq!(
            sha256_hex(&golden_path),
            string_field(scenario, "golden_sha256"),
            "{fixture_id} golden hash drifted"
        );

        assert_eq!(
            string_field(scenario, "command_shape"),
            format!(
                "cass swarm status --json --fixture-dir {FIXTURE_ROOT} --fixture-id {fixture_id}"
            ),
            "{fixture_id} command shape should stay robot-safe and fixture-backed"
        );
        assert_eq!(
            string_field(scenario, "stdout_capture_path"),
            string_field(scenario, "golden_path"),
            "{fixture_id} stdout capture should be the reviewed golden"
        );
        assert_eq!(string_field(scenario, "stderr_capture"), "");
        assert!(
            !string_field(scenario, "assertion_summary").is_empty(),
            "{fixture_id} missing assertion summary"
        );

        let redaction = scenario
            .get("redaction_report")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{fixture_id} missing redaction_report"));
        assert_eq!(
            redaction.get("raw_session_content_included"),
            Some(&Value::Bool(false)),
            "{fixture_id} fixtures must not include raw session content"
        );
        assert_eq!(
            redaction.get("mail_body_snippets_included"),
            Some(&Value::Bool(false)),
            "{fixture_id} base fixtures must stay metadata-only for mail"
        );
    }
}

#[test]
fn swarm_status_golden_update_workflow_is_pinned() {
    let manifest = read_json(repo_path(MANIFEST_PATH));
    let update_workflow = manifest
        .get("golden_update_workflow")
        .and_then(Value::as_object)
        .expect("manifest golden_update_workflow must be an object");

    assert_eq!(
        update_workflow.get("command_shape").and_then(Value::as_str),
        Some(GOLDEN_UPDATE_COMMAND_SHAPE),
        "golden update workflow must require UPDATE_GOLDENS=1 and rch"
    );
    assert_eq!(
        update_workflow
            .get("review_command")
            .and_then(Value::as_str),
        Some(GOLDEN_REVIEW_COMMAND_SHAPE),
        "golden update workflow must require explicit diff review"
    );
    assert_eq!(
        update_workflow.get("review_required"),
        Some(&Value::Bool(true)),
        "golden updates require human review before commit"
    );
    assert_eq!(
        update_workflow.get("uses_live_services"),
        Some(&Value::Bool(false)),
        "golden updates must stay fixture-only"
    );
}

#[test]
fn swarm_status_fixture_set_covers_required_scenarios() {
    let manifest = read_json(repo_path(MANIFEST_PATH));
    let actual: BTreeSet<&str> = scenarios(&manifest)
        .iter()
        .map(|scenario| string_field(scenario, "fixture_id"))
        .collect();
    let expected: BTreeSet<&str> = REQUIRED_SCENARIOS.iter().copied().collect();
    assert_eq!(actual, expected);
}

#[test]
fn swarm_status_goldens_follow_contract_shape() {
    let manifest = read_json(repo_path(MANIFEST_PATH));
    for scenario in scenarios(&manifest) {
        let fixture_id = string_field(scenario, "fixture_id");
        let input = read_json(repo_path(string_field(scenario, "input_path")));
        let output = read_json(repo_path(string_field(scenario, "golden_path")));

        assert_eq!(input["fixture_id"], fixture_id);
        assert_eq!(output["schema_version"], "cass.swarm.status.v1");
        assert!(
            matches!(output["status"].as_str(), Some("ok" | "partial")),
            "{fixture_id} status must be ok or partial"
        );

        for key in REQUIRED_TOP_LEVEL_KEYS {
            assert!(
                output.get(key).is_some(),
                "{fixture_id} missing top-level key {key}"
            );
        }

        let provider_names: BTreeSet<&str> = output["providers"]
            .as_array()
            .unwrap_or_else(|| panic!("{fixture_id} providers must be an array"))
            .iter()
            .map(|provider| string_field(provider, "name"))
            .collect();
        for provider in REQUIRED_PROVIDER_NAMES {
            assert!(
                provider_names.contains(provider),
                "{fixture_id} missing provider {provider}"
            );
        }
        assert!(
            provider_names.contains("evidence"),
            "{fixture_id} exposes top-level evidence without evidence provider status"
        );

        assert_eq!(
            output["privacy"]["raw_session_content_included"],
            Value::Bool(false),
            "{fixture_id} must not include raw session content"
        );
        assert_eq!(
            output["privacy"]["redaction_policy"], "strict",
            "{fixture_id} must default to strict redaction"
        );
        assert!(
            output["recommendations"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "{fixture_id} should include at least one branchable recommendation"
        );

        assert_no_forbidden_fixture_leaks(fixture_id, &output);
    }
}

#[test]
fn swarm_status_scenario_invariants_are_pinned() {
    let manifest = read_json(repo_path(MANIFEST_PATH));
    for scenario in scenarios(&manifest) {
        let fixture_id = string_field(scenario, "fixture_id");
        let output = read_json(repo_path(string_field(scenario, "golden_path")));

        match fixture_id {
            "healthy" => {
                assert_eq!(output["summary"]["ready_count"], 1);
                assert_eq!(output["summary"]["build_pressure"], "none");
                assert_eq!(output["recommendations"][0]["kind"], "claim-ready-bead");
            }
            "busy" => {
                assert_eq!(output["summary"]["active_agent_count"], 2);
                assert_eq!(output["summary"]["active_reservation_count"], 1);
                assert_eq!(output["summary"]["dirty_worktree"], true);
                assert_eq!(output["reservations"][0]["state"], "active");
                assert_eq!(output["summary"]["stale_state_counts"]["active"], 1);
                assert_eq!(output["beads"]["in_progress"][0]["stale_state"], "active");
                assert_eq!(output["summary"]["recommended_action"], "claim-ready-bead");
                assert_eq!(output["recommendations"][0]["kind"], "claim-ready-bead");
            }
            "stale_advisory" => {
                assert_eq!(output["summary"]["stale_candidate_count"], 1);
                assert_eq!(output["summary"]["stale_state_counts"]["likely_stale"], 1);
                assert_eq!(output["summary"]["stale_state_counts"]["recently_quiet"], 1);
                assert_eq!(
                    output["summary"]["stale_state_counts"]["conflicting_evidence"],
                    1
                );
                assert_eq!(
                    output["summary"]["stale_state_counts"]["manual_review_required"],
                    1
                );
                assert_eq!(
                    output["beads"]["stale_candidates"][0]["stale_state"],
                    "likely_stale"
                );
                assert_eq!(
                    output["beads"]["stale_candidates"][0]["takeover_advice"],
                    "inspect-only-use-agent-mail-stale-heuristics-before-reopen"
                );
                assert_eq!(
                    output["beads"]["in_progress"][1]["stale_state"],
                    "recently_quiet"
                );
                assert_eq!(
                    output["beads"]["in_progress"][2]["stale_state"],
                    "conflicting_evidence"
                );
                assert_eq!(
                    output["beads"]["in_progress"][3]["takeover_advice"],
                    "clock-skew-inspect-only"
                );
                assert_eq!(
                    output["recommendations"][0]["requires_human_confirmation"],
                    true
                );
                assert_eq!(
                    output["recommendations"][0]["commands"][0],
                    "br show cass-stale-1 --json"
                );
                assert_eq!(
                    output["recommendations"][0]["commands"][1],
                    "cass swarm status --json"
                );
            }
            "reservation_conflict" => {
                assert_eq!(output["beads"]["ready"][0]["safe_to_claim"], false);
                assert_eq!(output["reservations"][0]["state"], "conflicting");
                assert_eq!(output["recommendations"][0]["kind"], "coordinate");
            }
            "unrelated_reservation" => {
                assert_eq!(output["beads"]["ready"][0]["safe_to_claim"], true);
                assert_eq!(output["reservations"][0]["state"], "active");
                assert_eq!(output["reservations"][0]["overlaps_dirty_worktree"], false);
                assert_eq!(output["summary"]["recommended_action"], "claim-ready-bead");
                assert_eq!(output["recommendations"][0]["kind"], "claim-ready-bead");
            }
            "build_pressure" => {
                assert_eq!(output["summary"]["build_pressure"], "high");
                assert_eq!(output["build_pressure"]["active_rch_jobs"], 9);
                assert_eq!(output["build_pressure"]["active_cargo_jobs"], 1);
                assert_eq!(
                    output["recommendations"][0]["kind"],
                    "reduce-build-pressure"
                );
            }
            "no_ready_work" => {
                assert_eq!(output["summary"]["ready_count"], 0);
                assert_eq!(output["summary"]["recommended_action"], "no-ready-work");
                assert_eq!(output["recommendations"][0]["kind"], "no-ready-work");
            }
            "privacy_guardrails" => {
                assert_eq!(output["privacy"]["redaction_applied"], true);
                assert_eq!(output["privacy"]["sensitive_paths_scrubbed"], 4);
                assert_eq!(output["privacy"]["command_arguments_scrubbed"], 2);
                assert_eq!(output["privacy"]["env_values_scrubbed"], 1);
                assert_eq!(output["privacy"]["mailbox_snippets_omitted"], 1);
                assert_eq!(output["privacy"]["evidence_references_scrubbed"], 1);
                assert_eq!(
                    output["privacy"]["opt_in_boundary"],
                    "mail body snippets require --include-evidence; raw session content is unsupported in cass.swarm.status.v1"
                );
                assert_eq!(
                    output["evidence"]["recent_threads"][0]["body_snippet"],
                    "[MAIL_BODY_OMITTED]"
                );
                assert_eq!(
                    output["evidence"]["recent_proofs"][0]["redaction_status"],
                    "redacted"
                );
            }
            other => panic!("unexpected scenario {other}"),
        }
    }
}

#[test]
fn swarm_evidence_cli_links_committed_bead_to_proof_and_mail() -> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "evidence-linked",
        json!({
            "beads": {
                "closed": [{
                    "id": "cass-proof-1",
                    "title": "Proof-backed closeout",
                    "status": "closed",
                    "close_reason": "Verified by rch",
                    "commit_id": "abc123"
                }]
            },
            "agent_mail": {
                "messages": [{
                    "thread_id": "cass-proof-1",
                    "subject": "Closeout proof",
                    "from": "FixtureAgent",
                    "created_ts": "2026-05-08T16:00:00Z"
                }],
                "reservations": [{
                    "reason": "cass-proof-1",
                    "holder": "FixtureAgent",
                    "path_pattern": "src/lib.rs",
                    "exclusive": true,
                    "expires_ts": "2026-05-08T17:00:00Z"
                }]
            },
            "git": {
                "dirty": false,
                "dirty_paths": [],
                "recent_commits": [{
                    "hash": "abc123",
                    "subject": "feat: finish cass-proof-1",
                    "authored_ts": "2026-05-08T15:55:00Z",
                    "changed_paths": ["src/lib.rs", "tests/cli_robot.rs"]
                }]
            },
            "evidence": {
                "recent_threads": [{
                    "thread_id": "cass-proof-1",
                    "subject": "Closeout proof",
                    "sender": "FixtureAgent",
                    "created_ts": "2026-05-08T16:00:00Z"
                }],
                "recent_proofs": [{
                    "kind": "rch-test",
                    "bead_id": "cass-proof-1",
                    "commit_id": "abc123",
                    "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-proof cargo test --test cli_robot",
                    "status": "passed",
                    "remote_exit_status": 0,
                    "changed_paths": ["src/lib.rs", "tests/cli_robot.rs"],
                    "mail_thread_refs": ["cass-proof-1"]
                }],
                "proof_gaps": [],
                "redaction_applied": false
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {}
        }),
    )?;
    let output = run_swarm_evidence_fixture(&fixture_path, Some("cass-proof-1"));

    let output = output?;
    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.evidence.v1"),
        "schema version",
    )?;
    require_value_eq(get_path(&output, &["status"]), json!("ok"), "status")?;
    require_value_eq(
        get_path(&output, &["filter", "bead_id"]),
        json!("cass-proof-1"),
        "bead filter",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "bead_count"]),
        json!(1),
        "bead count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "commit_count"]),
        json!(1),
        "commit count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "proof_count"]),
        json!(1),
        "proof count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "mail_thread_count"]),
        json!(1),
        "mail thread count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "reservation_count"]),
        json!(1),
        "reservation count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "proof_gap_count"]),
        json!(0),
        "proof gap count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("proof-ledger-complete"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["privacy", "raw_session_content_included"]),
        json!(false),
        "raw session privacy flag",
    )?;
    require_value_eq(
        get_path(&output, &["privacy", "mail_body_snippets_included"]),
        json!(false),
        "mail snippet privacy flag",
    )?;

    let ledger = get_path(&output, &["ledger"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("ledger array missing"))?;
    require(
        ledger.iter().any(|row| {
            row.get("kind").and_then(Value::as_str) == Some("bead")
                && row.get("bead_id").and_then(Value::as_str) == Some("cass-proof-1")
                && row.get("status").and_then(Value::as_str) == Some("closed")
        }),
        "missing bead ledger row",
    )?;
    require(
        ledger.iter().any(|row| {
            row.get("kind").and_then(Value::as_str) == Some("commit")
                && row.get("commit_id").and_then(Value::as_str) == Some("abc123")
                && row
                    .get("bead_ids")
                    .and_then(Value::as_array)
                    .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some("cass-proof-1")))
        }),
        "missing commit ledger row",
    )?;
    require(
        ledger.iter().any(|row| {
            row.get("kind").and_then(Value::as_str) == Some("proof")
                && row.get("proof_kind").and_then(Value::as_str) == Some("rch-test")
                && row.get("remote_exit_status").and_then(Value::as_i64) == Some(0)
        }),
        "missing proof ledger row",
    )?;
    require(
        ledger.iter().any(|row| {
            row.get("kind").and_then(Value::as_str) == Some("mail_thread")
                && row.get("thread_id").and_then(Value::as_str) == Some("cass-proof-1")
        }),
        "missing mail thread ledger row",
    )?;
    require(
        ledger.iter().any(|row| {
            row.get("kind").and_then(Value::as_str) == Some("reservation")
                && row.get("bead_id").and_then(Value::as_str) == Some("cass-proof-1")
                && row.get("path_pattern").and_then(Value::as_str) == Some("src/lib.rs")
        }),
        "missing reservation ledger row",
    )?;
    assert_no_forbidden_fixture_leaks("evidence-linked", &output);
    Ok(())
}

#[test]
fn swarm_evidence_cli_surfaces_missing_conflicting_interrupted_and_unrelated_gaps()
-> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "evidence-gaps",
        json!({
            "beads": {
                "closed": [
                    {"id": "cass-missing", "status": "closed"},
                    {"id": "cass-conflict", "status": "closed"},
                    {"id": "cass-interrupted", "status": "closed"}
                ]
            },
            "agent_mail": {
                "messages": [],
                "reservations": []
            },
            "git": {
                "dirty": true,
                "dirty_paths": [{"path": "docs/unrelated.md"}],
                "recent_commits": [
                    {
                        "hash": "aaa111",
                        "subject": "finish cass-missing",
                        "changed_paths": ["src/missing.rs"]
                    },
                    {
                        "hash": "bbb222",
                        "subject": "finish cass-conflict",
                        "changed_paths": ["src/conflict.rs"]
                    },
                    {
                        "hash": "ccc333",
                        "subject": "finish cass-interrupted",
                        "changed_paths": ["src/interrupted.rs"]
                    }
                ]
            },
            "evidence": {
                "recent_proofs": [
                    {
                        "kind": "rch-test",
                        "bead_id": "cass-conflict",
                        "commit_id": "bbb222",
                        "status": "failed",
                        "remote_exit_status": 0,
                        "changed_paths": ["src/conflict.rs"]
                    },
                    {
                        "kind": "rch-test",
                        "bead_id": "cass-interrupted",
                        "commit_id": "ccc333",
                        "status": "passed",
                        "remote_exit_status": 0,
                        "artifact_retrieval": "interrupted",
                        "changed_paths": ["src/interrupted.rs"]
                    }
                ],
                "proof_gaps": [],
                "redaction_applied": false
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {}
        }),
    )?;
    let output = run_swarm_evidence_fixture(&fixture_path, None)?;
    let gap_kinds = get_path(&output, &["proof_gaps"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("proof gaps missing"))?
        .iter()
        .filter_map(|gap| gap.get("kind").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.evidence.v1"),
        "schema version",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("inspect-proof-gaps"),
        "recommended action",
    )?;
    require(gap_kinds.contains("missing-proof"), "missing proof gap")?;
    require(
        gap_kinds.contains("missing-rch-proof"),
        "missing rch proof gap",
    )?;
    require(
        gap_kinds.contains("conflicting-proof"),
        "missing conflicting proof gap",
    )?;
    require(
        gap_kinds.contains("artifact-retrieval-interrupted-after-success"),
        "missing interrupted retrieval gap",
    )?;
    require(
        gap_kinds.contains("unrelated-dirty-file"),
        "missing unrelated dirty file gap",
    )?;
    assert_no_forbidden_fixture_leaks("evidence-gaps", &output);
    Ok(())
}

#[test]
fn swarm_status_large_fixture_fast_gate_names_budget_sections() -> Result<(), Box<dyn Error>> {
    let scale = SyntheticSwarmScale {
        ready_count: 850,
        in_progress_count: 100,
        blocked_count: 50,
        reservation_count: 50,
        commit_count: 20,
        agent_count: 15,
        active_rch_jobs: 6,
        active_cargo_jobs: 0,
        cpu_count: 64,
    };
    let (_tmp, fixture_path) = write_synthetic_swarm_status_fixture("large-fast", scale)?;
    let (output, sample) = measure_swarm_status_fixture(&fixture_path)?;

    require_duration_at_most(
        "fixture parse",
        sample.fixture_parse,
        Duration::from_secs(1),
    )?;
    require_duration_at_most(
        "provider collect",
        sample.provider_collect,
        Duration::from_secs(1),
    )?;
    require_duration_at_most(
        "swarm status CLI wall",
        sample.cli_wall,
        Duration::from_secs(5),
    )?;
    require_duration_at_most(
        "output JSON accounting",
        sample.output_json_accounting,
        Duration::from_millis(250),
    )?;
    require_at_most(
        "swarm status output bytes",
        sample.output_bytes,
        5 * 1024 * 1024,
    )?;

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.status.v1"),
        "schema version",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "ready_count"]),
        json!(850),
        "ready count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "in_progress_count"]),
        json!(100),
        "in-progress count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "blocked_count"]),
        json!(50),
        "blocked count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "active_agent_count"]),
        json!(15),
        "active agent count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "active_reservation_count"]),
        json!(50),
        "active reservation count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "build_pressure"]),
        json!("light"),
        "build pressure",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("claim-ready-bead"),
        "recommended action",
    )?;
    assert_no_forbidden_fixture_leaks("large-fast", &output);
    Ok(())
}

#[test]
#[ignore = "operator stress proof: run explicitly with rch; writes a target/ artifact summary"]
fn swarm_status_large_fixture_stress_artifact_10k() -> Result<(), Box<dyn Error>> {
    let scale = SyntheticSwarmScale {
        ready_count: 8_000,
        in_progress_count: 1_500,
        blocked_count: 500,
        reservation_count: 500,
        commit_count: 200,
        agent_count: 100,
        active_rch_jobs: 32,
        active_cargo_jobs: 0,
        cpu_count: 64,
    };
    let (_tmp, fixture_path) = write_synthetic_swarm_status_fixture("large-stress-10k", scale)?;

    let mut samples = Vec::with_capacity(STRESS_SAMPLE_COUNT);
    let mut latest_output = None;
    for _ in 0..STRESS_SAMPLE_COUNT {
        let (output, sample) = measure_swarm_status_fixture(&fixture_path)?;
        latest_output = Some(output);
        samples.push(sample);
    }
    let output = latest_output.ok_or_else(|| test_error("stress test produced no samples"))?;
    let summary = summarize_swarm_status_samples(&samples)?;

    require_value_eq(
        get_path(&output, &["summary", "ready_count"]),
        json!(8_000),
        "stress ready count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "active_reservation_count"]),
        json!(500),
        "stress active reservation count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "build_pressure"]),
        json!("high"),
        "stress build pressure",
    )?;

    let artifact = json!({
        "schema_version": "cass.swarm.status.large_swarm_perf.v1",
        "fixture_id": "large-stress-10k",
        "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-swarm-large-target cargo test --test swarm_status_contract swarm_status_large_fixture_stress_artifact_10k -- --ignored --nocapture",
        "sample_count": samples.len(),
        "scale": {
            "beads": scale.total_bead_count(),
            "reservations": scale.reservation_count,
            "recent_commits": scale.commit_count,
            "agents": scale.agent_count,
            "active_rch_jobs": scale.active_rch_jobs,
            "cpu_count": scale.cpu_count
        },
        "measurements": {
            "p50": {
                "fixture_parse_ms": duration_ms(summary.fixture_parse_p50),
                "provider_collect_ms": duration_ms(summary.provider_collect_p50),
                "cli_wall_ms": duration_ms(summary.cli_wall_p50),
                "output_json_accounting_ms": duration_ms(summary.output_json_accounting_p50),
                "output_bytes": summary.output_bytes_p50
            },
            "p95": {
                "fixture_parse_ms": duration_ms(summary.fixture_parse_p95),
                "provider_collect_ms": duration_ms(summary.provider_collect_p95),
                "cli_wall_ms": duration_ms(summary.cli_wall_p95),
                "output_json_accounting_ms": duration_ms(summary.output_json_accounting_p95),
                "output_bytes": summary.output_bytes_p95
            },
            "max": {
                "output_bytes": summary.output_bytes_max
            },
            "raw_samples": samples.iter().map(sample_json).collect::<Vec<_>>()
        },
        "memory": {
            "harness_peak_rss_kb": process_peak_rss_kb(),
            "note": "VmHWM for the ignored test process when /proc is available"
        },
        "section_budgets": {
            "fixture_parse_ms": 2_000,
            "provider_collect_ms": 2_000,
            "cli_wall_ms": 20_000,
            "output_json_accounting_ms": 1_000,
            "output_bytes": 25 * 1024 * 1024
        }
    });
    let artifact_path = write_swarm_perf_artifact(&artifact)?;

    require_duration_at_most(
        "stress fixture parse p95",
        summary.fixture_parse_p95,
        Duration::from_secs(2),
    )?;
    require_duration_at_most(
        "stress provider collect p95",
        summary.provider_collect_p95,
        Duration::from_secs(2),
    )?;
    require_duration_at_most(
        "stress swarm status CLI wall p95",
        summary.cli_wall_p95,
        Duration::from_secs(20),
    )?;
    require_duration_at_most(
        "stress output JSON accounting p95",
        summary.output_json_accounting_p95,
        Duration::from_secs(1),
    )?;
    require_at_most(
        "stress swarm status output bytes",
        summary.output_bytes_max,
        25 * 1024 * 1024,
    )?;
    require(
        artifact_path.is_file(),
        format!(
            "stress artifact was not written: {}",
            artifact_path.display()
        ),
    )?;
    Ok(())
}

fn repo_path(relative: impl AsRef<Path>) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[derive(Clone, Copy)]
struct SyntheticSwarmScale {
    ready_count: usize,
    in_progress_count: usize,
    blocked_count: usize,
    reservation_count: usize,
    commit_count: usize,
    agent_count: usize,
    active_rch_jobs: usize,
    active_cargo_jobs: usize,
    cpu_count: usize,
}

#[derive(Clone, Copy)]
struct SwarmStatusPerfSample {
    fixture_parse: Duration,
    provider_collect: Duration,
    cli_wall: Duration,
    output_json_accounting: Duration,
    output_bytes: usize,
}

struct SwarmStatusPerfSummary {
    fixture_parse_p50: Duration,
    fixture_parse_p95: Duration,
    provider_collect_p50: Duration,
    provider_collect_p95: Duration,
    cli_wall_p50: Duration,
    cli_wall_p95: Duration,
    output_json_accounting_p50: Duration,
    output_json_accounting_p95: Duration,
    output_bytes_p50: usize,
    output_bytes_p95: usize,
    output_bytes_max: usize,
}

impl SyntheticSwarmScale {
    fn total_bead_count(self) -> usize {
        self.ready_count + self.in_progress_count + self.blocked_count
    }
}

fn write_synthetic_swarm_status_fixture(
    fixture_id: &str,
    scale: SyntheticSwarmScale,
) -> Result<(TempDir, PathBuf), Box<dyn Error>> {
    let tmp = TempDir::new()?;
    let fixture_path = tmp.path().join(format!("{fixture_id}.inputs.json"));
    fs::write(
        &fixture_path,
        serde_json::to_vec(&synthetic_swarm_status_fixture(fixture_id, scale)?)?,
    )?;
    Ok((tmp, fixture_path))
}

fn synthetic_swarm_status_fixture(
    fixture_id: &str,
    scale: SyntheticSwarmScale,
) -> Result<Value, Box<dyn Error>> {
    require(
        scale.reservation_count <= scale.ready_count,
        "synthetic reservations must fit within ready beads",
    )?;
    require(
        scale.commit_count <= scale.ready_count,
        "synthetic commits must fit within ready beads",
    )?;

    let ready = (0..scale.ready_count)
        .map(|idx| {
            json!({
                "id": format!("cass-large-ready-{idx:05}"),
                "title": format!("Large ready fixture bead {idx}"),
                "status": "open",
                "priority": if idx % 5 == 0 { 1 } else { 2 },
                "issue_type": "test",
                "labels": ["swarm", "testing", "performance"],
                "updated_at": "2026-05-08T15:55:00Z"
            })
        })
        .collect::<Vec<_>>();
    let in_progress = (0..scale.in_progress_count)
        .map(|idx| {
            json!({
                "id": format!("cass-large-active-{idx:05}"),
                "title": format!("Large active fixture bead {idx}"),
                "status": "in_progress",
                "priority": 2,
                "issue_type": "task",
                "labels": ["swarm"],
                "updated_at": "2026-05-08T15:58:00Z"
            })
        })
        .collect::<Vec<_>>();
    let blocked = (0..scale.blocked_count)
        .map(|idx| {
            json!({
                "id": format!("cass-large-blocked-{idx:05}"),
                "title": format!("Large blocked fixture bead {idx}"),
                "status": "blocked",
                "priority": 3,
                "issue_type": "task",
                "labels": ["swarm"],
                "updated_at": "2026-05-08T15:40:00Z"
            })
        })
        .collect::<Vec<_>>();
    let agents = (0..scale.agent_count)
        .map(|idx| {
            json!({
                "name": format!("FixtureAgent{idx:03}"),
                "program": "codex-cli",
                "model": "gpt-5",
                "task_description": "Synthetic large-swarm load fixture",
                "last_active_ts": "2026-05-08T15:59:30Z"
            })
        })
        .collect::<Vec<_>>();
    let messages = (0..scale.agent_count)
        .map(|idx| {
            json!({
                "thread_id": format!("cass-large-ready-{idx:05}"),
                "subject": format!("Working cass-large-ready-{idx:05}"),
                "from": format!("FixtureAgent{idx:03}"),
                "created_ts": "2026-05-08T15:59:45Z"
            })
        })
        .collect::<Vec<_>>();
    let reservations = (0..scale.reservation_count)
        .map(|idx| {
            json!({
                "reason": format!("cass-large-ready-{idx:05}"),
                "holder": format!("FixtureAgent{:03}", idx % scale.agent_count.max(1)),
                "path_pattern": format!("src/generated/large/{idx:05}/**"),
                "exclusive": true,
                "expires_ts": "2026-05-08T17:00:00Z"
            })
        })
        .collect::<Vec<_>>();
    let recent_commits = (0..scale.commit_count)
        .map(|idx| {
            json!({
                "hash": format!("abc{idx:05}"),
                "subject": format!("test: prove cass-large-ready-{idx:05}"),
                "authored_ts": "2026-05-08T15:50:00Z",
                "changed_paths": [format!("tests/generated/large_{idx:05}.rs")]
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "fixture_id": fixture_id,
        "description": "Generated large-swarm fixture for aggregation and resource proof gates.",
        "sources": {
            "beads": {
                "ready": ready,
                "in_progress": in_progress,
                "blocked": blocked,
                "graph": {
                    "node_count": scale.total_bead_count(),
                    "edge_count": scale.total_bead_count().saturating_sub(1),
                    "has_cycles": false
                }
            },
            "agent_mail": {
                "agents": agents,
                "messages": messages,
                "reservations": reservations
            },
            "git": {
                "branch": "main",
                "upstream": "origin/main",
                "ahead": 0,
                "behind": 0,
                "dirty": false,
                "dirty_paths": [],
                "recent_commits": recent_commits
            },
            "processes": {
                "active_rch_jobs": scale.active_rch_jobs,
                "active_cargo_jobs": scale.active_cargo_jobs,
                "load_average_1m": (scale.active_rch_jobs as f64) * 1.5,
                "cpu_count": scale.cpu_count
            },
            "cass_health": {
                "status": "healthy",
                "healthy": true,
                "initialized": true,
                "recommended_action": null
            },
            "cass_status": {
                "search_ready": true,
                "semantic_fallback_mode": "lexical",
                "active_rebuild": false
            },
            "evidence": {
                "recent_threads": [],
                "recent_proofs": [],
                "proof_gaps": [],
                "redaction_applied": false
            }
        }
    }))
}

fn write_swarm_evidence_fixture(
    fixture_id: &str,
    sources: Value,
) -> Result<(TempDir, PathBuf), Box<dyn Error>> {
    let tmp = TempDir::new()?;
    let fixture_path = tmp.path().join(format!("{fixture_id}.inputs.json"));
    let fixture = json!({
        "fixture_id": fixture_id,
        "description": "Temporary swarm evidence fixture for CLI contract coverage.",
        "sources": sources
    });
    fs::write(&fixture_path, serde_json::to_vec_pretty(&fixture)?)?;
    Ok((tmp, fixture_path))
}

fn run_swarm_status_fixture(fixture_path: &Path) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore — fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "status", "--json", "--fixture"]);
    cmd.arg(fixture_path);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm status should not log to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn measure_swarm_status_fixture(
    fixture_path: &Path,
) -> Result<(Value, SwarmStatusPerfSample), Box<dyn Error>> {
    let parse_start = Instant::now();
    let adapter_set =
        coding_agent_search::swarm_status::FixtureSwarmAdapterSet::from_fixture_path(fixture_path)?;
    let fixture_parse = parse_start.elapsed();

    let collect_start = Instant::now();
    let collection = adapter_set.collect_required();
    let provider_collect = collect_start.elapsed();
    require(
        !collection.partial(),
        "provider collect returned partial data",
    )?;

    let cli_start = Instant::now();
    let output = run_swarm_status_fixture(fixture_path)?;
    let cli_wall = cli_start.elapsed();

    let json_start = Instant::now();
    let output_bytes = serde_json::to_vec(&output)?.len();
    let output_json_accounting = json_start.elapsed();

    Ok((
        output,
        SwarmStatusPerfSample {
            fixture_parse,
            provider_collect,
            cli_wall,
            output_json_accounting,
            output_bytes,
        },
    ))
}

fn summarize_swarm_status_samples(
    samples: &[SwarmStatusPerfSample],
) -> Result<SwarmStatusPerfSummary, Box<dyn Error>> {
    require(!samples.is_empty(), "cannot summarize empty samples")?;

    Ok(SwarmStatusPerfSummary {
        fixture_parse_p50: percentile_duration(samples, |sample| sample.fixture_parse, 0.50),
        fixture_parse_p95: percentile_duration(samples, |sample| sample.fixture_parse, 0.95),
        provider_collect_p50: percentile_duration(samples, |sample| sample.provider_collect, 0.50),
        provider_collect_p95: percentile_duration(samples, |sample| sample.provider_collect, 0.95),
        cli_wall_p50: percentile_duration(samples, |sample| sample.cli_wall, 0.50),
        cli_wall_p95: percentile_duration(samples, |sample| sample.cli_wall, 0.95),
        output_json_accounting_p50: percentile_duration(
            samples,
            |sample| sample.output_json_accounting,
            0.50,
        ),
        output_json_accounting_p95: percentile_duration(
            samples,
            |sample| sample.output_json_accounting,
            0.95,
        ),
        output_bytes_p50: percentile_usize(samples, |sample| sample.output_bytes, 0.50),
        output_bytes_p95: percentile_usize(samples, |sample| sample.output_bytes, 0.95),
        output_bytes_max: samples
            .iter()
            .map(|sample| sample.output_bytes)
            .fold(0, usize::max),
    })
}

fn percentile_duration(
    samples: &[SwarmStatusPerfSample],
    select: impl Fn(&SwarmStatusPerfSample) -> Duration,
    percentile: f64,
) -> Duration {
    let mut values = samples.iter().map(select).collect::<Vec<_>>();
    values.sort();
    values
        .get(percentile_index(values.len(), percentile))
        .copied()
        .unwrap_or(Duration::ZERO)
}

fn percentile_usize(
    samples: &[SwarmStatusPerfSample],
    select: impl Fn(&SwarmStatusPerfSample) -> usize,
    percentile: f64,
) -> usize {
    let mut values = samples.iter().map(select).collect::<Vec<_>>();
    values.sort_unstable();
    values
        .get(percentile_index(values.len(), percentile))
        .copied()
        .unwrap_or(0)
}

fn percentile_index(len: usize, percentile: f64) -> usize {
    if len <= 1 {
        return 0;
    }
    let percentile = percentile.clamp(0.0, 1.0);
    ((((len - 1) as f64) * percentile).ceil() as usize).min(len - 1)
}

fn sample_json(sample: &SwarmStatusPerfSample) -> Value {
    json!({
        "fixture_parse_ms": duration_ms(sample.fixture_parse),
        "provider_collect_ms": duration_ms(sample.provider_collect),
        "cli_wall_ms": duration_ms(sample.cli_wall),
        "output_json_accounting_ms": duration_ms(sample.output_json_accounting),
        "output_bytes": sample.output_bytes
    })
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn run_swarm_evidence_fixture(
    fixture_path: &Path,
    bead: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore — fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "evidence", "--json", "--fixture"]);
    cmd.arg(fixture_path);
    if let Some(bead_id) = bead {
        cmd.args(["--bead", bead_id]);
    }

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm evidence should not log to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn read_json(path: PathBuf) -> Value {
    let body =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    serde_json::from_str(&body).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()))
}

fn scenarios(manifest: &Value) -> Vec<&Value> {
    manifest["scenarios"]
        .as_array()
        .expect("manifest scenarios must be an array")
        .iter()
        .collect()
}

fn string_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field {field} in {value:#}"))
}

fn sha256_hex(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn assert_no_forbidden_fixture_leaks(fixture_id: &str, value: &Value) {
    let text = serde_json::to_string(value).expect("serialize output");
    for needle in [
        "/home/",
        "BEGIN PRIVATE",
        "PRIVATE KEY",
        "SECRET_VALUE",
        "TOKEN=",
        "raw_session_text",
        "/Users/",
        "alice@example.com",
        "api.example.corp",
        "PRIVATE_SESSION_DO_NOT_LEAK",
    ] {
        assert!(
            !text.contains(needle),
            "{fixture_id} golden leaks forbidden fixture text: {needle}"
        );
    }
}

fn get_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn require_value_eq(
    actual: Option<&Value>,
    expected: Value,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    match actual {
        Some(actual) if actual == &expected => Ok(()),
        Some(actual) => Err(test_error(format!(
            "{label} mismatch: expected {expected}, got {actual}"
        ))),
        None => Err(test_error(format!("{label} missing"))),
    }
}

fn require(condition: bool, message: impl Into<String>) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(test_error(message))
    }
}

fn require_duration_at_most(
    stage: &str,
    actual: Duration,
    max: Duration,
) -> Result<(), Box<dyn Error>> {
    if actual <= max {
        Ok(())
    } else {
        Err(test_error(format!(
            "{stage} exceeded budget: actual_ms={:.3}, budget_ms={:.3}",
            actual.as_secs_f64() * 1000.0,
            max.as_secs_f64() * 1000.0
        )))
    }
}

fn require_at_most(stage: &str, actual: usize, max: usize) -> Result<(), Box<dyn Error>> {
    if actual <= max {
        Ok(())
    } else {
        Err(test_error(format!(
            "{stage} exceeded budget: actual={actual}, budget={max}"
        )))
    }
}

fn write_swarm_perf_artifact(artifact: &Value) -> Result<PathBuf, Box<dyn Error>> {
    let dir = repo_path("target/cass-swarm-status-perf");
    fs::create_dir_all(&dir)?;
    let path = dir.join("large-stress-10k-summary.json");
    fs::write(&path, serde_json::to_vec_pretty(artifact)?)?;
    Ok(path)
}

fn process_peak_rss_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("VmHWM:")?.trim();
        value.split_whitespace().next()?.parse().ok()
    })
}

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}
