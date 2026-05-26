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
fn swarm_cockpit_integrated_gate_composes_operator_surfaces() -> Result<(), Box<dyn Error>> {
    let manifest = read_json(repo_path(MANIFEST_PATH));

    assert_swarm_status_action_matrix(&manifest)?;
    assert_swarm_work_packets_follow_status(&manifest)?;
    assert_swarm_stale_busy_and_dirty_signals(&manifest)?;
    assert_swarm_proof_lint_and_pattern_surfaces_compose()?;

    Ok(())
}

fn assert_swarm_status_action_matrix(manifest: &Value) -> Result<(), Box<dyn Error>> {
    for (fixture_id, expected_action) in [
        ("healthy", "claim-ready-bead"),
        ("busy", "claim-ready-bead"),
        ("stale_advisory", "inspect-stale"),
        ("reservation_conflict", "coordinate-before-claim"),
        ("build_pressure", "reduce-build-pressure"),
        ("no_ready_work", "no-ready-work"),
        ("privacy_guardrails", "review-redaction-report"),
    ] {
        let output = read_json(manifest_golden_path(manifest, fixture_id)?);
        require_value_eq(
            get_path(&output, &["summary", "recommended_action"]),
            json!(expected_action),
            "status recommended action",
        )?;
        require_value_eq(
            get_path(&output, &["privacy", "raw_session_content_included"]),
            json!(false),
            "raw session privacy flag",
        )?;
        assert_no_forbidden_fixture_leaks(fixture_id, &output);
    }
    Ok(())
}

fn assert_swarm_work_packets_follow_status(manifest: &Value) -> Result<(), Box<dyn Error>> {
    for case in [
        WorkPacketCase {
            fixture_id: "healthy",
            expected_status_action: "claim-ready-bead",
            expected_packet_action: "claim-ready-bead",
            expected_readiness: "ready",
            safe_to_start: true,
        },
        WorkPacketCase {
            fixture_id: "reservation_conflict",
            expected_status_action: "coordinate-before-claim",
            expected_packet_action: "coordinate-before-claim",
            expected_readiness: "blocked",
            safe_to_start: false,
        },
        WorkPacketCase {
            fixture_id: "build_pressure",
            expected_status_action: "reduce-build-pressure",
            expected_packet_action: "wait-for-rch-capacity",
            expected_readiness: "build-pressure-high",
            safe_to_start: false,
        },
    ] {
        let fixture_path = manifest_input_path(manifest, case.fixture_id)?;
        let status = run_swarm_status_fixture(&fixture_path)?;
        let packet = run_swarm_work_packet_fixture(&fixture_path, None)?;

        require_value_eq(
            get_path(&status, &["summary", "recommended_action"]),
            json!(case.expected_status_action),
            &format!("{} status action", case.fixture_id),
        )?;
        require_value_eq(
            get_path(&packet, &["summary", "recommended_action"]),
            json!(case.expected_packet_action),
            &format!("{} packet action", case.fixture_id),
        )?;
        require_value_eq(
            get_path(&packet, &["summary", "readiness_state"]),
            json!(case.expected_readiness),
            &format!("{} packet readiness", case.fixture_id),
        )?;
        require_value_eq(
            get_path(&packet, &["summary", "safe_to_start"]),
            json!(case.safe_to_start),
            &format!("{} packet safety", case.fixture_id),
        )?;
        require_value_eq(
            get_path(&packet, &["source_status", "summary", "recommended_action"]),
            json!(case.expected_status_action),
            &format!("{} packet source-status action", case.fixture_id),
        )?;
        require_value_eq(
            get_path(&packet, &["work_packet", "verification", "rch_required"]),
            json!(true),
            &format!("{} packet rch requirement", case.fixture_id),
        )?;
    }
    Ok(())
}

fn assert_swarm_stale_busy_and_dirty_signals(manifest: &Value) -> Result<(), Box<dyn Error>> {
    let busy = read_json(manifest_golden_path(manifest, "busy")?);
    require_value_eq(
        get_path(&busy, &["summary", "active_agent_count"]),
        json!(2),
        "busy active agents",
    )?;
    require_value_eq(
        get_path(&busy, &["summary", "active_reservation_count"]),
        json!(1),
        "busy active reservations",
    )?;
    require_value_eq(
        get_path(&busy, &["summary", "dirty_worktree"]),
        json!(true),
        "busy dirty worktree",
    )?;

    let stale = read_json(manifest_golden_path(manifest, "stale_advisory")?);
    require_value_eq(
        get_path(&stale, &["summary", "stale_candidate_count"]),
        json!(1),
        "stale candidate count",
    )?;
    require_value_eq(
        get_path(
            &stale,
            &["recommendations", "0", "requires_human_confirmation"],
        ),
        json!(true),
        "stale human confirmation",
    )?;
    require_value_eq(
        get_path(&stale, &["beads", "in_progress", "2", "stale_state"]),
        json!("conflicting_evidence"),
        "stale conflicting evidence",
    )?;

    let conflict = read_json(manifest_golden_path(manifest, "reservation_conflict")?);
    require_value_eq(
        get_path(&conflict, &["beads", "ready", "0", "safe_to_claim"]),
        json!(false),
        "reservation conflict safe_to_claim",
    )?;
    require_value_eq(
        get_path(&conflict, &["reservations", "0", "overlaps_dirty_worktree"]),
        json!(true),
        "reservation conflict dirty overlap",
    )?;
    Ok(())
}

fn assert_swarm_proof_lint_and_pattern_surfaces_compose() -> Result<(), Box<dyn Error>> {
    let (_clear_tmp, clear_path) =
        write_swarm_evidence_fixture("integrated-clear-proof", integrated_clear_proof_sources())?;
    assert_integrated_clear_proof(&clear_path)?;

    let (_debt_tmp, debt_path) =
        write_swarm_evidence_fixture("integrated-proof-debt", integrated_proof_debt_sources())?;
    assert_integrated_proof_debt(&debt_path)?;

    Ok(())
}

fn assert_integrated_clear_proof(fixture_path: &Path) -> Result<(), Box<dyn Error>> {
    let evidence = run_swarm_evidence_fixture(fixture_path, Some("cass-integrated-clear"))?;
    let proof_debt = run_swarm_proof_debt_fixture(fixture_path, Some("cass-integrated-clear"))?;
    let patterns = run_swarm_failure_patterns_fixture(fixture_path, Some("cass-integrated-clear"))?;
    let lint = run_swarm_lint_fixture(fixture_path, Some("cass-integrated-clear"))?;

    require_value_eq(
        get_path(&evidence, &["summary", "recommended_action"]),
        json!("proof-ledger-complete"),
        "clear evidence action",
    )?;
    require_value_eq(
        get_path(&proof_debt, &["summary", "recommended_action"]),
        json!("proof-debt-clear"),
        "clear proof-debt action",
    )?;
    require_value_eq(
        get_path(&patterns, &["summary", "recommended_action"]),
        json!("no-recurring-patterns"),
        "clear failure-pattern action",
    )?;
    require_value_eq(
        get_path(&lint, &["summary", "recommended_action"]),
        json!("coordination-clean"),
        "clear lint action",
    )?;
    Ok(())
}

fn assert_integrated_proof_debt(fixture_path: &Path) -> Result<(), Box<dyn Error>> {
    let lint = run_swarm_lint_fixture(fixture_path, None)?;
    let evidence = run_swarm_evidence_fixture(fixture_path, None)?;
    let proof_debt = run_swarm_proof_debt_fixture(fixture_path, None)?;
    let patterns = run_swarm_failure_patterns_fixture(fixture_path, None)?;

    require_value_eq(
        get_path(&lint, &["status"]),
        json!("partial"),
        "debt lint status",
    )?;
    require_value_eq(
        get_path(&lint, &["summary", "recommended_action"]),
        json!("inspect-unavailable-providers"),
        "debt lint action",
    )?;
    require_value_eq(
        get_path(&evidence, &["summary", "recommended_action"]),
        json!("inspect-unavailable-providers"),
        "debt evidence action",
    )?;
    require_value_eq(
        get_path(&proof_debt, &["summary", "recommended_action"]),
        json!("inspect-unavailable-providers"),
        "debt proof-debt action",
    )?;
    require(
        get_path(&proof_debt, &["summary", "blocking_debt_count"])
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0),
        "provider-partial proof debt should still expose blocking debt",
    )?;
    require(
        get_path(&patterns, &["patterns"])
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|pattern| {
                    pattern.get("kind").and_then(Value::as_str) == Some("proof-closeout-gap")
                })
            }),
        "debt failure-pattern surface should explain the proof closeout gap",
    )?;
    Ok(())
}

struct WorkPacketCase {
    fixture_id: &'static str,
    expected_status_action: &'static str,
    expected_packet_action: &'static str,
    expected_readiness: &'static str,
    safe_to_start: bool,
}

fn integrated_clear_proof_sources() -> Value {
    json!({
        "beads": {
            "closed": [{
                "id": "cass-integrated-clear",
                "title": "Complete integrated proof",
                "status": "closed",
                "close_reason": "Verified by rch",
                "commit_id": "abc123"
            }]
        },
        "agent_mail": {
            "messages": [{
                "thread_id": "cass-integrated-clear",
                "subject": "Closeout proof",
                "from": "FixtureAgent",
                "created_ts": "2026-05-08T16:00:00Z"
            }],
            "reservations": []
        },
        "git": {
            "dirty": false,
            "dirty_paths": [],
            "recent_commits": [{
                "hash": "abc123",
                "subject": "test: finish cass-integrated-clear",
                "changed_paths": ["tests/swarm_status_contract.rs"]
            }]
        },
        "evidence": {
            "recent_threads": [{
                "thread_id": "cass-integrated-clear",
                "subject": "Closeout proof",
                "sender": "FixtureAgent"
            }],
            "recent_proofs": [{
                "kind": "rch-test",
                "bead_id": "cass-integrated-clear",
                "commit_id": "abc123",
                "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-proof cargo test --test swarm_status_contract",
                "status": "passed",
                "remote_exit_status": 0,
                "changed_paths": ["tests/swarm_status_contract.rs"],
                "mail_thread_refs": ["cass-integrated-clear"]
            }],
            "proof_gaps": [],
            "redaction_applied": false
        },
        "processes": {},
        "cass_health": {},
        "cass_status": {}
    })
}

fn integrated_proof_debt_sources() -> Value {
    json!({
        "beads": {
            "closed": [{
                "id": "cass-integrated-debt",
                "title": "Missing integrated proof",
                "status": "closed",
                "close_reason": "Closed before proof",
                "commit_id": "def456"
            }]
        },
        "git": {
            "dirty": true,
            "dirty_paths": [{"path": "docs/unrelated.md"}],
            "recent_commits": [{
                "hash": "def456",
                "subject": "test: finish cass-integrated-debt",
                "changed_paths": ["tests/swarm_status_contract.rs"]
            }]
        },
        "evidence": {
            "recent_threads": [],
            "recent_proofs": [],
            "proof_gaps": [],
            "session_hits": [],
            "redaction_applied": false
        },
        "processes": {},
        "cass_health": {},
        "cass_status": {}
    })
}

#[test]
fn swarm_work_packet_cli_builds_ready_read_only_packet() -> Result<(), Box<dyn Error>> {
    let fixture_path = repo_path("tests/fixtures/swarm_status/healthy.inputs.json");
    let output = run_swarm_work_packet_fixture(&fixture_path, None)?;

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.work_packet.v1"),
        "schema version",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "bead_id"]),
        json!("cass-ready-1"),
        "selected bead",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "safe_to_start"]),
        json!(true),
        "safe_to_start",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "readiness_state"]),
        json!("ready"),
        "readiness state",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("claim-ready-bead"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(
            &output,
            &["work_packet", "coordination", "send_before_editing"],
        ),
        json!(true),
        "coordination send gate",
    )?;

    let reservations = get_path(&output, &["work_packet", "suggested_reservations"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("suggested reservations missing"))?;
    require(
        reservations.iter().any(|reservation| {
            reservation.get("path_pattern").and_then(Value::as_str) == Some("docs/**")
        }),
        "docs label should suggest docs reservation",
    )?;
    require(
        reservations.iter().any(|reservation| {
            reservation.get("path_pattern").and_then(Value::as_str) == Some("src/lib.rs")
        }),
        "swarm label should suggest existing swarm source reservation",
    )?;

    let commands = get_path(&output, &["work_packet", "verification", "commands"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("verification commands missing"))?;
    require(
        commands.iter().any(|command| {
            command
                .as_str()
                .is_some_and(|text| text.contains("cargo test --test swarm_status_contract"))
        }),
        "swarm packet should include focused swarm contract proof",
    )?;
    require(
        commands.iter().all(|command| {
            command
                .as_str()
                .is_some_and(|text| text.starts_with("rch exec -- env "))
        }),
        "verification commands must use rch",
    )?;
    assert_no_forbidden_fixture_leaks("work-packet-healthy", &output);
    Ok(())
}

#[test]
fn swarm_work_packet_cli_blocks_reserved_dirty_ready_work() -> Result<(), Box<dyn Error>> {
    let fixture_path = repo_path("tests/fixtures/swarm_status/reservation_conflict.inputs.json");
    let output = run_swarm_work_packet_fixture(&fixture_path, None)?;

    require_value_eq(
        get_path(&output, &["summary", "bead_id"]),
        json!("cass-ready-conflict"),
        "selected bead",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "safe_to_start"]),
        json!(false),
        "safe_to_start",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "readiness_state"]),
        json!("blocked"),
        "readiness state",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("coordinate-before-claim"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["work_packet", "suggested_reservations"]),
        json!([]),
        "blocked packets should not suggest new reservations",
    )?;

    let reasons = get_path(&output, &["work_packet", "readiness", "reasons"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("readiness reasons missing"))?;
    require(
        reasons
            .iter()
            .any(|reason| reason.as_str() == Some("active-reservation")),
        "reservation blocker missing",
    )?;
    require(
        reasons
            .iter()
            .any(|reason| reason.as_str() == Some("dirty-peer-work")),
        "dirty peer blocker missing",
    )?;
    assert_no_forbidden_fixture_leaks("work-packet-conflict", &output);
    Ok(())
}

#[test]
fn swarm_work_packet_cli_defers_when_build_pressure_is_high() -> Result<(), Box<dyn Error>> {
    let fixture_path = repo_path("tests/fixtures/swarm_status/build_pressure.inputs.json");
    let output = run_swarm_work_packet_fixture(&fixture_path, None)?;

    require_value_eq(
        get_path(&output, &["summary", "readiness_state"]),
        json!("build-pressure-high"),
        "readiness state",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("wait-for-rch-capacity"),
        "recommended action",
    )?;
    let fallback_command = get_path(&output, &["work_packet", "fallback_actions"])
        .and_then(Value::as_array)
        .and_then(|actions| actions.first())
        .and_then(|action| action.get("command"))
        .cloned();
    require_value_eq(
        fallback_command.as_ref(),
        json!("rch status"),
        "fallback command",
    )?;
    require_value_eq(
        get_path(&output, &["work_packet", "verification", "rch_required"]),
        json!(true),
        "rch required",
    )?;
    assert_no_forbidden_fixture_leaks("work-packet-build-pressure", &output);
    Ok(())
}

#[test]
fn swarm_work_packet_cli_reports_missing_requested_bead() -> Result<(), Box<dyn Error>> {
    let fixture_path = repo_path("tests/fixtures/swarm_status/healthy.inputs.json");
    let output = run_swarm_work_packet_fixture(&fixture_path, Some("cass-missing"))?;

    require_value_eq(
        get_path(&output, &["filter", "bead_id"]),
        json!("cass-missing"),
        "filter bead",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "bead_id"]),
        json!(null),
        "selected bead",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "readiness_state"]),
        json!("bead-not-found"),
        "readiness state",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("inspect-bead"),
        "recommended action",
    )?;
    let fallback_command = get_path(&output, &["work_packet", "fallback_actions"])
        .and_then(Value::as_array)
        .and_then(|actions| actions.first())
        .and_then(|action| action.get("command"))
        .cloned();
    require_value_eq(
        fallback_command.as_ref(),
        json!("br show cass-missing --json"),
        "fallback command",
    )?;
    assert_no_forbidden_fixture_leaks("work-packet-missing-bead", &output);
    Ok(())
}

#[test]
fn swarm_coordination_lint_cli_reports_clean_read_only_fixture() -> Result<(), Box<dyn Error>> {
    let fixture_path = repo_path("tests/fixtures/swarm_status/healthy.inputs.json");
    let output = run_swarm_lint_fixture(&fixture_path, None)?;

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.coordination_lint.v1"),
        "schema version",
    )?;
    require_value_eq(get_path(&output, &["status"]), json!("ok"), "status")?;
    require_value_eq(
        get_path(&output, &["summary", "finding_count"]),
        json!(0),
        "finding count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("coordination-clean"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "read_only"]),
        json!(true),
        "read-only contract",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "mutation_performed"]),
        json!(false),
        "mutation flag",
    )?;
    assert_no_forbidden_fixture_leaks("coordination-lint-clean", &output);
    Ok(())
}

#[test]
fn swarm_coordination_lint_cli_catches_protocol_findings() -> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "coordination-lint-problems",
        json!({
            "beads": {
                "ready": [],
                "in_progress": [
                    {
                        "id": "cass-start-missing",
                        "title": "Missing intro mail",
                        "status": "in_progress",
                        "updated_at": "2026-05-08T12:00:00Z"
                    },
                    {
                        "id": "cass-owned-stale",
                        "title": "Old but actively owned work",
                        "status": "in_progress",
                        "updated_at": "2026-05-08T12:00:00Z"
                    }
                ],
                "blocked": [],
                "closed": [
                    {
                        "id": "cass-closed-missing",
                        "title": "Closed without proof",
                        "status": "closed",
                        "close_reason": ""
                    }
                ]
            },
            "agent_mail": {
                "agents": [
                    {
                        "name": "ActiveOwner",
                        "last_active_ts": "2026-05-08T15:59:00Z"
                    },
                    {
                        "name": "DeadOwner",
                        "last_active_ts": "2026-05-08T12:00:00Z"
                    }
                ],
                "messages": [
                    {
                        "id": 77,
                        "thread_id": "cass-unacked",
                        "subject": "Please ack before editing",
                        "from": "ActiveOwner",
                        "ack_required": true,
                        "created_ts": "2026-05-08T15:50:00Z"
                    },
                    {
                        "id": 78,
                        "thread_id": "cass-unsafe",
                        "subject": "force release stale reservation now",
                        "from": "ActiveOwner",
                        "created_ts": "2026-05-08T15:52:00Z"
                    },
                    {
                        "id": 79,
                        "thread_id": "cass-owned-stale",
                        "subject": "still working cass-owned-stale",
                        "from": "ActiveOwner",
                        "created_ts": "2026-05-08T15:55:00Z"
                    }
                ],
                "reservations": [
                    {
                        "holder": "ActiveOwner",
                        "path_pattern": "src/lib.rs",
                        "exclusive": true,
                        "reason": "cass-owned-stale",
                        "expires_ts": "2026-05-08T17:00:00Z"
                    },
                    {
                        "holder": "DeadOwner",
                        "path_pattern": "tests/**",
                        "exclusive": true,
                        "reason": "cass-dead-reservation",
                        "expires_ts": "2026-05-08T17:00:00Z"
                    },
                    {
                        "holder": "ActiveOwner",
                        "path_pattern": "docs/**",
                        "exclusive": true,
                        "reason": "cass-expired-reservation",
                        "expires_ts": "2026-05-08T12:30:00Z"
                    },
                    {
                        "holder": "ActiveOwner",
                        "path_pattern": "src/lib.rs",
                        "exclusive": true,
                        "reason": "cass-closed-missing",
                        "expires_ts": "2026-05-08T17:00:00Z"
                    }
                ]
            },
            "git": {
                "dirty": true,
                "dirty_paths": [{"status": "M", "path": "src/lib.rs"}],
                "recent_commits": []
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {},
            "evidence": {
                "recent_threads": [],
                "recent_proofs": [],
                "proof_gaps": [],
                "redaction_applied": false
            }
        }),
    )?;
    let output = run_swarm_lint_fixture(&fixture_path, None)?;
    let findings = get_path(&output, &["findings"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("findings missing"))?;
    let codes = findings
        .iter()
        .filter_map(|finding| finding.get("code").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();

    for (expected, missing_message) in [
        (
            "unacked-required-mail",
            "missing unacked-required-mail finding",
        ),
        (
            "unsafe-takeover-language",
            "missing unsafe-takeover-language finding",
        ),
        ("missing-start-mail", "missing missing-start-mail finding"),
        (
            "missing-closeout-mail",
            "missing missing-closeout-mail finding",
        ),
        (
            "missing-close-reason",
            "missing missing-close-reason finding",
        ),
        (
            "missing-proof-reference",
            "missing missing-proof-reference finding",
        ),
        ("stale-reservation", "missing stale-reservation finding"),
        (
            "dead-agent-stale-reservation",
            "missing dead-agent-stale-reservation finding",
        ),
        (
            "reservation-on-closed-bead",
            "missing reservation-on-closed-bead finding",
        ),
        (
            "reservation-without-known-bead",
            "missing reservation-without-known-bead finding",
        ),
        ("dirty-peer-files", "missing dirty-peer-files finding"),
    ] {
        require(codes.contains(expected), missing_message)?;
    }
    require(
        !findings.iter().any(|finding| {
            finding.get("code").and_then(Value::as_str) == Some("missing-start-mail")
                && finding.get("subject_id").and_then(Value::as_str) == Some("cass-owned-stale")
        }),
        "stale-but-owned work should not be treated as missing coordination",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("fix-coordination-before-closeout"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "agent_mail_mutations"]),
        json!(false),
        "Agent Mail mutation flag",
    )?;
    assert_no_forbidden_fixture_leaks("coordination-lint-problems", &output);
    Ok(())
}

#[test]
fn swarm_coordination_lint_cli_reports_offline_agent_mail() -> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "coordination-lint-offline-mail",
        json!({
            "beads": {
                "ready": [],
                "in_progress": [],
                "blocked": []
            },
            "git": {
                "dirty": false,
                "dirty_paths": [],
                "recent_commits": []
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {},
            "evidence": {
                "recent_threads": [],
                "recent_proofs": [],
                "proof_gaps": [],
                "redaction_applied": false
            }
        }),
    )?;
    let output = run_swarm_lint_fixture(&fixture_path, None)?;
    let codes = get_path(&output, &["findings"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("findings missing"))?
        .iter()
        .filter_map(|finding| finding.get("code").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();

    require_value_eq(get_path(&output, &["status"]), json!("partial"), "status")?;
    require(
        codes.contains("agent-mail-unavailable"),
        "missing offline Agent Mail finding",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("inspect-unavailable-providers"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "bead_mutations"]),
        json!(false),
        "Beads mutation flag",
    )?;
    assert_no_forbidden_fixture_leaks("coordination-lint-offline", &output);
    Ok(())
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
fn swarm_proof_debt_cli_reports_clear_complete_fixture() -> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "proof-debt-clear",
        json!({
            "beads": {
                "closed": [{
                    "id": "cass-proof-clear",
                    "title": "Complete proof closeout",
                    "status": "closed",
                    "close_reason": "Verified by rch",
                    "commit_id": "abc123"
                }]
            },
            "agent_mail": {
                "messages": [{
                    "thread_id": "cass-proof-clear",
                    "subject": "Closeout proof",
                    "from": "FixtureAgent",
                    "created_ts": "2026-05-08T16:00:00Z"
                }],
                "reservations": []
            },
            "git": {
                "dirty": false,
                "dirty_paths": [],
                "recent_commits": [{
                    "hash": "abc123",
                    "subject": "feat: finish cass-proof-clear",
                    "authored_ts": "2026-05-08T15:55:00Z",
                    "changed_paths": ["src/lib.rs", "tests/swarm_status_contract.rs"]
                }]
            },
            "evidence": {
                "recent_threads": [{
                    "thread_id": "cass-proof-clear",
                    "subject": "Closeout proof",
                    "sender": "FixtureAgent",
                    "created_ts": "2026-05-08T16:00:00Z"
                }],
                "recent_proofs": [{
                    "kind": "rch-test",
                    "bead_id": "cass-proof-clear",
                    "commit_id": "abc123",
                    "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-proof cargo test --test swarm_status_contract",
                    "status": "passed",
                    "remote_exit_status": 0,
                    "changed_paths": ["src/lib.rs", "tests/swarm_status_contract.rs"],
                    "mail_thread_refs": ["cass-proof-clear"]
                }],
                "proof_gaps": [],
                "redaction_applied": false
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {}
        }),
    )?;
    let output = run_swarm_proof_debt_fixture(&fixture_path, Some("cass-proof-clear"))?;

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.proof_debt.v1"),
        "schema version",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "debt_count"]),
        json!(0),
        "debt count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("proof-debt-clear"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "read_only"]),
        json!(true),
        "read-only contract",
    )?;
    require_value_eq(
        get_path(&output, &["gate_contract", "fails_closed_by_default"]),
        json!(false),
        "default gate behavior",
    )?;
    require_value_eq(
        get_path(&output, &["privacy", "raw_session_content_included"]),
        json!(false),
        "raw session privacy flag",
    )?;
    assert_no_forbidden_fixture_leaks("proof-debt-clear", &output);
    Ok(())
}

#[test]
fn swarm_proof_debt_cli_prioritizes_and_suppresses_debt() -> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "proof-debt-mixed",
        json!({
            "beads": {
                "closed": [
                    {"id": "cass-no-proof", "status": "closed", "close_reason": "missing proof"},
                    {"id": "cass-clippy-only", "status": "closed", "close_reason": "linted"},
                    {"id": "cass-ignored-stress", "status": "closed", "close_reason": "stress artifact retained"},
                    {"id": "cass-ubs", "status": "closed", "close_reason": "UBS baseline reviewed"},
                    {"id": "cass-mail-missing", "status": "closed", "close_reason": "proof exists"}
                ]
            },
            "agent_mail": {
                "messages": [
                    {"thread_id": "cass-no-proof", "subject": "closeout cass-no-proof", "from": "FixtureAgent"},
                    {"thread_id": "cass-clippy-only", "subject": "closeout cass-clippy-only", "from": "FixtureAgent"},
                    {"thread_id": "cass-ignored-stress", "subject": "closeout cass-ignored-stress", "from": "FixtureAgent"},
                    {"thread_id": "cass-ubs", "subject": "closeout cass-ubs", "from": "FixtureAgent"}
                ],
                "reservations": []
            },
            "git": {
                "dirty": true,
                "dirty_paths": [{"path": "docs/unrelated.md"}],
                "recent_commits": [
                    {"hash": "aaa111", "subject": "finish cass-no-proof", "changed_paths": ["src/no_proof.rs"]},
                    {"hash": "bbb222", "subject": "finish cass-clippy-only", "changed_paths": ["src/clippy.rs"]},
                    {"hash": "ccc333", "subject": "finish cass-ignored-stress", "changed_paths": ["tests/stress.rs"]},
                    {"hash": "ddd444", "subject": "finish cass-ubs", "changed_paths": ["src/ubs.rs"]},
                    {"hash": "eee555", "subject": "finish cass-mail-missing", "changed_paths": ["src/mail.rs"]}
                ]
            },
            "evidence": {
                "recent_threads": [
                    {"thread_id": "cass-no-proof", "subject": "closeout cass-no-proof", "sender": "FixtureAgent"},
                    {"thread_id": "cass-clippy-only", "subject": "closeout cass-clippy-only", "sender": "FixtureAgent"},
                    {"thread_id": "cass-ignored-stress", "subject": "closeout cass-ignored-stress", "sender": "FixtureAgent"},
                    {"thread_id": "cass-ubs", "subject": "closeout cass-ubs", "sender": "FixtureAgent"}
                ],
                "recent_proofs": [
                    {
                        "kind": "rch-clippy",
                        "bead_id": "cass-clippy-only",
                        "commit_id": "bbb222",
                        "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-proof cargo clippy --all-targets -- -D warnings",
                        "status": "passed",
                        "remote_exit_status": 0,
                        "changed_paths": ["src/clippy.rs"]
                    },
                    {
                        "kind": "rch-stress",
                        "bead_id": "cass-ignored-stress",
                        "commit_id": "ccc333",
                        "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-proof cargo test --test swarm_status_contract swarm_status_large_fixture_stress_artifact_10k -- --ignored",
                        "status": "ignored",
                        "changed_paths": ["tests/stress.rs"],
                        "artifact_refs": ["docs/artifacts/swarm/stress-proof.json"],
                        "suppression_reason": "stress proof is reviewed only during explicit perf sweeps"
                    },
                    {
                        "kind": "ubs",
                        "bead_id": "cass-ubs",
                        "commit_id": "ddd444",
                        "command_shape": "ubs src/lib.rs",
                        "status": "passed",
                        "remote_exit_status": 0,
                        "changed_paths": ["src/ubs.rs"]
                    },
                    {
                        "kind": "rch-test",
                        "bead_id": "cass-mail-missing",
                        "commit_id": "eee555",
                        "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-proof cargo test --test swarm_status_contract",
                        "status": "passed",
                        "remote_exit_status": 0,
                        "changed_paths": ["src/mail.rs"]
                    }
                ],
                "proof_gaps": [{
                    "kind": "ubs-baseline-warning",
                    "severity": "medium",
                    "bead_id": "cass-ubs",
                    "summary": "UBS warning count matches the known acceptable baseline.",
                    "suppression_reason": "known acceptable UBS baseline warning"
                }],
                "redaction_applied": false
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {}
        }),
    )?;
    let output = run_swarm_proof_debt_fixture(&fixture_path, None)?;
    let debt_items = get_path(&output, &["debt_items"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("debt items missing"))?;
    let kinds = debt_items
        .iter()
        .filter_map(|item| item.get("kind").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();

    for expected in [
        "missing-proof",
        "missing-rch-proof",
        "incomplete-proof-command-set",
        "ignored-stress-proof",
        "ubs-baseline-warning",
        "unrelated-dirty-file",
        "missing-closeout-mail",
    ] {
        require(
            kinds.contains(expected),
            format!("missing proof debt kind {expected}"),
        )?;
    }
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("remediate-proof-debt"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "suppressed_count"]),
        json!(2),
        "suppressed count",
    )?;
    require(
        get_path(&output, &["summary", "blocking_debt_count"])
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 3),
        "blocking debt count should include high and medium unsuppressed debt",
    )?;
    require(
        debt_items.iter().any(|item| {
            item.get("kind").and_then(Value::as_str) == Some("ignored-stress-proof")
                && item
                    .get("suppression")
                    .and_then(|suppression| suppression.get("status"))
                    .and_then(Value::as_str)
                    == Some("suppressed")
                && item
                    .get("observed_evidence_refs")
                    .and_then(Value::as_array)
                    .is_some_and(|refs| {
                        refs.iter().any(|value| {
                            value.as_str() == Some("docs/artifacts/swarm/stress-proof.json")
                        })
                    })
        }),
        "ignored stress proof should remain artifact-backed suppressed debt",
    )?;
    require(
        debt_items.iter().any(|item| {
            item.get("kind").and_then(Value::as_str) == Some("ubs-baseline-warning")
                && item
                    .get("suppression")
                    .and_then(|suppression| suppression.get("reason"))
                    .and_then(Value::as_str)
                    == Some("known acceptable UBS baseline warning")
        }),
        "UBS baseline warning should carry suppression reason",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "agent_mail_mutations"]),
        json!(false),
        "Agent Mail mutation flag",
    )?;
    require_value_eq(
        get_path(&output, &["gate_contract", "explicit_gate_required"]),
        json!(true),
        "gate opt-in contract",
    )?;
    assert_no_forbidden_fixture_leaks("proof-debt-mixed", &output);
    Ok(())
}

#[test]
fn swarm_failure_patterns_cli_reports_no_patterns_for_clean_fixture() -> Result<(), Box<dyn Error>>
{
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "failure-patterns-clear",
        json!({
            "beads": {
                "closed": [{
                    "id": "cass-pattern-clear",
                    "title": "Clean closeout",
                    "status": "closed",
                    "close_reason": "Verified by rch proof",
                    "commit_id": "abc123"
                }]
            },
            "agent_mail": {
                "messages": [{"thread_id": "cass-pattern-clear", "subject": "closeout", "from": "FixtureAgent"}],
                "reservations": []
            },
            "git": {
                "dirty": false,
                "dirty_paths": [],
                "recent_commits": [{
                    "hash": "abc123",
                    "subject": "feat: finish cass-pattern-clear",
                    "changed_paths": ["src/lib.rs"]
                }]
            },
            "evidence": {
                "recent_threads": [{"thread_id": "cass-pattern-clear", "subject": "closeout", "sender": "FixtureAgent"}],
                "recent_proofs": [{
                    "kind": "rch-test",
                    "bead_id": "cass-pattern-clear",
                    "commit_id": "abc123",
                    "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-pattern cargo test --test swarm_status_contract",
                    "status": "passed",
                    "remote_exit_status": 0,
                    "changed_paths": ["src/lib.rs"]
                }],
                "proof_gaps": [],
                "session_hits": [],
                "redaction_applied": false
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {}
        }),
    )?;
    let output = run_swarm_failure_patterns_fixture(&fixture_path, Some("cass-pattern-clear"))?;

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.failure_patterns.v1"),
        "schema version",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "pattern_count"]),
        json!(0),
        "pattern count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("no-recurring-patterns"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "auto_create_beads"]),
        json!(false),
        "auto-create contract",
    )?;
    assert_no_forbidden_fixture_leaks("failure-patterns-clear", &output);
    Ok(())
}

#[test]
fn swarm_failure_patterns_cli_ranks_test_suggestions_and_redacts_sessions()
-> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "failure-patterns-mixed",
        json!({
            "beads": {
                "closed": [
                    {
                        "id": "cass-fsqlite",
                        "status": "closed",
                        "title": "fsqlite join regression",
                        "close_reason": "frankensqlite execute_join_select range end index out of range"
                    },
                    {
                        "id": "cass-local-cargo",
                        "status": "closed",
                        "title": "local cargo proof",
                        "close_reason": "cargo test was run locally without rch"
                    },
                    {
                        "id": "cass-robot-json",
                        "status": "closed",
                        "title": "robot json schema drift",
                        "close_reason": "robot JSON schema drift needs a golden"
                    }
                ]
            },
            "agent_mail": {
                "messages": [
                    {"thread_id": "cass-fsqlite", "subject": "closeout cass-fsqlite", "from": "FixtureAgent"},
                    {"thread_id": "cass-local-cargo", "subject": "closeout cass-local-cargo", "from": "FixtureAgent"},
                    {"thread_id": "cass-robot-json", "subject": "closeout cass-robot-json", "from": "FixtureAgent"}
                ],
                "reservations": []
            },
            "git": {
                "dirty": false,
                "dirty_paths": [],
                "recent_commits": [
                    {"hash": "aaa111", "subject": "finish cass-fsqlite", "changed_paths": ["src/storage/sqlite.rs"]},
                    {"hash": "bbb222", "subject": "finish cass-local-cargo", "changed_paths": ["tests/swarm_status_contract.rs"]},
                    {"hash": "ccc333", "subject": "finish cass-robot-json", "changed_paths": ["src/lib.rs"]}
                ]
            },
            "evidence": {
                "recent_threads": [
                    {"thread_id": "cass-fsqlite", "subject": "closeout cass-fsqlite", "sender": "FixtureAgent"},
                    {"thread_id": "cass-local-cargo", "subject": "closeout cass-local-cargo", "sender": "FixtureAgent"},
                    {"thread_id": "cass-robot-json", "subject": "closeout cass-robot-json", "sender": "FixtureAgent"}
                ],
                "recent_proofs": [
                    {
                        "kind": "cargo-test",
                        "bead_id": "cass-local-cargo",
                        "commit_id": "bbb222",
                        "command_shape": "cargo test --test swarm_status_contract",
                        "status": "passed",
                        "remote_exit_status": 0,
                        "changed_paths": ["tests/swarm_status_contract.rs"]
                    },
                    {
                        "kind": "rch-stress",
                        "bead_id": "cass-flaky",
                        "command_shape": "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-pattern cargo test --test e2e_large_dataset -- --ignored",
                        "status": "ignored",
                        "failure_signature": "e2e_large_dataset timeout during stress proof"
                    }
                ],
                "proof_gaps": [
                    {
                        "kind": "missing-proof",
                        "severity": "high",
                        "bead_id": "cass-fsqlite",
                        "summary": "Closed bead has no linked proof artifact."
                    },
                    {
                        "kind": "robot-json-contract-gap",
                        "severity": "medium",
                        "bead_id": "cass-robot-json",
                        "summary": "Robot JSON schema changed without a golden."
                    }
                ],
                "session_hits": [{
                    "session_id": "session-1",
                    "bead_id": "cass-fsqlite",
                    "summary": "frankensqlite execute_join_select hit range end index out of range",
                    "failure_signature": "range end index 12 out of range for slice of length 10",
                    "evidence_ref": "pack:///home/alice/private-client/session.jsonl#L44",
                    "body": "PRIVATE_SESSION_DO_NOT_LEAK"
                }],
                "redaction_applied": true
            },
            "processes": {},
            "cass_health": {},
            "cass_status": {}
        }),
    )?;
    let output = run_swarm_failure_patterns_fixture(&fixture_path, None)?;
    let patterns = get_path(&output, &["patterns"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("patterns missing"))?;
    let kinds = patterns
        .iter()
        .filter_map(|pattern| pattern.get("kind").and_then(Value::as_str))
        .collect::<Vec<_>>();

    require(
        kinds.starts_with(&["fsqlite-query-shape-regression", "panic-surface-regression"]),
        format!("high-severity pattern ordering drifted: {kinds:?}"),
    )?;
    for expected in [
        "flaky-or-toxic-suite",
        "proof-closeout-gap",
        "rch-proof-discipline-gap",
        "robot-json-contract-gap",
    ] {
        require(
            kinds.contains(&expected),
            format!("missing failure pattern kind {expected}"),
        )?;
    }
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("review-regression-suggestions"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["source_accounting", "session_hit_count"]),
        json!(1),
        "session hit count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "candidate_test_count"]),
        get_path(&output, &["summary", "pattern_count"])
            .cloned()
            .ok_or_else(|| test_error("pattern count missing"))?,
        "candidate test count",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "auto_create_beads"]),
        json!(false),
        "auto-create contract",
    )?;
    require(
        patterns.iter().all(|pattern| {
            pattern
                .get("false_positive_controls")
                .and_then(Value::as_array)
                .is_some_and(|controls| controls.len() >= 3)
        }),
        "every pattern should carry false-positive controls",
    )?;
    assert_no_forbidden_fixture_leaks("failure-patterns-mixed", &output);
    Ok(())
}

#[test]
fn swarm_dependency_drift_cli_reports_clean_read_only_fixture() -> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "dependency-drift-clean",
        json!({
            "dependency_drift": {
                "network": {"upstream_status": "not_checked"},
                "dependencies": [{
                    "name": "frankensqlite",
                    "package": "fsqlite",
                    "manifest_key": "frankensqlite",
                    "source_kind": "git",
                    "pinned_rev": "abc123",
                    "local_head": "abc123456789",
                    "dirty": false,
                    "sibling_status": "clean",
                    "required_downstream_tests": [
                        "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-strict-target cargo check --features strict-path-dep-validation"
                    ]
                }]
            }
        }),
    )?;
    let output = run_swarm_dependency_drift_fixture(&fixture_path)?;

    require_value_eq(
        get_path(&output, &["schema_version"]),
        json!("cass.swarm.dependency_drift.v1"),
        "schema version",
    )?;
    require_value_eq(get_path(&output, &["status"]), json!("ok"), "status")?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("dependencies-clean"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "release_readiness"]),
        json!("ready"),
        "release readiness",
    )?;
    require_value_eq(
        get_path(
            &output,
            &["dependencies", "0", "sibling", "revision_matches_pin"],
        ),
        json!(true),
        "pin match",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "read_only"]),
        json!(true),
        "read-only contract",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "touches_network"]),
        json!(false),
        "network mutation contract",
    )?;
    assert_no_forbidden_fixture_leaks("dependency-drift-clean", &output);
    Ok(())
}

#[test]
fn swarm_dependency_drift_cli_flags_pin_dirty_missing_and_network_risks()
-> Result<(), Box<dyn Error>> {
    let (_tmp, fixture_path) = write_swarm_evidence_fixture(
        "dependency-drift-risky",
        json!({
            "dependency_drift": {
                "network": {"upstream_status": "unavailable"},
                "dependencies": [
                    {
                        "name": "frankensqlite",
                        "package": "fsqlite",
                        "manifest_key": "frankensqlite",
                        "source_kind": "git",
                        "pinned_rev": "abc123",
                        "local_head": "def456789",
                        "dirty": true,
                        "sibling_status": "dirty"
                    },
                    {
                        "name": "frankensearch",
                        "package": "frankensearch",
                        "manifest_key": "frankensearch",
                        "source_kind": "git",
                        "pinned_rev": "1111111",
                        "local_head": "2222222",
                        "dirty": false,
                        "sibling_status": "clean"
                    },
                    {
                        "name": "fsqlite-types",
                        "package": "fsqlite-types",
                        "manifest_key": "fsqlite-types",
                        "source_kind": "git",
                        "manifest_status": "missing-rev",
                        "dirty": false,
                        "sibling_status": "clean"
                    },
                    {
                        "name": "toon",
                        "package": "tru",
                        "manifest_key": "toon",
                        "source_kind": "git",
                        "pinned_rev": "5669b72a",
                        "dirty": false,
                        "sibling_status": "missing"
                    }
                ]
            }
        }),
    )?;
    let output = run_swarm_dependency_drift_fixture(&fixture_path)?;

    require_value_eq(get_path(&output, &["status"]), json!("warning"), "status")?;
    require_value_eq(
        get_path(&output, &["summary", "recommended_action"]),
        json!("restore-manifest-pin"),
        "recommended action",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "release_readiness"]),
        json!("blocked"),
        "release readiness",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "warning_count"]),
        json!(3),
        "warning count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "dirty_count"]),
        json!(1),
        "dirty count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "local_rev_mismatch_count"]),
        json!(2),
        "local rev mismatch count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "missing_sibling_count"]),
        json!(1),
        "missing sibling count",
    )?;
    require_value_eq(
        get_path(&output, &["summary", "network_status"]),
        json!("unavailable"),
        "network status",
    )?;

    let recommendation_kinds = get_path(&output, &["recommendations"])
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("recommendations missing"))?
        .iter()
        .filter_map(|recommendation| recommendation.get("kind").and_then(Value::as_str))
        .collect::<Vec<_>>();
    require(
        recommendation_kinds.contains(&"frankensqlite-first"),
        "frankensqlite-specific recommendation missing",
    )?;
    require(
        serde_json::to_string(&output)?.contains(
            "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-strict-target cargo check --features strict-path-dep-validation",
        ),
        "strict validation command missing",
    )?;
    require_value_eq(
        get_path(&output, &["mutation_contract", "mutates_git"]),
        json!(false),
        "git mutation contract",
    )?;
    assert_no_forbidden_fixture_leaks("dependency-drift-risky", &output);
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

fn run_swarm_work_packet_fixture(
    fixture_path: &Path,
    bead: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore — fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "work-packet", "--json", "--fixture"]);
    cmd.arg(fixture_path);
    if let Some(bead_id) = bead {
        cmd.args(["--bead", bead_id]);
    }

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm work-packet should not log to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn run_swarm_lint_fixture(
    fixture_path: &Path,
    bead: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore — fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "lint", "--json", "--fixture"]);
    cmd.arg(fixture_path);
    if let Some(bead_id) = bead {
        cmd.args(["--bead", bead_id]);
    }

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm lint should not log to stderr: {}",
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

fn run_swarm_proof_debt_fixture(
    fixture_path: &Path,
    bead: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore - fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "proof-debt", "--json", "--fixture"]);
    cmd.arg(fixture_path);
    if let Some(bead_id) = bead {
        cmd.args(["--bead", bead_id]);
    }

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm proof-debt should not log to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn run_swarm_failure_patterns_fixture(
    fixture_path: &Path,
    bead: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore - fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "failure-patterns", "--json", "--fixture"]);
    cmd.arg(fixture_path);
    if let Some(bead_id) = bead {
        cmd.args(["--bead", bead_id]);
    }

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm failure-patterns should not log to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn run_swarm_dependency_drift_fixture(fixture_path: &Path) -> Result<Value, Box<dyn Error>> {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore - fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.args(["swarm", "dependency-drift", "--json", "--fixture"]);
    cmd.arg(fixture_path);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    require(
        output.stderr.is_empty(),
        format!(
            "swarm dependency-drift should not log to stderr: {}",
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

fn manifest_input_path(manifest: &Value, fixture_id: &str) -> Result<PathBuf, Box<dyn Error>> {
    manifest_path_field(manifest, fixture_id, "input_path")
}

fn manifest_golden_path(manifest: &Value, fixture_id: &str) -> Result<PathBuf, Box<dyn Error>> {
    manifest_path_field(manifest, fixture_id, "golden_path")
}

fn manifest_path_field(
    manifest: &Value,
    fixture_id: &str,
    field: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let scenario = scenarios(manifest)
        .into_iter()
        .find(|scenario| string_field(scenario, "fixture_id") == fixture_id)
        .ok_or_else(|| test_error(format!("missing swarm fixture scenario {fixture_id}")))?;
    Ok(repo_path(string_field(scenario, field)))
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
        current = match current {
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => current.get(*key)?,
        };
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
