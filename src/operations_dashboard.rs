//! Local, read-only operations dashboard for guided workflows.
//!
//! The dashboard is deliberately a projection over existing robot contracts.
//! It does not call providers, scan session content, write a report, or execute
//! a suggested command. Callers supply the JSON emitted by `cass guide`,
//! `cass swarm macros`, `resource-plan`, `privacy-preview`, `repro-capsule`,
//! and search. The renderer keeps only scan-friendly metadata and produces a
//! self-contained HTML document with no script or network dependency.

use crate::pages::redact::redact_swarm_text;
use serde_json::{Value, json};

/// Stable schema identifier for the normalized dashboard model.
pub const SCHEMA_VERSION: &str = "cass.swarm.operations_dashboard.v1";

/// The JSON surfaces whose contracts feed this human-facing projection.
pub const UNDERLYING_JSON_SURFACES: &[&str] = &[
    "cass guide <intent> --json",
    "cass swarm macros --json",
    "cass swarm resource-plan --json",
    "cass swarm privacy-preview --json",
    "cass swarm repro-capsule --json",
    "cass search <query> --robot --robot-meta",
    "cass swarm evidence --json",
];

/// Render a deterministic dashboard from fixture-provided child payloads.
///
/// Expected source keys are `guide`, `workflow_macros`, `resource_plan`,
/// `privacy_exposure`, `repro_capsules`, and `search_results`. Every key is
/// optional. `next_proof_command` may override the conservative evidence
/// default, but only when it is an explicitly robot-safe command.
#[must_use]
pub fn render_operations_dashboard_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    render_dashboard(fixture_id, "fixture", source)
}

/// Render the empty live shell. Live provider collection stays in command
/// dispatch so this pure module cannot accidentally acquire a mutation path.
#[must_use]
pub fn render_operations_dashboard_live() -> Value {
    let source = json!({
        "guide": crate::guide_planner::render_guide_catalog("live", "live"),
        "workflow_macros": crate::workflow_macros::render_workflow_macros_live(),
        "resource_plan": crate::resource_plan::render_resource_plan_live(None),
        "privacy_exposure": crate::privacy_exposure::render_privacy_exposure_live(),
        "repro_capsules": [],
        "search_results": {"results": []}
    });
    render_dashboard("live", "live", Some(&source))
}

fn render_dashboard(fixture_id: &str, source_kind: &str, source: Option<&Value>) -> Value {
    let guide = source.and_then(|value| value.get("guide"));
    let macros = source.and_then(|value| value.get("workflow_macros"));
    let resource = source.and_then(|value| value.get("resource_plan"));
    let privacy = source.and_then(|value| value.get("privacy_exposure"));
    let capsules = source.and_then(|value| value.get("repro_capsules"));
    let search = source.and_then(|value| value.get("search_results"));

    let current_goal = source
        .and_then(|value| value.get("current_goal"))
        .and_then(Value::as_str)
        .or_else(|| {
            guide
                .and_then(|value| value.pointer("/intent/raw"))
                .and_then(Value::as_str)
        })
        .map(safe_text);
    let workflows = render_workflows(macros);
    let blocked_prerequisites = render_blocked_prerequisites(guide, &workflows);
    let trust_warnings = render_trust_warnings(search);
    let privacy_card = render_privacy_card(privacy);
    let resource_card = render_resource_card(resource);
    let repro_capsules = render_capsules(capsules);
    let next_proof = render_next_proof(source, guide);

    let section_count = [guide, macros, resource, privacy, capsules, search]
        .iter()
        .filter(|section| section.is_some())
        .count();
    let blocked_workflow_count = workflows
        .iter()
        .filter(|workflow| value_str(workflow, "readiness") == Some("blocked"))
        .count();
    let high_risk = privacy_card
        .as_ref()
        .is_some_and(|card| value_str(card, "status") == Some("warning"))
        || resource_card
            .as_ref()
            .is_some_and(|card| value_str(card, "status") == Some("warning"));
    let is_empty = source.is_none_or(|value| {
        value.as_object().is_none_or(|map| map.is_empty()) || section_count == 0
    });
    let status = if is_empty {
        "empty"
    } else if !blocked_prerequisites.is_empty()
        || blocked_workflow_count > 0
        || !trust_warnings.is_empty()
        || high_risk
    {
        "warning"
    } else if section_count < 6 {
        "partial"
    } else {
        "ok"
    };

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "source": source_kind,
            "fixture_id": safe_text(fixture_id),
            "contract": "local read-only projection of robot JSON"
        },
        "summary": {
            "current_goal": current_goal,
            "recommended_action": guide
                .and_then(|value| value.get("recommended_action"))
                .and_then(Value::as_str)
                .map(safe_text)
                .unwrap_or_else(|| if is_empty { "select-a-guided-workflow".to_string() } else { "review-dashboard-warnings".to_string() }),
            "workflow_count": workflows.len(),
            "blocked_workflow_count": blocked_workflow_count,
            "blocked_prerequisite_count": blocked_prerequisites.len(),
            "trust_warning_count": trust_warnings.len(),
            "repro_capsule_count": repro_capsules.len(),
            "available_section_count": section_count,
            "empty": is_empty
        },
        "cards": {
            "workflows": workflows,
            "blocked_prerequisites": blocked_prerequisites,
            "trust_warnings": trust_warnings,
            "privacy": privacy_card,
            "resources": resource_card,
            "recent_repro_capsules": repro_capsules,
            "next_proof": next_proof
        },
        "underlying_json_surfaces": UNDERLYING_JSON_SURFACES,
        "mutation_contract": {
            "read_only": true,
            "apply_mode": false,
            "schedules_work": false,
            "mutates_files": false,
            "mutates_db": false,
            "runs_builds": false,
            "touches_network": false
        },
        "privacy": {
            "contains_session_content": false,
            "contains_raw_secrets": false,
            "contains_raw_paths": false,
            "redaction_applied": true
        }
    })
}

fn render_workflows(macros: Option<&Value>) -> Vec<Value> {
    let mut workflows = macros
        .and_then(|value| value.get("macros"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|workflow| {
            let id = workflow.get("id").and_then(Value::as_str)?;
            let readiness = workflow
                .get("readiness")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let missing = workflow
                .get("missing_preflight_facts")
                .or_else(|| workflow.get("missing_prerequisites"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(safe_text)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(json!({
                "id": safe_identifier(id),
                "title": workflow.get("title").and_then(Value::as_str).map(safe_text),
                "readiness": safe_state(readiness),
                "privacy_tier": workflow.get("privacy_tier").and_then(Value::as_str).map(safe_state),
                "missing_prerequisites": missing,
                "inspect_command": format!("cass guide {} --json", safe_identifier(id))
            }))
        })
        .collect::<Vec<_>>();
    workflows.sort_by(|left, right| {
        value_str(left, "id")
            .unwrap_or_default()
            .cmp(value_str(right, "id").unwrap_or_default())
    });
    workflows
}

fn render_blocked_prerequisites(guide: Option<&Value>, workflows: &[Value]) -> Vec<Value> {
    let mut blocked = guide
        .and_then(|value| value.pointer("/plan/prerequisites"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|prerequisite| {
            let status = prerequisite.get("status").and_then(Value::as_str)?;
            matches!(status, "unmet" | "needs-confirmation").then(|| {
                json!({
                    "workflow": guide
                        .and_then(|value| value.pointer("/plan/macro_id"))
                        .and_then(Value::as_str)
                        .map(safe_identifier),
                    "fact": prerequisite.get("fact").and_then(Value::as_str).map(safe_text),
                    "status": safe_state(status)
                })
            })
        })
        .collect::<Vec<_>>();

    for workflow in workflows {
        if value_str(workflow, "readiness") != Some("blocked") {
            continue;
        }
        let workflow_id = value_str(workflow, "id").unwrap_or("unknown");
        let Some(missing) = workflow
            .get("missing_prerequisites")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for fact in missing.iter().filter_map(Value::as_str) {
            if !blocked.iter().any(|entry| {
                value_str(entry, "workflow") == Some(workflow_id)
                    && value_str(entry, "fact") == Some(fact)
            }) {
                blocked.push(json!({
                    "workflow": workflow_id,
                    "fact": fact,
                    "status": "unmet"
                }));
            }
        }
    }
    blocked
}

fn render_trust_warnings(search: Option<&Value>) -> Vec<Value> {
    let assessments = search
        .and_then(|value| value.get("results").or_else(|| value.get("hits")))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| {
            result
                .get("trust")
                .or_else(|| result.get("trust_assessment"))
        })
        .chain(
            search
                .and_then(|value| value.get("trust_assessments"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    let mut warnings = assessments
        .filter_map(|assessment| {
            let tier = assessment.get("trust_tier").and_then(Value::as_str)?;
            (tier != "trusted").then(|| {
                json!({
                    "trust_tier": safe_state(tier),
                    "confidence": assessment.get("confidence").and_then(Value::as_str).map(safe_state),
                    "reason": assessment.get("stale_reason").and_then(Value::as_str).map(safe_text),
                    "recommended_followup": assessment.get("recommended_followup").and_then(Value::as_str).map(safe_text),
                    "provenance_refs": assessment.get("provenance_refs").and_then(Value::as_array).map(|refs| {
                        refs.iter().filter_map(Value::as_str).map(safe_identifier).collect::<Vec<_>>()
                    }).unwrap_or_default()
                })
            })
        })
        .collect::<Vec<_>>();
    warnings.sort_by(|left, right| {
        value_str(left, "trust_tier")
            .unwrap_or_default()
            .cmp(value_str(right, "trust_tier").unwrap_or_default())
            .then_with(|| {
                value_str(left, "reason")
                    .unwrap_or_default()
                    .cmp(value_str(right, "reason").unwrap_or_default())
            })
    });
    warnings
}

fn render_privacy_card(privacy: Option<&Value>) -> Option<Value> {
    let privacy = privacy?;
    Some(json!({
        "status": privacy.get("status").and_then(Value::as_str).map(safe_state).unwrap_or_else(|| "partial".to_string()),
        "readiness": privacy.pointer("/summary/readiness").and_then(Value::as_str).map(safe_state),
        "recommended_action": privacy.pointer("/summary/recommended_action").and_then(Value::as_str).map(safe_text),
        "risk_categories": privacy.get("risk_categories").and_then(Value::as_array).map(|risks| {
            risks.iter().filter_map(|risk| {
                Some(json!({
                    "category": safe_text(risk.get("category")?.as_str()?),
                    "severity": risk.get("severity").and_then(Value::as_str).map(safe_state),
                    "count": risk.get("count").and_then(Value::as_u64),
                    "blocking": risk.get("blocking").and_then(Value::as_bool).unwrap_or(false)
                }))
            }).collect::<Vec<_>>()
        }).unwrap_or_default(),
        "inspect_command": "cass swarm privacy-preview --json"
    }))
}

fn render_resource_card(resource: Option<&Value>) -> Option<Value> {
    let resource = resource?;
    Some(json!({
        "status": resource.get("status").and_then(Value::as_str).map(safe_state).unwrap_or_else(|| "partial".to_string()),
        "readiness": resource.pointer("/summary/readiness").and_then(Value::as_str).map(safe_state),
        "recommended_action": resource.pointer("/summary/recommended_action").and_then(Value::as_str).map(safe_text),
        "blocked_count": resource.pointer("/summary/blocked_count").and_then(Value::as_u64).unwrap_or(0),
        "high_risk_count": resource.pointer("/summary/high_risk_count").and_then(Value::as_u64).unwrap_or(0),
        "warning_count": resource.pointer("/summary/warning_count").and_then(Value::as_u64).unwrap_or(0),
        "inspect_command": "cass swarm resource-plan --json"
    }))
}

fn render_capsules(capsules: Option<&Value>) -> Vec<Value> {
    let mut rendered = capsules
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let capsule = entry.get("payload").unwrap_or(entry);
            let id = capsule.pointer("/manifest/capsule_id").and_then(Value::as_str)?;
            let local_ref = entry
                .get("local_ref")
                .and_then(Value::as_str)
                .filter(|path| safe_local_ref(path))
                .map(safe_text);
            Some(json!({
                "capsule_id": safe_identifier(id),
                "incident_kind": capsule.pointer("/manifest/incident_kind").and_then(Value::as_str).map(safe_state),
                "status": capsule.get("status").and_then(Value::as_str).map(safe_state),
                "local_ref": local_ref,
                "rerun_command": capsule.pointer("/rerun/command_template").and_then(Value::as_str).map(safe_text),
                "targets_live_data": capsule.pointer("/rerun/targets_live_data").and_then(Value::as_bool).unwrap_or(true)
            }))
        })
        .collect::<Vec<_>>();
    rendered.sort_by(|left, right| {
        value_str(left, "capsule_id")
            .unwrap_or_default()
            .cmp(value_str(right, "capsule_id").unwrap_or_default())
    });
    rendered
}

fn render_next_proof(source: Option<&Value>, guide: Option<&Value>) -> Value {
    let requested = source
        .and_then(|value| value.get("next_proof_command"))
        .and_then(Value::as_str)
        .filter(|command| is_robot_safe_command(command));
    let guide_gate = guide
        .and_then(|value| value.pointer("/plan/required_proof_gates/0"))
        .and_then(Value::as_str)
        .map(safe_text);
    json!({
        "command": requested.map(safe_text).unwrap_or_else(|| "cass swarm evidence --json".to_string()),
        "proof_gate": guide_gate,
        "reason": if requested.is_some() { "fixture-selected robot proof surface" } else { "conservative read-only evidence check" }
    })
}

fn is_robot_safe_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.contains([
        '\n', '\r', ';', '|', '&', '>', '<', '`', '$', '(', ')', '\'', '"', '\\',
    ]) {
        return false;
    }

    let tokens = trimmed.split_ascii_whitespace().collect::<Vec<_>>();
    if tokens.first() == Some(&"cass") {
        let allowed_surface = matches!(
            tokens.get(1).copied(),
            Some(
                "health"
                    | "status"
                    | "diag"
                    | "capabilities"
                    | "introspect"
                    | "api-version"
                    | "search"
                    | "robot-docs"
            )
        ) || tokens.get(1..3) == Some(&["doctor", "check"])
            || matches!(
                tokens.get(1..3),
                Some(
                    ["swarm", "evidence"]
                        | ["swarm", "status"]
                        | ["swarm", "proof-debt"]
                        | ["swarm", "lint"]
                )
            );
        let forbidden_flag = tokens.iter().any(|token| {
            matches!(
                *token,
                "--fix"
                    | "--apply"
                    | "--repair"
                    | "--cleanup"
                    | "--full"
                    | "--force"
                    | "--install"
                    | "--update"
            )
        });
        return allowed_surface
            && !forbidden_flag
            && tokens
                .iter()
                .any(|token| matches!(*token, "--json" | "--robot"));
    }

    if tokens.get(0..3) != Some(&["rch", "exec", "--"])
        || tokens.get(3) != Some(&"env")
        || !tokens.get(4).is_some_and(|token| {
            token
                .strip_prefix("CARGO_TARGET_DIR=/data/tmp/cass-")
                .is_some_and(|suffix| !suffix.is_empty())
        })
        || tokens.get(5) != Some(&"cargo")
        || !matches!(
            tokens.get(6).copied(),
            Some("test" | "check" | "clippy" | "bench" | "fmt")
        )
    {
        return false;
    }
    tokens.get(6) != Some(&"fmt") || tokens.contains(&"--check")
}

fn safe_local_ref(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with(['/', '\\'])
        && !path.contains(':')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}

fn safe_text(value: &str) -> String {
    redact_swarm_text(value)
}

fn safe_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
        })
        .take(160)
        .collect()
}

fn safe_state(value: &str) -> String {
    safe_identifier(&value.to_ascii_lowercase().replace('_', "-"))
}

fn value_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Render a complete offline HTML report from a normalized dashboard payload.
///
/// All dynamic text is HTML-escaped. The document contains no JavaScript,
/// external URL, form, or mutation control. Safe relative capsule refs are the
/// only links derived from fixture data.
#[must_use]
pub fn render_operations_dashboard_html(dashboard: &Value) -> String {
    let status = dashboard
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("partial");
    let summary = dashboard.get("summary").unwrap_or(&Value::Null);
    let cards = dashboard.get("cards").unwrap_or(&Value::Null);
    let goal = summary
        .get("current_goal")
        .and_then(Value::as_str)
        .unwrap_or("No current goal selected");
    let recommended = summary
        .get("recommended_action")
        .and_then(Value::as_str)
        .unwrap_or("inspect underlying JSON surfaces");

    let workflows = html_workflows(cards.get("workflows"));
    let blocked = html_blocked(cards.get("blocked_prerequisites"));
    let trust = html_trust(cards.get("trust_warnings"));
    let privacy = html_status_card("Privacy exposure", cards.get("privacy"));
    let resources = html_status_card("Resource what-if", cards.get("resources"));
    let capsules = html_capsules(cards.get("recent_repro_capsules"));
    let next_proof = cards
        .pointer("/next_proof/command")
        .and_then(Value::as_str)
        .unwrap_or("cass swarm evidence --json");
    let surfaces = UNDERLYING_JSON_SURFACES
        .iter()
        .map(|surface| format!("<li><code>{}</code></li>", html_escape(surface)))
        .collect::<String>();

    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n<title>CASS operations dashboard</title>\n<style>:root{{color-scheme:light dark;font:15px system-ui,sans-serif}}body{{max-width:1100px;margin:auto;padding:24px;line-height:1.45}}header,.card{{border:1px solid #7776;border-radius:10px;padding:16px;margin:0 0 16px}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:16px}}.grid .card{{margin:0}}.status{{font-weight:700;text-transform:uppercase}}.warning{{color:#d97706}}.ok{{color:#15803d}}.empty,.partial{{color:#64748b}}code{{overflow-wrap:anywhere}}ul{{padding-left:20px}}a{{color:inherit}}.muted{{opacity:.75}}</style>\n</head>\n<body>\n<header><h1>CASS operations dashboard</h1><p class=\"status {status}\">{status}</p><p><strong>Current goal:</strong> {goal}</p><p><strong>Recommended:</strong> {recommended}</p></header>\n<main class=\"grid\">\n{workflows}\n{blocked}\n{trust}\n{privacy}\n{resources}\n{capsules}\n<section class=\"card\"><h2>Next proof</h2><code>{next_proof}</code></section>\n<section class=\"card\"><h2>Robot JSON sources</h2><p class=\"muted\">Agents must consume these contracts, not parse this HTML.</p><ul>{surfaces}</ul></section>\n</main>\n</body>\n</html>\n",
        status = html_escape(status),
        goal = html_escape(goal),
        recommended = html_escape(recommended),
        next_proof = html_escape(next_proof),
    )
}

fn html_workflows(value: Option<&Value>) -> String {
    let items = value.and_then(Value::as_array);
    let body = items.filter(|items| !items.is_empty()).map_or_else(
        || "<p class=\"muted\">No recommended workflows available.</p>".to_string(),
        |items| {
            items
                .iter()
                .map(|item| {
                    format!(
                        "<li><strong>{}</strong> — {}<br><code>{}</code></li>",
                        html_escape(
                            value_str(item, "title")
                                .or_else(|| value_str(item, "id"))
                                .unwrap_or("unnamed")
                        ),
                        html_escape(value_str(item, "readiness").unwrap_or("unknown")),
                        html_escape(
                            value_str(item, "inspect_command").unwrap_or("cass guide --json")
                        )
                    )
                })
                .collect::<String>()
        },
    );
    format!("<section class=\"card\"><h2>Guided workflows</h2><ul>{body}</ul></section>")
}

fn html_blocked(value: Option<&Value>) -> String {
    let items = value.and_then(Value::as_array);
    let body = items.filter(|items| !items.is_empty()).map_or_else(
        || "<p class=\"muted\">No blocked prerequisites.</p>".to_string(),
        |items| {
            items
                .iter()
                .map(|item| {
                    format!(
                        "<li><code>{}</code> — {}</li>",
                        html_escape(value_str(item, "fact").unwrap_or("unknown")),
                        html_escape(value_str(item, "status").unwrap_or("unknown"))
                    )
                })
                .collect::<String>()
        },
    );
    format!("<section class=\"card\"><h2>Blocked prerequisites</h2><ul>{body}</ul></section>")
}

fn html_trust(value: Option<&Value>) -> String {
    let items = value.and_then(Value::as_array);
    let body = items.filter(|items| !items.is_empty()).map_or_else(
        || "<p class=\"muted\">No trust warnings.</p>".to_string(),
        |items| {
            items
                .iter()
                .map(|item| {
                    format!(
                        "<li><strong>{}</strong> — {}</li>",
                        html_escape(value_str(item, "trust_tier").unwrap_or("unverified")),
                        html_escape(value_str(item, "reason").unwrap_or("review provenance"))
                    )
                })
                .collect::<String>()
        },
    );
    format!("<section class=\"card\"><h2>Trust warnings</h2><ul>{body}</ul></section>")
}

fn html_status_card(title: &str, value: Option<&Value>) -> String {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return format!(
            "<section class=\"card\"><h2>{}</h2><p class=\"muted\">Not reported.</p></section>",
            html_escape(title)
        );
    };
    format!(
        "<section class=\"card\"><h2>{}</h2><p><strong>Status:</strong> {}</p><p><strong>Readiness:</strong> {}</p><p><strong>Next:</strong> {}</p><code>{}</code></section>",
        html_escape(title),
        html_escape(value_str(value, "status").unwrap_or("partial")),
        html_escape(value_str(value, "readiness").unwrap_or("unknown")),
        html_escape(value_str(value, "recommended_action").unwrap_or("inspect JSON")),
        html_escape(value_str(value, "inspect_command").unwrap_or("cass swarm status --json")),
    )
}

fn html_capsules(value: Option<&Value>) -> String {
    let items = value.and_then(Value::as_array);
    let body = items.filter(|items| !items.is_empty()).map_or_else(
        || "<p class=\"muted\">No recent repro capsules.</p>".to_string(),
        |items| {
            items
                .iter()
                .map(|item| {
                    let id = html_escape(value_str(item, "capsule_id").unwrap_or("unknown"));
                    if let Some(path) = value_str(item, "local_ref").filter(|path| safe_local_ref(path)) {
                        format!("<li><a href=\"{}\"><code>{id}</code></a><br><span class=\"muted\">{}</span></li>", html_escape(path), html_escape(path))
                    } else {
                        format!("<li><code>{id}</code></li>")
                    }
                })
                .collect::<String>()
        },
    );
    format!("<section class=\"card\"><h2>Recent repro capsules</h2><ul>{body}</ul></section>")
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    type TestResult = Result<(), Box<dyn Error>>;

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

    fn full_fixture() -> Value {
        json!({
            "current_goal": "prepare a trustworthy release",
            "guide": {
                "status": "ok",
                "intent": {"raw": "prepare-release"},
                "readiness": "ready",
                "recommended_action": "follow-plan-steps",
                "plan": {
                    "macro_id": "prepare-release",
                    "prerequisites": [{"fact": "version_bumped", "status": "met"}],
                    "required_proof_gates": ["gauntlet-green"]
                }
            },
            "workflow_macros": {"macros": [{
                "id": "prepare-release",
                "title": "Prepare and verify a release",
                "readiness": "ready",
                "privacy_tier": "low",
                "missing_preflight_facts": []
            }]},
            "resource_plan": {"status": "ok", "summary": {
                "readiness": "ready", "recommended_action": "proceed-with-offloaded-window",
                "blocked_count": 0, "high_risk_count": 0, "warning_count": 0
            }},
            "privacy_exposure": {"status": "ok", "summary": {
                "readiness": "ready", "recommended_action": "proceed-with-redaction"
            }, "risk_categories": []},
            "repro_capsules": [{
                "local_ref": "reports/capsule.json",
                "payload": {"status": "ok", "manifest": {
                    "capsule_id": "capsule-blake3:abc123", "incident_kind": "ci-failure"
                }, "rerun": {"targets_live_data": false, "command_template": "cass swarm repro-capsule --json --fixture repro-capsule.fixture.json"}}
            }],
            "search_results": {"results": [{"trust": {
                "trust_tier": "trusted", "confidence": "high",
                "provenance_refs": ["release:v0.6.22"]
            }}]},
            "next_proof_command": "rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-proof cargo test --all-targets"
        })
    }

    #[test]
    fn fixture_projection_is_deterministic_and_complete() -> TestResult {
        let fixture = full_fixture();
        let first = render_operations_dashboard_fixture("healthy", Some(&fixture));
        let second = render_operations_dashboard_fixture("healthy", Some(&fixture));
        verify_eq!(first, second);
        verify_eq!(first["status"], "ok");
        verify_eq!(first["summary"]["workflow_count"], 1);
        verify_eq!(first["summary"]["repro_capsule_count"], 1);
        verify_eq!(first["mutation_contract"]["read_only"], true);
        verify_eq!(first["mutation_contract"]["touches_network"], false);
        Ok(())
    }

    #[test]
    fn missing_optional_sections_are_explicitly_partial() -> TestResult {
        let fixture = json!({
            "guide": {
                "intent": {"raw": "fix-ci"},
                "recommended_action": "gather-preflight-facts-or-follow-plan"
            }
        });
        let dashboard = render_operations_dashboard_fixture("partial", Some(&fixture));
        verify_eq!(dashboard["status"], "partial");
        verify!(dashboard["cards"]["privacy"].is_null());
        verify!(dashboard["cards"]["resources"].is_null());
        let html = render_operations_dashboard_html(&dashboard);
        verify!(html.contains("Not reported."));
        verify!(html.contains("No recent repro capsules."));
        Ok(())
    }

    #[test]
    fn blocked_high_risk_state_is_scan_friendly() -> TestResult {
        let fixture = json!({
            "guide": {
                "intent": {"raw": "prepare-release"},
                "readiness": "blocked",
                "recommended_action": "satisfy-prerequisites-then-follow-plan",
                "plan": {"macro_id": "prepare-release", "prerequisites": [
                    {"fact": "version_bumped", "status": "unmet"}
                ], "required_proof_gates": ["gauntlet-green"]}
            },
            "workflow_macros": {"macros": [{
                "id": "prepare-release", "title": "Prepare release", "readiness": "blocked",
                "missing_preflight_facts": ["version_bumped"]
            }]},
            "resource_plan": {"status": "warning", "summary": {
                "readiness": "blocked", "recommended_action": "free-disk-before-action",
                "blocked_count": 1, "high_risk_count": 1, "warning_count": 3
            }},
            "privacy_exposure": {"status": "warning", "summary": {
                "readiness": "opt-in-required", "recommended_action": "review-required-opt-ins"
            }, "risk_categories": [{"category": "secrets-detected", "severity": "high", "count": 2}]},
            "repro_capsules": [],
            "search_results": {"results": [{"trust": {
                "trust_tier": "failed", "confidence": "high", "stale_reason": "failed_attempt",
                "recommended_followup": "use a landed result", "provenance_refs": ["bead:abc"]
            }}]}
        });
        let dashboard = render_operations_dashboard_fixture("blocked", Some(&fixture));
        verify_eq!(dashboard["status"], "warning");
        verify_eq!(dashboard["summary"]["blocked_prerequisite_count"], 1);
        verify_eq!(dashboard["summary"]["trust_warning_count"], 1);
        verify_eq!(dashboard["cards"]["resources"]["high_risk_count"], 1);
        let html = render_operations_dashboard_html(&dashboard);
        for expected in [
            "version_bumped",
            "failed_attempt",
            "free-disk-before-action",
        ] {
            verify!(html.contains(expected), "missing {expected}");
        }
        Ok(())
    }

    #[test]
    fn redacts_untrusted_text_and_escapes_html() -> TestResult {
        let secret = ["sk-ant-", "api03-", "AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH"].concat();
        let fixture = json!({
            "current_goal": format!("inspect /home/alice/private and {secret} <script>alert(1)</script>"),
            "guide": {"recommended_action": "review alice@example.com"},
            "search_results": {"trust_assessments": [{
                "trust_tier": "stale", "confidence": "medium", "stale_reason": "aged_out",
                "recommended_followup": "open /home/alice/private/session.jsonl",
                "provenance_refs": ["commit:abcdef123456"]
            }]}
        });
        let dashboard = render_operations_dashboard_fixture("redacted", Some(&fixture));
        let json = serde_json::to_string(&dashboard).unwrap_or_default();
        let html = render_operations_dashboard_html(&dashboard);
        for forbidden in [&secret, "/home/alice/private", "alice@example.com"] {
            verify!(!json.contains(forbidden), "JSON leaked {forbidden}");
            verify!(!html.contains(forbidden), "HTML leaked {forbidden}");
        }
        verify!(!html.contains("<script>"));
        verify!(html.contains("&lt;script&gt;") || html.contains("REDACTED"));
        Ok(())
    }

    #[test]
    fn long_safe_capsule_path_wraps_and_traversal_never_links() -> TestResult {
        let long_path = format!("reports/{}capsule.json", "nested/".repeat(40));
        let fixture = json!({
            "repro_capsules": [
                {"local_ref": long_path, "manifest": {"capsule_id": "capsule:long"}},
                {"local_ref": "../private/capsule.json", "manifest": {"capsule_id": "capsule:unsafe"}}
            ]
        });
        let dashboard = render_operations_dashboard_fixture("long-path", Some(&fixture));
        verify_eq!(
            dashboard["cards"]["recent_repro_capsules"][0]["local_ref"],
            long_path
        );
        verify!(dashboard["cards"]["recent_repro_capsules"][1]["local_ref"].is_null());
        let html = render_operations_dashboard_html(&dashboard);
        verify!(html.contains("overflow-wrap:anywhere"));
        verify!(html.contains(&format!("href=\"{long_path}\"")));
        verify!(!html.contains("href=\"../private"));
        Ok(())
    }

    #[test]
    fn empty_state_html_is_pinned_inline_snapshot() -> TestResult {
        let dashboard = render_operations_dashboard_fixture("empty", Some(&json!({})));
        verify_eq!(dashboard["status"], "empty");
        let html = render_operations_dashboard_html(&dashboard);
        let required_snapshot_lines = [
            "<!doctype html>",
            "<h1>CASS operations dashboard</h1>",
            "<p class=\"status empty\">empty</p>",
            "<strong>Current goal:</strong> No current goal selected",
            "No recommended workflows available.",
            "No blocked prerequisites.",
            "No trust warnings.",
            "No recent repro capsules.",
            "Agents must consume these contracts, not parse this HTML.",
            "<code>cass swarm evidence --json</code>",
        ];
        for line in required_snapshot_lines {
            verify!(html.contains(line), "empty snapshot missing: {line}");
        }
        verify!(!html.contains("<script"));
        verify!(!html.contains("https://"));
        verify!(!html.contains("<form"));
        Ok(())
    }

    #[test]
    fn unsafe_proof_override_falls_back_to_read_only_json_surface() -> TestResult {
        for unsafe_command in [
            "cass doctor --fix --json",
            "cass doctor repair --json",
            "cass index --full --json",
            "rch exec -- env CARGO_TARGET_DIR=/tmp/proof rm -f report --json",
            "rch exec -- env CARGO_TARGET_DIR=/tmp/proof cargo test; rm report --json",
            "rch exec -- env CARGO_TARGET_DIR=/tmp/proof cargo test && rm report --json",
            "rch exec -- env CARGO_TARGET_DIR=/tmp/proof cargo test | tee report --json",
            "rch exec -- env CARGO_TARGET_DIR=/tmp/proof cargo test > report --json",
            "rch exec -- env CARGO_TARGET_DIR=/tmp/proof cargo test\nrm report --json",
        ] {
            let fixture = json!({"next_proof_command": unsafe_command});
            let dashboard = render_operations_dashboard_fixture("unsafe-proof", Some(&fixture));
            verify_eq!(
                dashboard["cards"]["next_proof"]["command"],
                "cass swarm evidence --json",
                "unsafe command was accepted: {unsafe_command}"
            );
        }
        Ok(())
    }

    #[test]
    fn proof_override_accepts_only_pinned_read_only_shapes() -> TestResult {
        for safe_command in [
            "cass health --json",
            "cass swarm evidence --json",
            "cass doctor check --json",
            "rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-proof cargo test --all-targets",
            "rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-proof cargo fmt --check",
        ] {
            let fixture = json!({"next_proof_command": safe_command});
            let dashboard = render_operations_dashboard_fixture("safe-proof", Some(&fixture));
            verify_eq!(
                dashboard["cards"]["next_proof"]["command"],
                safe_command,
                "safe command was rejected: {safe_command}"
            );
        }
        Ok(())
    }
}
