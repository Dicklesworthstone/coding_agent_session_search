//! Resolve service agent scopes before opening readers or running retrieval.
//! Keep the CLI's GH #532 aliases, but derive supported identities from the
//! connector registry rather than probing this host or inventing another list.

use std::sync::OnceLock;

use serde::{Deserialize, Deserializer, de::Error as _};

use super::{MAX_FILTERS, MAX_IDENTITY_BYTES};

fn known_agents() -> &'static [&'static str] {
    static AGENTS: OnceLock<Vec<&'static str>> = OnceLock::new();
    AGENTS.get_or_init(|| {
        let mut agents: Vec<_> = coding_agent_search::connectors::get_connector_factories()
            .into_iter()
            .map(|(name, _)| match name {
                "claude" => "claude_code",
                other => other,
            })
            .collect();
        // Imported legacy conversations can carry the CLI's unknown identity.
        agents.push("unknown");
        agents.sort_unstable();
        agents.dedup();
        agents
    })
}

pub(super) fn normalize(agents: &[String]) -> Result<Vec<String>, String> {
    // Bound the RAW scope before aliases collapse, including the serde lane.
    // A list of repeated aliases must not bypass the 32-value admission limit.
    if agents.len() > MAX_FILTERS {
        return Err("at most 32 values are allowed in each filter".into());
    }
    if agents
        .iter()
        .any(|agent| agent.trim().is_empty() || agent.len() > MAX_IDENTITY_BYTES)
    {
        return Err("filter values must be nonempty and at most 4096 UTF-8 bytes".into());
    }
    if agents.is_empty() {
        return Ok(Vec::new());
    }
    let known = known_agents();
    let mut resolved = Vec::with_capacity(agents.len());
    for raw in agents {
        let key = raw.trim().to_ascii_lowercase().replace('-', "_");
        let candidate = match key.as_str() {
            "claude" => "claude_code",
            "oh_my_pi" | "ohmypi" => "omp",
            "open_claw" => "openclaw",
            other => other,
        };
        let Some(agent) = known
            .iter()
            .copied()
            .find(|slug| slug.replace('-', "_") == candidate)
        else {
            let suggestion = known
                .iter()
                .map(|slug| (*slug, strsim::levenshtein(candidate, slug)))
                .filter(|(_, distance)| *distance <= 3)
                .min_by_key(|(_, distance)| *distance)
                .map(|(slug, _)| format!("Did you mean '{slug}'? "))
                .unwrap_or_default();
            return Err(format!(
                "unknown agent filter {raw:?}. {suggestion}Known agents: {}. Run 'cass capabilities --json' to list connectors.",
                known.join(", ")
            ));
        };
        // A repeated alias must preserve union semantics, never widen scope.
        if !resolved.iter().any(|value| value == agent) {
            resolved.push(agent.to_string());
        }
    }
    Ok(resolved)
}

pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let agents = Vec::<String>::deserialize(deserializer)?;
    normalize(&agents).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{Filters, Request, validate_search};
    use serde_json::json;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn aliases_preserve_canonical_scope_and_distinct_agent_union() {
        assert_eq!(
            normalize(&names(&[
                " Claude-Code ", "claude", "Claude_Code", "CODEX", "pi-agent",
                "Oh-My-Pi", "ohmypi", "open-claw", "Open_Claw", "UNKNOWN",
            ])).unwrap(),
            names(&["claude_code", "codex", "pi_agent", "omp", "openclaw", "unknown"])
        );
        assert!(normalize(&[]).unwrap().is_empty());
    }

    #[test]
    fn every_registered_identity_accepts_case_and_separator_variants() {
        for slug in known_agents() {
            assert_eq!(
                normalize(&[
                    (*slug).to_string(),
                    slug.to_ascii_uppercase().replace('_', "-"),
                ]).unwrap(),
                [(*slug).to_string()]
            );
        }
        // Availability on this host is not part of filter admission.
        assert_eq!(normalize(&names(&["aider"])).unwrap(), names(&["aider"]));
    }

    #[test]
    fn unknown_and_mixed_scopes_are_errors_not_partial_valid_scopes() {
        for agents in [
            names(&["claud_code"]),
            names(&["claude", "claud_code"]),
            names(&["claud_code", "codex"]),
            names(&["未登録"]),
        ] {
            let error = normalize(&agents).unwrap_err();
            assert!(error.contains("unknown agent filter"));
            assert!(error.contains("Known agents:"));
        }
        assert!(normalize(&names(&["claud_code"]))
            .unwrap_err().contains("Did you mean 'claude_code'?"));
    }

    #[test]
    fn raw_limits_are_checked_before_trimming_or_alias_deduplication() {
        for agents in [
            vec!["claude".to_string(); MAX_FILTERS + 1],
            vec![format!("{}claude", " ".repeat(MAX_IDENTITY_BYTES))],
            names(&[""]),
            names(&["  "]),
        ] {
            assert!(normalize(&agents).is_err());
            assert!(serde_json::from_value::<Filters>(json!({"agents": agents})).is_err());
        }
        assert_eq!(
            normalize(&vec!["claude".to_string(); MAX_FILTERS]).unwrap(),
            names(&["claude_code"])
        );
    }

    #[test]
    fn all_json_retrieval_operations_resolve_agents_before_dispatch() {
        for op in ["search", "semantic_search", "refine"] {
            let mut value = json!({
                "id": 7, "op": op, "query": "paper cuts",
                "filters": {
                    "agents": ["claude", "Claude-Code", "CODEX"],
                    "workspaces": [" /Case/Sensitive "],
                    "source_id": "remote/CaseSensitive",
                    "created_from": -1, "created_to": 42
                }
            });
            if op == "refine" {
                value["lexical_query"] = json!("paper cuts");
            }
            let request = serde_json::from_value::<Request>(value.clone()).unwrap();
            let filters = match request {
                Request::Search { filters, .. }
                | Request::SemanticSearch { filters, .. }
                | Request::Refine { filters, .. } => filters,
                _ => panic!("expected a retrieval request"),
            };
            assert_eq!(filters.agents, names(&["claude_code", "codex"]));
            assert_eq!(filters.workspaces, names(&[" /Case/Sensitive "]));
            assert_eq!(filters.source_id.as_deref(), Some("remote/CaseSensitive"));
            assert_eq!((filters.created_from, filters.created_to), (Some(-1), Some(42)));
            validate_search("paper cuts", 1, 0, &filters).unwrap();
            value["filters"]["agents"] = json!(["claude", "claud_code"]);
            let error = serde_json::from_value::<Request>(value).unwrap_err();
            assert!(error.to_string().contains("unknown agent filter"), "{op}: {error}");
        }
    }

    #[test]
    fn direct_filter_decoding_and_internal_validation_do_not_allow_unknown_scopes() {
        // MCP argument structs deserialize this same Filters type, not Request.
        let filters: Filters = serde_json::from_value(json!({"agents": ["Claude_Code"]})).unwrap();
        assert_eq!(filters.agents, names(&["claude_code"]));
        assert!(serde_json::from_value::<Filters>(json!({"agents": ["nonsense-agent"]})).is_err());
        let invalid = Filters {
            agents: names(&["codex", "nonsense-agent"]),
            ..Filters::default()
        };
        assert!(validate_search("paper cuts", 1, 0, &invalid).is_err());
        assert!(serde_json::from_value::<Filters>(json!({})).unwrap().agents.is_empty());
    }
}
