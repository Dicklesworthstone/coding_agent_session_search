use chrono::Utc;
use serde_json::{Value, json};
use std::fs;
use std::process::Command;
use std::thread;

pub const SCHEMA_VERSION: &str = "cass.swarm.resource_plan.v1";
pub const DEFAULT_TARGET_DIR: &str = "/data/tmp/cass-resource-plan-target";
pub const ACTIONS: &[&str] = &[
    "full-index",
    "semantic-backfill",
    "model-install",
    "html-export",
    "support-capsule",
    "release-verification",
];

#[derive(Debug, Clone)]
struct ResourceFacts {
    fixture_problem: Option<String>,
    cpu_count: Option<u64>,
    memory_total_mb: Option<u64>,
    memory_available_mb: Option<u64>,
    disk_available_mb: Option<u64>,
    db_size_mb: Option<u64>,
    message_count: Option<u64>,
    semantic_model_installed: Option<bool>,
    active_rebuild: Option<bool>,
    build_pressure: String,
    profile: Option<String>,
}

#[derive(Debug, Clone)]
struct ActionEstimate {
    action: &'static str,
    action_status: &'static str,
    estimated_work_units: u64,
    peak_memory_mb: u64,
    disk_write_mb: u64,
    p50_seconds: u64,
    p95_seconds: u64,
    interactive_latency_risk: &'static str,
    recommended_action: &'static str,
    safer_time_window: &'static str,
    warnings: Vec<&'static str>,
}

#[must_use]
pub fn render_resource_plan_live(action_filter: Option<&str>) -> Value {
    render_payload("live", "live", live_facts(), action_filter)
}

#[must_use]
pub fn render_resource_plan_fixture(
    fixture_id: &str,
    source: Option<&Value>,
    action_filter: Option<&str>,
) -> Value {
    render_payload(fixture_id, "fixture", fixture_facts(source), action_filter)
}

pub fn validate_action_filter(action: Option<&str>) -> Result<Option<String>, String> {
    let Some(action) = action else {
        return Ok(None);
    };
    let normalized = action.trim().to_ascii_lowercase().replace('_', "-");
    if ACTIONS.contains(&normalized.as_str()) {
        Ok(Some(normalized))
    } else {
        Err(format!(
            "unknown resource-plan action `{action}`; expected one of {}",
            ACTIONS.join(", ")
        ))
    }
}

fn render_payload(
    fixture_id: &str,
    source_kind: &str,
    facts: ResourceFacts,
    action_filter: Option<&str>,
) -> Value {
    let selected_actions = selected_actions(action_filter);
    let plans = selected_actions
        .iter()
        .map(|action| estimate_action(action, &facts))
        .collect::<Vec<_>>();
    let summary = summarize(&facts, &plans);
    let status = summary
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("partial");

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "generated_at": Utc::now().to_rfc3339(),
            "source": source_kind,
            "fixture_id": fixture_id,
            "contract": "read-only resource what-if planner"
        },
        "summary": summary,
        "inputs": render_inputs(&facts),
        "plans": plans.iter().map(render_plan).collect::<Vec<_>>(),
        "concurrency_caps": concurrency_caps(&facts),
        "offload": {
            "recommended": plans.iter().any(|plan| plan.interactive_latency_risk != "low"),
            "target_dir": DEFAULT_TARGET_DIR,
            "command_prefix": format!("rch exec -- env CARGO_TARGET_DIR={DEFAULT_TARGET_DIR}"),
            "reason": "use a dedicated target dir per agent and keep expensive work off the interactive pane"
        },
        "guided_workflow": {
            "surface": "cass swarm resource-plan --json",
            "bead_id": "coding_agent_session_search-guided-ops-repro-trust-5u82n.11",
            "apply_mode_available": false,
            "next_step": summary.get("recommended_action").cloned().unwrap_or_else(|| json!("inspect-health-status"))
        },
        "mutation_contract": {
            "read_only": true,
            "schedules_work": false,
            "apply_mode": false,
            "mutates_files": false,
            "mutates_db": false,
            "runs_builds": false,
            "touches_network": false
        },
        "privacy": {
            "contains_session_content": false,
            "contains_secrets": false,
            "redaction_applied": false
        }
    })
}

fn selected_actions(action_filter: Option<&str>) -> Vec<&'static str> {
    match action_filter {
        Some(action) => ACTIONS
            .iter()
            .copied()
            .filter(|candidate| *candidate == action)
            .collect(),
        None => ACTIONS.to_vec(),
    }
}

fn render_inputs(facts: &ResourceFacts) -> Value {
    json!({
        "host": {
            "profile": host_profile(facts),
            "cpu_count": facts.cpu_count,
            "memory_total_mb": facts.memory_total_mb,
            "memory_available_mb": facts.memory_available_mb,
            "disk_available_mb": facts.disk_available_mb
        },
        "cass": {
            "db_size_mb": facts.db_size_mb,
            "message_count": facts.message_count,
            "semantic_model_installed": facts.semantic_model_installed,
            "active_rebuild": facts.active_rebuild
        },
        "build_pressure": {
            "level": facts.build_pressure
        },
        "fixture_problem": facts.fixture_problem
    })
}

fn render_plan(plan: &ActionEstimate) -> Value {
    json!({
        "action": plan.action,
        "action_status": plan.action_status,
        "estimated_work_units": plan.estimated_work_units,
        "peak_memory_mb": plan.peak_memory_mb,
        "disk_write_mb": plan.disk_write_mb,
        "estimated_duration": {
            "p50_seconds": plan.p50_seconds,
            "p95_seconds": plan.p95_seconds,
            "confidence": if plan.action_status == "estimated" { "medium" } else { "low" }
        },
        "interactive_latency_risk": plan.interactive_latency_risk,
        "recommended_action": plan.recommended_action,
        "safer_time_window": plan.safer_time_window,
        "warnings": plan.warnings
    })
}

fn summarize(facts: &ResourceFacts, plans: &[ActionEstimate]) -> Value {
    let blocked_count = plans
        .iter()
        .filter(|plan| plan.action_status == "blocked")
        .count();
    let high_risk_count = plans
        .iter()
        .filter(|plan| plan.interactive_latency_risk == "high")
        .count();
    let warning_count = plans.iter().map(|plan| plan.warnings.len()).sum::<usize>();
    let partial_count = usize::from(facts.fixture_problem.is_some())
        + usize::from(facts.cpu_count.is_none())
        + usize::from(facts.memory_available_mb.is_none())
        + usize::from(facts.disk_available_mb.is_none())
        + usize::from(facts.db_size_mb.is_none())
        + usize::from(facts.message_count.is_none());
    let status = if blocked_count > 0 || high_risk_count > 0 {
        "warning"
    } else if partial_count > 0 {
        "partial"
    } else {
        "ok"
    };
    let recommended_action = plans
        .iter()
        .find(|plan| plan.recommended_action != "proceed-with-offloaded-window")
        .map(|plan| plan.recommended_action)
        .unwrap_or(if partial_count > 0 {
            "inspect-health-status"
        } else {
            "proceed-with-offloaded-window"
        });

    json!({
        "status": status,
        "plan_count": plans.len(),
        "blocked_count": blocked_count,
        "high_risk_count": high_risk_count,
        "warning_count": warning_count,
        "partial_input_count": partial_count,
        "recommended_action": recommended_action,
        "host_profile": host_profile(facts),
        "readiness": if blocked_count > 0 { "blocked" } else if high_risk_count > 0 || warning_count > 0 { "review-required" } else { "ready" }
    })
}

fn estimate_action(action: &'static str, facts: &ResourceFacts) -> ActionEstimate {
    let cpu = facts.cpu_count.unwrap_or(2).max(1);
    let db_size_mb = facts.db_size_mb.unwrap_or(512);
    let message_count = facts.message_count.unwrap_or(100_000);
    let caps = concurrency_numbers(facts);
    let threads = caps.0.max(1);
    let work_units = work_units(action, db_size_mb, message_count);
    let peak_memory_mb = peak_memory_mb(action, db_size_mb, caps.0, caps.2);
    let disk_write_mb = disk_write_mb(action, db_size_mb, message_count);
    let duration_floor = match action {
        "model-install" => 45,
        "html-export" => 30,
        "support-capsule" => 20,
        _ => 60,
    };
    let p50_seconds = duration_floor.max((work_units * 3).div_ceil(threads));
    let p95_seconds = p50_seconds.saturating_mul(2).saturating_add(30);
    let mut warnings = Vec::new();

    if facts.fixture_problem.is_some() {
        warnings.push("fixture-source-missing");
    }
    if facts.db_size_mb.is_none() || facts.message_count.is_none() {
        warnings.push("workload-size-estimated");
    }
    if facts.disk_available_mb.is_none() {
        warnings.push("disk-availability-unknown");
    }
    if facts
        .disk_available_mb
        .is_some_and(|available| available < disk_write_mb.saturating_mul(2).max(1024))
    {
        warnings.push("low-disk-headroom");
    }
    if facts.active_rebuild == Some(true) {
        warnings.push("active-rebuild-in-progress");
    }
    if facts.build_pressure == "high" {
        warnings.push("high-build-pressure");
    }
    if action == "semantic-backfill" && facts.semantic_model_installed == Some(false) {
        warnings.push("semantic-model-absent");
    }
    if action == "model-install" {
        warnings.push("operator-opt-in-network-step");
    }

    let action_status = if warnings.contains(&"low-disk-headroom")
        || warnings.contains(&"active-rebuild-in-progress")
        || warnings.contains(&"semantic-model-absent")
    {
        "blocked"
    } else if warnings.is_empty() {
        "estimated"
    } else {
        "advisory"
    };
    let recommended_action = recommendation_for(&warnings);
    let safer_time_window = safer_time_window_for(&warnings);
    let interactive_latency_risk = latency_risk(action, cpu, p95_seconds, &warnings);

    ActionEstimate {
        action,
        action_status,
        estimated_work_units: work_units,
        peak_memory_mb,
        disk_write_mb,
        p50_seconds,
        p95_seconds,
        interactive_latency_risk,
        recommended_action,
        safer_time_window,
        warnings,
    }
}

fn work_units(action: &str, db_size_mb: u64, message_count: u64) -> u64 {
    match action {
        "full-index" => (message_count / 1_000)
            .saturating_add(db_size_mb / 8)
            .max(100),
        "semantic-backfill" => (message_count / 500)
            .saturating_add(db_size_mb / 16)
            .max(120),
        "model-install" => 25,
        "html-export" => (message_count / 10_000)
            .saturating_add(db_size_mb / 64)
            .max(10),
        "support-capsule" => (message_count / 20_000)
            .saturating_add(db_size_mb / 128)
            .max(8),
        "release-verification" => db_size_mb.saturating_div(4).saturating_add(300),
        _ => 100,
    }
}

fn peak_memory_mb(action: &str, db_size_mb: u64, index_threads: u64, semantic_batches: u64) -> u64 {
    match action {
        "full-index" => 512 + (db_size_mb / 4).min(8_192) + index_threads.saturating_mul(96),
        "semantic-backfill" => 1_536 + semantic_batches.max(1).saturating_mul(384),
        "model-install" => 768,
        "html-export" => 384 + (db_size_mb / 8).min(4_096),
        "support-capsule" => 512 + (db_size_mb / 16).min(2_048),
        "release-verification" => 2_048 + index_threads.saturating_mul(128),
        _ => 512,
    }
}

fn disk_write_mb(action: &str, db_size_mb: u64, message_count: u64) -> u64 {
    match action {
        "full-index" => (db_size_mb / 2).max(256),
        "semantic-backfill" => (message_count / 500)
            .saturating_add(db_size_mb / 8)
            .max(512),
        "model-install" => 120,
        "html-export" => (message_count / 1_000).max(20),
        "support-capsule" => (db_size_mb / 20).max(50),
        "release-verification" => 256,
        _ => 128,
    }
}

fn recommendation_for(warnings: &[&str]) -> &'static str {
    if warnings.contains(&"low-disk-headroom") {
        "free-disk-before-action"
    } else if warnings.contains(&"active-rebuild-in-progress") {
        "wait-for-active-rebuild"
    } else if warnings.contains(&"semantic-model-absent") {
        "install-model-first"
    } else if warnings.contains(&"high-build-pressure") {
        "defer-expensive-action"
    } else if warnings.iter().any(|warning| warning.ends_with("-unknown"))
        || warnings.contains(&"workload-size-estimated")
        || warnings.contains(&"fixture-source-missing")
    {
        "inspect-health-status"
    } else {
        "proceed-with-offloaded-window"
    }
}

fn safer_time_window_for(warnings: &[&str]) -> &'static str {
    if warnings.contains(&"low-disk-headroom") {
        "after-disk-recovery"
    } else if warnings.contains(&"active-rebuild-in-progress") {
        "after-active-rebuild"
    } else if warnings.contains(&"semantic-model-absent") {
        "after-model-install"
    } else if warnings.contains(&"high-build-pressure") {
        "when-build-pressure-drops"
    } else {
        "now"
    }
}

fn latency_risk(action: &str, cpu: u64, p95_seconds: u64, warnings: &[&str]) -> &'static str {
    if warnings.contains(&"low-disk-headroom")
        || warnings.contains(&"active-rebuild-in-progress")
        || warnings.contains(&"high-build-pressure")
    {
        return "high";
    }
    match action {
        "full-index" | "semantic-backfill" | "release-verification" if p95_seconds > 1_800 => {
            "high"
        }
        "full-index" | "semantic-backfill" | "release-verification" => "medium",
        "html-export" | "support-capsule" if cpu <= 4 || p95_seconds > 180 => "medium",
        _ => "low",
    }
}

fn concurrency_caps(facts: &ResourceFacts) -> Value {
    let (index_threads, writer_threads, semantic_batches) = concurrency_numbers(facts);
    json!({
        "max_index_threads": index_threads,
        "max_writer_threads": writer_threads,
        "max_semantic_batches": semantic_batches,
        "policy": "advisory caps only; cass does not schedule background work from this command"
    })
}

fn concurrency_numbers(facts: &ResourceFacts) -> (u64, u64, u64) {
    let cpu = facts.cpu_count.unwrap_or(2).max(1);
    let pressure_divisor = match facts.build_pressure.as_str() {
        "high" => 8,
        "medium" => 4,
        "low" => 2,
        _ => 4,
    };
    let profile_cap = match host_profile(facts) {
        "many-core" => 16,
        "laptop" => 4,
        _ => 8,
    };
    let index_threads = (cpu / pressure_divisor).max(1).min(profile_cap);
    let writer_threads = index_threads.clamp(1, 4);
    let semantic_batches = if facts.semantic_model_installed == Some(false) {
        0
    } else {
        (index_threads / 2).clamp(1, 8)
    };
    (index_threads, writer_threads, semantic_batches)
}

fn host_profile(facts: &ResourceFacts) -> &'static str {
    if let Some(profile) = facts.profile.as_deref() {
        return match profile {
            "many-core" => "many-core",
            "laptop" => "laptop",
            "standard" => "standard",
            _ => "standard",
        };
    }
    match facts.cpu_count.unwrap_or(0) {
        32.. => "many-core",
        1..=8 => "laptop",
        _ => "standard",
    }
}

fn fixture_facts(source: Option<&Value>) -> ResourceFacts {
    let Some(source) = source else {
        return ResourceFacts {
            fixture_problem: Some("resource_plan fixture source is missing".to_string()),
            cpu_count: None,
            memory_total_mb: None,
            memory_available_mb: None,
            disk_available_mb: None,
            db_size_mb: None,
            message_count: None,
            semantic_model_installed: None,
            active_rebuild: None,
            build_pressure: "unknown".to_string(),
            profile: None,
        };
    };

    ResourceFacts {
        fixture_problem: None,
        cpu_count: value_u64(source, &["host", "cpu_count"]),
        memory_total_mb: value_u64(source, &["host", "memory_total_mb"]),
        memory_available_mb: value_u64(source, &["host", "memory_available_mb"]),
        disk_available_mb: value_u64(source, &["host", "disk_available_mb"]),
        db_size_mb: value_u64(source, &["cass", "db_size_mb"]),
        message_count: value_u64(source, &["cass", "message_count"]),
        semantic_model_installed: value_bool(source, &["cass", "semantic_model_installed"]),
        active_rebuild: value_bool(source, &["cass", "active_rebuild"]),
        build_pressure: value_str(source, &["build_pressure", "level"])
            .unwrap_or("unknown")
            .to_string(),
        profile: value_str(source, &["host", "profile"]).map(str::to_string),
    }
}

fn live_facts() -> ResourceFacts {
    let (memory_total_mb, memory_available_mb) = read_linux_memory_mb().unwrap_or((None, None));
    ResourceFacts {
        fixture_problem: None,
        cpu_count: thread::available_parallelism()
            .ok()
            .and_then(|count| u64::try_from(count.get()).ok()),
        memory_total_mb,
        memory_available_mb,
        disk_available_mb: read_disk_available_mb(),
        db_size_mb: None,
        message_count: None,
        semantic_model_installed: None,
        active_rebuild: None,
        build_pressure: "unknown".to_string(),
        profile: None,
    }
}

fn value_u64(value: &Value, path: &[&str]) -> Option<u64> {
    get_value(value, path).and_then(Value::as_u64)
}

fn value_bool(value: &Value, path: &[&str]) -> Option<bool> {
    get_value(value, path).and_then(Value::as_bool)
}

fn value_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    get_value(value, path).and_then(Value::as_str)
}

fn get_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn read_linux_memory_mb() -> Option<(Option<u64>, Option<u64>)> {
    let body = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("MemTotal:") {
            total = parse_meminfo_mb(value);
        } else if let Some(value) = line.strip_prefix("MemAvailable:") {
            available = parse_meminfo_mb(value);
        }
    }
    Some((total, available))
}

fn parse_meminfo_mb(value: &str) -> Option<u64> {
    let kb = value.split_whitespace().next()?.parse::<u64>().ok()?;
    Some(kb / 1024)
}

fn read_disk_available_mb() -> Option<u64> {
    let output = Command::new("df").args(["-Pk", "."]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout.lines().nth(1)?;
    let available_kb = line.split_whitespace().nth(3)?.parse::<u64>().ok()?;
    Some(available_kb / 1024)
}
