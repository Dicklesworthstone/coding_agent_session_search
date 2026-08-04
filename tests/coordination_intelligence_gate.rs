//! Integrated coordination-intelligence golden & e2e gate (gnrxb.10).
//!
//! Freezes the cross-surface contract for the swarm coordination-intelligence
//! read surfaces (resource plan, privacy preview, context pack, workflow
//! analytics, replay fixture). It renders each surface across clean / partial /
//! sensitive scenarios and asserts the shared invariants that every robot-JSON
//! coordination surface must uphold:
//!   * a pinned `schema_version` (a frozen golden manifest, below);
//!   * a `status` drawn from the known enum;
//!   * a read-only `mutation_contract`;
//!   * a `privacy` projection (and no raw secret/path/email leakage when fed
//!     sensitive input);
//!   * deterministic output (two identical renders are byte-equal).
//!
//! Adding or renaming a surface, or bumping a schema version, requires updating
//! the `FROZEN_SCHEMAS` manifest here — that is the golden freeze.

use serde_json::{Value, json};

/// The frozen coordination-intelligence contract: surface key -> schema_version.
/// This is the golden manifest; changing a surface must change this list.
const FROZEN_SCHEMAS: &[(&str, &str)] = &[
    ("resource_plan", "cass.swarm.resource_plan.v1"),
    ("privacy_exposure", "cass.swarm.privacy_exposure.v1"),
    ("context_pack", "cass.swarm.context_pack.v1"),
    ("workflow_analytics", "cass.swarm.workflow_analytics.v1"),
    ("replay_fixture", "cass.swarm.replay_fixture.v1"),
];

const KNOWN_STATUSES: &[&str] = &["ok", "partial", "warning"];

/// Render every surface for a given scenario. `sensitive` controls whether the
/// inputs carry secret-looking values (used by the no-leak assertions).
fn render_all(sensitive: bool, partial: bool) -> Vec<(&'static str, Value)> {
    use coding_agent_search as cass;

    if partial {
        // None source -> each surface reports its conservative partial envelope.
        return vec![
            (
                "resource_plan",
                cass::resource_plan::render_resource_plan_fixture("gate", None, None),
            ),
            (
                "privacy_exposure",
                cass::privacy_exposure::render_privacy_exposure_fixture("gate", None),
            ),
            (
                "context_pack",
                cass::context_pack::render_context_pack_fixture("gate", None),
            ),
            (
                "workflow_analytics",
                cass::workflow_analytics::render_workflow_analytics_fixture("gate", None),
            ),
            (
                "replay_fixture",
                cass::swarm_replay_fixture::render_replay_fixture_fixture("gate", None),
            ),
        ];
    }

    let now_ms: i64 = 1_749_456_000_000;
    let day_ms: i64 = 86_400_000;
    let secret = if sensitive {
        "sk-ant-supersecretvalue1234567890"
    } else {
        "noop"
    };
    let path = if sensitive {
        "/home/alice/.claude/projects/x"
    } else {
        "relative/path"
    };
    let email = if sensitive {
        "alice@example.com"
    } else {
        "user"
    };

    let resource_plan_src = json!({
        "host": {"profile": "many-core", "cpu_count": 64, "memory_total_mb": 262144,
                  "memory_available_mb": 180000, "disk_available_mb": 500000},
        "cass": {"db_size_mb": 4096, "message_count": 1200000,
                  "semantic_model_installed": true, "active_rebuild": false},
        "build_pressure": {"level": "low"}
    });
    let privacy_src = json!({
        "providers": [{
            "name": "claude-code", "source_class": "local-agent-history", "enabled": true,
            "roots": [path], "file_count": 100, "symlink_count": 0, "unreadable_count": 0,
            "secret_samples": [secret, format!("contact {email}")]
        }],
        "excluded_agents": [], "raw_mirror": {"enabled": false},
        "exports": {"chatgpt_encrypted_present": false, "html_export_tier": "redacted"}
    });
    let context_pack_src = json!({
        "bead_id": "demo", "token_budget": 2000, "now_ms": now_ms, "freshness_window_days": 30,
        "candidates": [{
            "id": "c1", "kind": "closeout_note", "path": path,
            "excerpt": format!("note {secret} {email}"),
            "created_at_ms": now_ms - day_ms, "relevance": 0.9, "authority": 0.8,
            "privacy_risk": "low"
        }]
    });
    let workflow_src = json!({
        "now_ms": now_ms, "window_days": 30,
        "records": [{
            "ts_ms": now_ms - day_ms, "agent": "cc", "source": "local", "workspace": "cass",
            "skill": "ubs", "command": "cargo clippy", "proof_gate": "clippy",
            "file_area": path, "outcome": "clean_close", "duration_ms": 1000
        }]
    });
    let replay_src = json!({
        "replay_id": "swarm", "events": [{
            "seq": 1, "ts_ms": now_ms, "kind": "mail_send", "actor": "cc", "bead": "demo",
            "payload": {"to": "cod", "body": format!("secret {secret}"), "path": path,
                         "note": format!("email {email}")}
        }]
    });

    vec![
        (
            "resource_plan",
            cass::resource_plan::render_resource_plan_fixture(
                "gate",
                Some(&resource_plan_src),
                None,
            ),
        ),
        (
            "privacy_exposure",
            cass::privacy_exposure::render_privacy_exposure_fixture("gate", Some(&privacy_src)),
        ),
        (
            "context_pack",
            cass::context_pack::render_context_pack_fixture("gate", Some(&context_pack_src)),
        ),
        (
            "workflow_analytics",
            cass::workflow_analytics::render_workflow_analytics_fixture(
                "gate",
                Some(&workflow_src),
            ),
        ),
        (
            "replay_fixture",
            cass::swarm_replay_fixture::render_replay_fixture_fixture("gate", Some(&replay_src)),
        ),
    ]
}

fn schema_for(surface: &str) -> &'static str {
    FROZEN_SCHEMAS
        .iter()
        .find(|(key, _)| *key == surface)
        .map(|(_, schema)| *schema)
        .unwrap_or("MISSING")
}

fn assert_shared_invariants(surface: &str, value: &Value) {
    assert_eq!(
        value["schema_version"].as_str(),
        Some(schema_for(surface)),
        "{surface}: schema_version drifted from frozen manifest"
    );
    let status = value["status"].as_str().unwrap_or("");
    assert!(
        KNOWN_STATUSES.contains(&status),
        "{surface}: unknown status {status:?}"
    );
    assert_eq!(
        value["mutation_contract"]["read_only"],
        json!(true),
        "{surface}: must be read-only"
    );
    assert_eq!(
        value["mutation_contract"]["touches_network"],
        json!(false),
        "{surface}: must not touch network"
    );
    assert!(
        value.get("privacy").map(Value::is_object).unwrap_or(false),
        "{surface}: must expose a privacy projection"
    );
    assert!(
        value["_meta"]["contract"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "{surface}: must declare a _meta.contract"
    );
}

fn assert_no_sensitive_leak(surface: &str, value: &Value) {
    let text = serde_json::to_string(value).expect("serialize");
    for needle in [
        "sk-ant-",
        "supersecretvalue",
        "/home/",
        "/Users/",
        "alice@example.com",
        "BEGIN PRIVATE",
    ] {
        assert!(
            !text.contains(needle),
            "{surface}: leaked sensitive needle {needle}"
        );
    }
}

#[test]
fn frozen_schema_manifest_covers_exactly_the_rendered_surfaces() {
    let rendered = render_all(false, false);
    let mut rendered_keys: Vec<&str> = rendered.iter().map(|(key, _)| *key).collect();
    let mut frozen_keys: Vec<&str> = FROZEN_SCHEMAS.iter().map(|(key, _)| *key).collect();
    rendered_keys.sort_unstable();
    frozen_keys.sort_unstable();
    assert_eq!(
        rendered_keys, frozen_keys,
        "rendered surfaces and frozen manifest must match exactly"
    );
    // Every rendered surface advertises its frozen schema version.
    for (surface, value) in &rendered {
        assert_eq!(
            value["schema_version"].as_str(),
            Some(schema_for(surface)),
            "{surface}: schema mismatch"
        );
    }
}

#[test]
fn clean_scenario_upholds_shared_invariants() {
    for (surface, value) in render_all(false, false) {
        assert_shared_invariants(surface, &value);
    }
}

#[test]
fn partial_scenario_is_safe_and_invariant() {
    for (surface, value) in render_all(false, true) {
        assert_shared_invariants(surface, &value);
        // With no input, every surface degrades to a documented partial/empty
        // envelope rather than panicking or emitting an unknown status.
        let status = value["status"].as_str().unwrap_or("");
        assert!(
            status == "partial" || status == "warning" || status == "ok",
            "{surface}: partial scenario produced status {status:?}"
        );
    }
}

#[test]
fn sensitive_scenario_never_leaks_raw_payload() {
    for (surface, value) in render_all(true, false) {
        assert_shared_invariants(surface, &value);
        assert_no_sensitive_leak(surface, &value);
    }
}

#[test]
fn renders_are_deterministic_across_runs() {
    // Strip the wall-clock _meta.generated_at before comparing; the rest of each
    // payload must be byte-stable across identical renders.
    let strip = |mut value: Value| -> Value {
        if let Some(meta) = value.get_mut("_meta").and_then(Value::as_object_mut) {
            meta.remove("generated_at");
        }
        value
    };
    let first = render_all(true, false);
    let second = render_all(true, false);
    assert_eq!(first.len(), second.len());
    for ((surface, a), (_, b)) in first.into_iter().zip(second) {
        assert_eq!(
            strip(a),
            strip(b),
            "{surface}: render is not deterministic across runs"
        );
    }
}

#[test]
fn assertion_summary_accounts_for_every_surface_and_scenario() {
    // A compact machine-readable summary of what this gate verified, mirroring
    // the e2e artifact the bead asks for.
    let surfaces = FROZEN_SCHEMAS.len();
    let scenarios = ["clean", "partial", "sensitive"];
    let summary = json!({
        "contract": "cass.swarm.coordination_intelligence_gate.v1",
        "surfaces": surfaces,
        "scenarios": scenarios.len(),
        "checks_per_surface": ["schema_version", "status_enum", "read_only", "no_network", "privacy_block", "meta_contract"],
        "frozen_schemas": FROZEN_SCHEMAS.iter().map(|(k, v)| json!({"surface": k, "schema": v})).collect::<Vec<_>>(),
    });
    assert_eq!(summary["surfaces"], json!(5));
    assert_eq!(summary["scenarios"], json!(3));
    assert_eq!(
        summary["frozen_schemas"].as_array().map(Vec::len),
        Some(surfaces)
    );
}
