use assert_cmd::Command;
use coding_agent_search::operations_dashboard::{
    render_operations_dashboard_fixture, render_operations_dashboard_html,
};
use coding_agent_search::swarm_status::{
    FixtureSwarmAdapterSet, SwarmProviderName, SwarmSourceAdapter,
};
use serde_json::{Value, json};
use std::error::Error;
use std::fs;
use std::path::Path;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn verify(condition: bool, message: String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}

macro_rules! verify {
    ($condition:expr) => {
        verify(
            $condition,
            format!("verification failed: {}", stringify!($condition)),
        )?
    };
    ($condition:expr, $($argument:tt)+) => {
        verify($condition, format!($($argument)+))?
    };
}

macro_rules! verify_eq {
    ($actual:expr, $expected:expr) => {{
        let actual = &$actual;
        let expected = &$expected;
        verify(
            actual == expected,
            format!("values differ: actual={actual:?}, expected={expected:?}"),
        )?
    }};
    ($actual:expr, $expected:expr, $($argument:tt)+) => {{
        let actual = &$actual;
        let expected = &$expected;
        verify(
            actual == expected,
            format!(
                "{}; actual={actual:?}, expected={expected:?}",
                format_args!($($argument)+)
            ),
        )?
    }};
}

fn dashboard_source() -> Value {
    let long_ref = format!("reports/{}capsule.json", "nested/".repeat(32));
    json!({
        "current_goal": "investigate /home/alice/private <script>alert(1)</script>",
        "guide": {
            "intent": {"raw": "fix-ci"},
            "recommended_action": "satisfy-prerequisites-then-follow-plan",
            "plan": {
                "macro_id": "fix-ci-regression",
                "prerequisites": [{"fact": "ci_logs_available", "status": "unmet"}],
                "required_proof_gates": ["gate-green"]
            }
        },
        "workflow_macros": {"macros": [{
            "id": "fix-ci-regression", "title": "Diagnose <b>CI</b>",
            "readiness": "blocked", "privacy_tier": "low",
            "missing_preflight_facts": ["ci_logs_available"]
        }]},
        "resource_plan": {"status": "warning", "summary": {
            "readiness": "blocked", "recommended_action": "free-disk-before-action",
            "blocked_count": 1, "high_risk_count": 1, "warning_count": 1
        }},
        "privacy_exposure": {"status": "warning", "summary": {
            "readiness": "opt-in-required", "recommended_action": "review-required-opt-ins"
        }, "risk_categories": [{"category": "secrets-detected", "severity": "high", "count": 1}]},
        "repro_capsules": [{
            "local_ref": long_ref,
            "payload": {"status": "ok", "manifest": {
                "capsule_id": "capsule-blake3:abc123", "incident_kind": "ci-failure"
            }, "rerun": {"targets_live_data": false, "command_template": "cass swarm repro-capsule --json --fixture repro-capsule.fixture.json"}}
        }],
        "search_results": {"results": [{"trust": {
            "trust_tier": "stale", "confidence": "medium", "stale_reason": "aged_out",
            "recommended_followup": "inspect alice@example.com", "provenance_refs": ["commit:abcdef123456"]
        }}]},
        "next_proof_command": "cass swarm evidence --json"
    })
}

fn write_fixture(path: &Path) -> TestResult {
    let fixture = json!({
        "fixture_id": "dashboard-hostile",
        "description": "deterministic operations dashboard fixture",
        "sources": {"operations_dashboard": dashboard_source()}
    });
    fs::write(path, serde_json::to_vec_pretty(&fixture)?)?;
    Ok(())
}

#[test]
fn fixture_provider_projects_only_the_dashboard_source() -> TestResult {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("dashboard.json");
    write_fixture(&path)?;

    let adapters = FixtureSwarmAdapterSet::from_fixture_path(&path)?;
    let source = adapters
        .input()
        .source_value(SwarmProviderName::OperationsDashboard)
        .ok_or("operations_dashboard fixture source missing")?;
    let dashboard =
        render_operations_dashboard_fixture(adapters.input().fixture_id(), Some(source));
    verify_eq!(dashboard["status"], "warning");
    verify_eq!(dashboard["summary"]["blocked_prerequisite_count"], 1);
    verify_eq!(dashboard["summary"]["trust_warning_count"], 1);

    let adapter = adapters
        .all_adapters()
        .into_iter()
        .find(|adapter| adapter.provider() == SwarmProviderName::OperationsDashboard)
        .ok_or("operations dashboard adapter missing")?;
    let snapshot = adapter.collect();
    verify_eq!(snapshot.name, SwarmProviderName::OperationsDashboard);
    verify_eq!(snapshot.source, "fixture:operations_dashboard");
    verify_eq!(snapshot.payload["guide"]["intent"]["raw"], "fix-ci");
    verify!(!serde_json::to_string(&snapshot.payload)?.contains("/home/alice/private"));
    Ok(())
}

#[test]
fn cli_html_is_byte_deterministic_offline_and_hostile_safe() -> TestResult {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("dashboard.json");
    write_fixture(&path)?;

    let run_html = || -> TestResult<Vec<u8>> {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("cass")) // ubs:ignore - fixed test binary from assert_cmd.
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .args(["swarm", "dashboard", "--html", "--fixture"])
            .arg(&path)
            .output()?;
        verify!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        verify!(output.stderr.is_empty(), "HTML command wrote diagnostics");
        Ok(output.stdout)
    };

    let first = run_html()?;
    let second = run_html()?;
    verify_eq!(
        first,
        second,
        "HTML report changed across identical invocations"
    );
    let html = String::from_utf8(first)?;
    verify!(html.starts_with("<!doctype html>\n"));
    verify!(html.contains("Content-Security-Policy"));
    verify!(html.contains("default-src 'none'"));
    verify!(html.contains("&lt;b&gt;CI&lt;/b&gt;"));
    verify!(html.contains("overflow-wrap:anywhere"));
    verify!(html.contains("cass swarm evidence --json"));
    for forbidden in [
        "/home/alice/private",
        "alice@example.com",
        "<script",
        "https://",
        "http://",
        "<form",
        "--apply",
    ] {
        verify!(
            !html.contains(forbidden),
            "HTML leaked/embedded {forbidden}"
        );
    }

    let json_output = Command::new(assert_cmd::cargo::cargo_bin!("cass")) // ubs:ignore - fixed test binary from assert_cmd.
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["swarm", "dashboard", "--json", "--fixture"])
        .arg(&path)
        .output()?;
    verify!(json_output.status.success());
    verify!(json_output.stderr.is_empty());
    let payload: Value = serde_json::from_slice(&json_output.stdout)?;
    verify_eq!(payload["mutation_contract"]["read_only"], true);
    verify_eq!(payload["mutation_contract"]["mutates_files"], false);
    verify_eq!(payload["mutation_contract"]["mutates_db"], false);
    verify_eq!(payload["mutation_contract"]["touches_network"], false);
    verify_eq!(payload["privacy"]["contains_raw_paths"], false);
    Ok(())
}

#[test]
fn html_renderer_is_deterministic_without_cli_state() -> TestResult {
    let source = dashboard_source();
    let dashboard = render_operations_dashboard_fixture("direct", Some(&source));
    verify_eq!(
        render_operations_dashboard_html(&dashboard),
        render_operations_dashboard_html(&dashboard)
    );
    Ok(())
}
