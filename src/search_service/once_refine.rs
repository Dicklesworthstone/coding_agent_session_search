//! One-shot argv adapter for the existing bounded lexical refinement request.
//!
//! The service owns retrieval, local model loading, ranking and supervision.
//! This module only admits the same request that the JSON-lines protocol uses.

use super::protocol::{self, Filters, Request};
use super::refinement;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    query: String,
    lexical_query: String,
    #[serde(default)]
    filters: Filters,
    #[serde(default = "refinement::default_candidates")]
    candidate_limit: usize,
    #[serde(default = "refinement::default_limit")]
    limit: usize,
}

/// Validate before starting a supervisor, resolving paths or admitting readers.
/// Whitespace and escaped JSON count against the encoded-input ceiling too.
/// Scope is decoded through the shared filter type, including agent aliases.
pub(super) fn request(encoded: &str) -> Result<Request, String> {
    if encoded.len() > protocol::MAX_REQUEST_BYTES {
        return Err("refinement selection exceeds the 64 KiB input limit".into());
    }
    let selection: Selection = serde_json::from_str(encoded)
        .map_err(|error| format!("invalid refinement selection: {error}"))?;
    refinement::validate(
        &selection.query,
        &selection.lexical_query,
        &selection.filters,
        selection.candidate_limit,
        selection.limit,
    )
    .map_err(str::to_owned)?;
    Ok(Request::Refine {
        id: 1,
        query: selection.query,
        lexical_query: selection.lexical_query,
        filters: selection.filters,
        candidate_limit: selection.candidate_limit,
        limit: selection.limit,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{ServiceCli, ServiceCommand, Session};
    use super::*;
    use clap::{CommandFactory, Parser};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    const SELECTION: &str =
        r#"{"query":"Which change fixed the isolation failure?","lexical_query":"git fixture"}"#;

    fn parsed(extra: &[&str]) -> Result<ServiceCli, clap::Error> {
        ServiceCli::try_parse_from(
            ["cass", "serve", "--index", "/not-opened"]
                .into_iter()
                .chain(extra.iter().copied()),
        )
    }

    fn fields(request: Request) -> Value {
        let Request::Refine {
            id,
            query,
            lexical_query,
            filters,
            candidate_limit,
            limit,
        } = request
        else {
            panic!("one-shot refinement must use the real service request");
        };
        json!({
            "id": id, "query": query, "lexical_query": lexical_query,
            "candidate_limit": candidate_limit, "limit": limit,
            "filters": {
                "agents": filters.agents, "workspaces": filters.workspaces,
                "source_id": filters.source_id, "created_from": filters.created_from,
                "created_to": filters.created_to,
            },
        })
    }

    #[test]
    fn one_shot_refinement_requires_model_selection_but_not_archive_access() {
        ServiceCli::command().debug_assert();
        assert!(parsed(&["--refine", SELECTION]).is_err());
        let cli = parsed(&["--refine", SELECTION, "--reranker-model", "/not-loaded"])
            .expect("index-only refinement must not require --db or --stdio");
        let ServiceCommand::Serve {
            refine,
            stdio,
            db,
            reranker_model,
            ..
        } = cli.command;
        assert!(!stdio);
        assert!(db.is_none());
        assert_eq!(reranker_model, Some(PathBuf::from("/not-loaded")));
        let selected = fields(request(refine.as_deref().unwrap()).unwrap());
        assert_eq!(selected["id"], 1);
        assert_eq!(selected["candidate_limit"], 20);
        assert_eq!(selected["limit"], 5);
        assert_eq!(selected["filters"]["agents"], json!([]));
    }

    #[test]
    fn one_shot_refinement_cannot_discard_another_transport_or_query_scope() {
        let base = ["--refine", SELECTION, "--reranker-model", "/not-loaded"];
        for extra in [
            vec!["--stdio"],
            vec!["--mcp"],
            vec!["--query", "other"],
            vec!["--transcript", "{}", "--db", "/archive"],
            vec!["--mode", "lexical"],
            vec!["--approximate"],
            vec!["--limit", "1"],
            vec!["--offset", "0"],
            vec!["--agent", "codex"],
            vec!["--workspace", "/scope"],
            vec!["--source-id", "remote-id"],
            vec!["--created-from", "-1"],
            vec!["--created-to", "42"],
        ] {
            let args = base.into_iter().chain(extra).collect::<Vec<_>>();
            assert!(parsed(&args).is_err(), "accepted incompatible {args:?}");
        }
        assert!(
            ServiceCli::try_parse_from([
                "cass",
                "serve",
                "--data-dir",
                "/not-opened",
                "--db",
                "/archive",
                "--refine",
                SELECTION,
                "--reranker-model",
                "/not-loaded",
                "--semantic-embedder",
                "hash",
            ])
            .is_err()
        );
        // Existing query-only behavior is unchanged: a model path alone must
        // never turn --query into an implicit refinement request.
        assert!(parsed(&["--query", "needle", "--reranker-model", "/model"]).is_err());
        assert!(parsed(&["--stdio", "--reranker-model", "/model"]).is_ok());
        assert!(
            parsed(&[
                "--refine",
                SELECTION,
                "--reranker-model",
                "/model",
                "--lexical-fallback",
                "--db",
                "/archive",
            ])
            .is_ok()
        );
    }

    #[test]
    fn one_shot_refinement_preserves_exact_queries_and_filters_like_stdio() {
        let selection = json!({
            "query": "Why did the δ fixture fail?",
            "lexical_query": "git AND (fixture OR isolation)",
            "candidate_limit": 12,
            "limit": 3,
            "filters": {
                "agents": ["codex"],
                "workspaces": ["/work/δ' %_", " /literal-space "],
                "source_id": "remote-id",
                "created_from": -1,
                "created_to": 42,
            },
        });
        let actual = fields(request(&selection.to_string()).unwrap());
        let mut wire = selection.clone();
        wire["op"] = json!("refine");
        wire["id"] = json!(1);
        let expected = fields(serde_json::from_value(wire).unwrap());
        assert_eq!(actual, expected);
        assert_eq!(actual["query"], selection["query"]);
        assert_eq!(actual["lexical_query"], selection["lexical_query"]);
        assert_eq!(actual["filters"], selection["filters"]);
        assert_eq!(actual["limit"], 3);
        assert_eq!(actual["candidate_limit"], 12);
    }

    #[test]
    fn one_shot_refinement_resolves_aliases_without_widening_scope() {
        let mut selection: Value = serde_json::from_str(SELECTION).unwrap();
        selection["filters"] = json!({"agents": ["claude", "Claude-Code", "CODEX"]});
        let selected = fields(request(&selection.to_string()).unwrap());
        assert_eq!(
            selected["filters"]["agents"],
            json!(["claude_code", "codex"])
        );
        selection["filters"]["agents"] = json!(vec!["claude"; 33]);
        assert!(request(&selection.to_string()).is_err());
    }

    #[test]
    fn one_shot_refinement_rejects_ambiguous_or_out_of_budget_requests() {
        for encoded in ["", "null", "true", "[]", "{}", "{", "{} {}"] {
            assert!(request(encoded).is_err(), "accepted {encoded}");
        }
        let base: Value = serde_json::from_str(SELECTION).unwrap();
        for (key, value) in [
            ("query", json!(" ")),
            ("query", json!("雪".repeat(1366))),
            ("lexical_query", json!("")),
            ("lexical_query", json!("x".repeat(4097))),
            ("candidate_limit", json!(0)),
            ("candidate_limit", json!(33)),
            ("candidate_limit", json!(4)), // default result limit is still five
            ("candidate_limit", json!(u64::MAX)),
            ("limit", json!(0)),
            ("limit", json!(21)),
            ("limit", json!(-1)),
            ("limit", json!(1.5)),
            ("limit", json!("1")),
            ("id", json!(1)),
            ("op", json!("refine")),
            ("offset", json!(0)),
            ("mode", json!("semantic")),
            ("filters", json!({"agents": ["not-a-cass-agent-92841"]})),
            ("filters", json!({"workspaces": vec!["x"; 33]})),
            ("filters", json!({"source_id": " local "})),
            ("filters", json!({"created_from": 43, "created_to": 42})),
            ("filters", json!({"session_paths": ["/unimplemented"]})),
        ] {
            let mut selection = base.clone();
            selection[key] = value;
            assert!(
                request(&selection.to_string()).is_err(),
                "accepted {selection}"
            );
        }
        assert!(request(r#"{"query":"first","query":"second","lexical_query":"needle"}"#).is_err());
        assert!(request(r#"{"query":"q","lexical_query":"needle","filters":{"source_id":"a","source_id":"b"}}"#).is_err());
        let mut selection = base;
        selection["candidate_limit"] = json!(32);
        selection["limit"] = json!(32);
        assert!(request(&selection.to_string()).is_ok());
    }

    #[test]
    fn one_shot_refinement_counts_encoded_bytes_before_deserializing() {
        let exact = format!(
            "{}{}",
            SELECTION,
            " ".repeat(protocol::MAX_REQUEST_BYTES - SELECTION.len())
        );
        assert!(request(&exact).is_ok());
        assert!(request(&(exact + " ")).unwrap_err().contains("64 KiB"));
        // Invalid JSON still receives the size refusal first. Count bytes,
        // not Unicode characters or the eventual decoded request fields.
        let oversized = "δ".repeat(protocol::MAX_REQUEST_BYTES / 2 + 1);
        assert!(request(&oversized).unwrap_err().contains("64 KiB"));
    }

    #[test]
    fn one_shot_refinement_reaches_existing_service_consent_before_reader_open() {
        let mut session = Session::new(PathBuf::from("/not-opened"));
        let (reply, shutdown) = session.handle(request(SELECTION).unwrap());
        assert!(!shutdown);
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().kind, "refinement_disabled");
        assert_eq!(session.status()["open_attempts"], 0);
        assert_eq!(session.status()["canonical_database_accessed"], false);
        assert_eq!(session.status()["models_loaded"], false);
    }
}
