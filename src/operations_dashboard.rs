//! Operations dashboard entry points: pure fixture projection plus explicit,
//! bounded live repository intake. The underlying workflow renderer remains
//! isolated from provider I/O, so fixture and HTML rendering never run Git.

mod projection;
mod repository;

use serde_json::{Value, json};

pub use projection::{SCHEMA_VERSION, UNDERLYING_JSON_SURFACES};

/// Render supplied robot payloads without consulting the local machine.
#[must_use]
pub fn render_operations_dashboard_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    let dashboard = projection::render_operations_dashboard_fixture(fixture_id, source);
    attach_repository(
        dashboard,
        repository::project(source.and_then(|value| value.get("repository"))),
    )
}

/// Collect only repository-local Git index/ref state and Beads metadata. No
/// build, suggested command, session query, or network request is executed.
#[must_use]
pub fn render_operations_dashboard_live() -> Value {
    let mut dashboard = projection::render_operations_dashboard_live();
    // The original catalog supplies empty placeholders for these two sources,
    // not live observations. Do not count them as measured/available sections.
    dashboard["_meta"]["unavailable_sections"] = json!(["repro_capsules", "search_results"]);
    dashboard["summary"]["available_section_count"] = json!(
        dashboard["summary"]["available_section_count"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(2)
    );
    dashboard["summary"]["observations_complete"] = json!(false);
    let source = match std::env::current_dir() {
        Ok(root) => repository::collect(&root),
        Err(_) => json!({
            "git": {"status": "unavailable", "error_kind": "current-directory-unavailable"},
            "beads": {"status": "unavailable", "error_kind": "current-directory-unavailable"}
        }),
    };
    if let Some(contract) = source.get("collection_contract") {
        dashboard["_meta"]["repository_collection"] = contract.clone();
    }
    attach_repository(dashboard, repository::project(Some(&source)))
}

fn attach_repository(mut dashboard: Value, repository: Option<Value>) -> Value {
    let Some(repository) = repository else {
        return dashboard;
    };
    let candidates = repository
        .pointer("/beads/locally_unblocked_count")
        .and_then(Value::as_u64);
    let is_warning = repository["status"] == "warning";
    let existing_warning = dashboard["status"] == "warning";
    dashboard["status"] = json!(if is_warning || existing_warning {
        "warning"
    } else {
        "partial"
    });
    dashboard["summary"]["available_section_count"] = json!(
        dashboard["summary"]["available_section_count"]
            .as_u64()
            .unwrap_or(0)
            + 1
    );
    dashboard["summary"]["empty"] = json!(false);
    dashboard["summary"]["observations_complete"] = json!(false);
    dashboard["summary"]["repository_candidate_count"] = json!(candidates);
    if dashboard["summary"]["current_goal"].is_null() && !existing_warning {
        dashboard["summary"]["recommended_action"] = json!(if is_warning {
            "review-repository-and-task-warnings"
        } else if candidates.is_some_and(|count| count > 0) {
            "inspect-local-candidates-and-confirm-reservations"
        } else {
            "review-local-provider-coverage"
        });
    }
    dashboard["cards"]["repository"] = repository;
    dashboard
}

/// Render already-collected data only. The repository card is inserted at the
/// end of the existing offline template's main region; user data is escaped
/// before insertion and cannot supply the structural closing tag.
#[must_use]
pub fn render_operations_dashboard_html(dashboard: &Value) -> String {
    let html = projection::render_operations_dashboard_html(dashboard);
    let Some(card) = dashboard.pointer("/cards/repository") else {
        return html;
    };
    let mut additional = repository::html(card);
    if let Some(unavailable) = dashboard
        .pointer("/_meta/unavailable_sections")
        .and_then(Value::as_array)
    {
        let names = unavailable
            .iter()
            .filter_map(Value::as_str)
            .take(16)
            .map(|name| {
                html_escape(
                    &crate::pages::redact::redact_swarm_text(name)
                        .chars()
                        .take(80)
                        .collect::<String>(),
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        additional.push_str(&format!("<section class=\"card\"><h2>Uncollected sources</h2><p class=\"muted\">{names}. Empty cards are not proof that these sources contain no records.</p></section>"));
    }
    html.replacen("</main>", &format!("{additional}\n</main>"), 1)
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

    fn repository_fixture() -> Value {
        json!({
            "git": {"status": "partial", "payload": {"staged_changes": true, "detached_head": false, "merge_in_progress": false}},
            "beads": {"status": "ok", "payload": {
                "complete": true, "counts": {"open": 2}, "locally_unblocked_count": 2,
                "candidate_tasks": [{"id": "task-1", "title": "Implement native search", "priority": 0, "status": "open", "readiness": "locally-unblocked"}]
            }}
        })
    }

    #[test]
    fn existing_fixture_contracts_are_unchanged_without_repository_input() {
        for source in [
            json!({}),
            json!({"guide": {"intent": {"raw": "fix-ci"}}}),
            json!({"next_proof_command": "cass health --json"}),
        ] {
            let expected = projection::render_operations_dashboard_fixture("same", Some(&source));
            let actual = render_operations_dashboard_fixture("same", Some(&source));
            assert_eq!(actual, expected);
            assert_eq!(
                render_operations_dashboard_html(&actual),
                projection::render_operations_dashboard_html(&expected)
            );
        }
    }

    #[test]
    fn repository_only_input_is_visible_but_never_claims_complete_coordination() {
        let source = json!({"repository": repository_fixture()});
        let dashboard = render_operations_dashboard_fixture("repository", Some(&source));
        assert_eq!(dashboard["status"], "partial");
        assert_eq!(dashboard["summary"]["empty"], false);
        assert_eq!(dashboard["summary"]["repository_candidate_count"], 2);
        assert_eq!(
            dashboard["summary"]["recommended_action"],
            "inspect-local-candidates-and-confirm-reservations"
        );
        assert_eq!(
            dashboard["cards"]["repository"]["coordination_verified"],
            false
        );
        let html = render_operations_dashboard_html(&dashboard);
        assert_eq!(html.matches("Repository and task cockpit").count(), 1);
        assert!(html.find("task-1").unwrap() < html.find("</main>").unwrap());
        assert_eq!(html.matches("</main>").count(), 1);
        assert!(!html.contains("<script"));
    }

    #[test]
    fn uncollected_sources_remain_explicit_in_the_offline_html() {
        let mut dashboard = render_operations_dashboard_fixture(
            "repository",
            Some(&json!({"repository": repository_fixture()})),
        );
        dashboard["_meta"]["unavailable_sections"] = json!(["repro_capsules", "search_results"]);
        let html = render_operations_dashboard_html(&dashboard);
        assert!(html.contains("Uncollected sources"));
        assert!(html.contains("Empty cards are not proof"));
    }

    #[test]
    fn merge_warning_cannot_be_overridden_by_locally_unblocked_tasks() {
        let mut source = repository_fixture();
        source["git"]["payload"]["merge_in_progress"] = json!(true);
        let dashboard =
            render_operations_dashboard_fixture("merge", Some(&json!({"repository": source})));
        assert_eq!(dashboard["status"], "warning");
        assert_eq!(
            dashboard["summary"]["recommended_action"],
            "review-repository-and-task-warnings"
        );
    }

    #[test]
    fn active_guided_goal_keeps_its_original_recommendation() {
        let source = json!({"repository": repository_fixture(), "current_goal": "recover safely", "guide": {"recommended_action": "preserve-archive-first"}});
        let dashboard = render_operations_dashboard_fixture("goal", Some(&source));
        assert_eq!(
            dashboard["summary"]["recommended_action"],
            "preserve-archive-first"
        );
    }
}
