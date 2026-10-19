//! One-shot access to the supervised, read-only search service.
//!
//! This is deliberately the same request path as stdio search, not another
//! search engine or an implicit repair lane. No stdin is opened in this mode.

use std::io::{self, Write};

use clap::Args;
use coding_agent_search::{CliError, CliResult};

use super::protocol::{self, Filters, Reply, Request};
use super::semantic;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
enum QueryMode {
    #[default]
    Lexical,
    Semantic,
    Hybrid,
}

#[derive(Debug, Default, Args)]
pub(super) struct Options {
    /// Run one query and exit; emit a bounded JSON reply without reading stdin.
    /// Uses the service's read-only readers, fallback consent and resource supervision.
    #[arg(long, value_name = "QUERY", conflicts_with_all = ["stdio", "mcp", "reranker_model"])]
    query: Option<String>,
    /// Retrieval mode for --query (default lexical). Semantic and hybrid require
    /// --semantic-embedder, --data-dir and --db. No models are downloaded.
    #[arg(long, value_enum, requires = "query", requires_ifs([
        ("semantic", "semantic_embedder"), ("hybrid", "semantic_embedder")
    ]))]
    mode: Option<QueryMode>,
    /// Request approximate retrieval with an explicit semantic/hybrid --mode.
    /// The service currently uses exact search and reports ANN as unavailable.
    #[arg(long, requires_all = ["query", "mode"])]
    approximate: bool,
    /// Maximum hits for --query (default 10, maximum 100).
    #[arg(long, requires = "query")]
    limit: Option<usize>,
    /// Result offset for --query (default 0; offset + limit + 1 must be at most 1024).
    #[arg(long, requires = "query")]
    offset: Option<usize>,
    /// Agent name or supported alias for --query; repeat for multiple agents.
    /// Unknown names fail before opening the index or archive.
    #[arg(long = "agent", requires = "query")]
    agents: Vec<String>,
    /// Exact workspace for --query; repeat for multiple workspaces.
    #[arg(long = "workspace", requires = "query")]
    workspaces: Vec<String>,
    /// Exact source ID for --query, not the synthetic remote group.
    #[arg(long, requires = "query")]
    source_id: Option<String>,
    /// Inclusive lower creation timestamp in epoch milliseconds for --query.
    #[arg(long, requires = "query", allow_hyphen_values = true)]
    created_from: Option<i64>,
    /// Inclusive upper creation timestamp in epoch milliseconds for --query.
    #[arg(long, requires = "query", allow_hyphen_values = true)]
    created_to: Option<i64>,
}

impl Options {
    /// Validate before resolving paths, starting supervisors or touching storage.
    pub(super) fn request(self) -> Result<Option<Request>, String> {
        let Some(query) = self.query else {
            if self.mode.is_some() || self.approximate {
                return Err("--mode and --approximate require --query".into());
            }
            return Ok(None);
        };
        let mode = self.mode.unwrap_or_default();
        if self.approximate && mode == QueryMode::Lexical {
            return Err("--approximate requires --mode semantic or --mode hybrid".into());
        }
        let limit = self.limit.unwrap_or(10);
        let offset = self.offset.unwrap_or(0);
        let mut filters = Filters {
            agents: self.agents,
            workspaces: self.workspaces,
            source_id: self.source_id,
            created_from: self.created_from,
            created_to: self.created_to,
        };
        // argv has no JSON frame, but must not bypass the transport's aggregate
        // input budget by supplying many individually valid filter identities.
        // Count RAW identities before trimming and collapsing agent aliases.
        let identity_bytes = filters
            .agents
            .iter()
            .chain(&filters.workspaces)
            .chain(filters.source_id.iter())
            .try_fold(query.len(), |total, value| total.checked_add(value.len()))
            .ok_or("one-shot query and filters exceed the 64 KiB input limit")?;
        if identity_bytes > protocol::MAX_REQUEST_BYTES {
            return Err("one-shot query and filters exceed the 64 KiB input limit".into());
        }
        // This lane constructs Filters directly rather than deserializing it.
        // Use the same resolver and retain its unknown-name/suggestion detail.
        filters.agents = protocol::normalize_agents(&filters.agents)?;
        protocol::validate_search(&query, limit, offset, &filters)?;
        // Dispatch through Session::handle just like the wire protocol. In
        // particular, hybrid's explicit fallback metadata and semantic-only
        // refusal semantics must not be reimplemented in the argv transport.
        Ok(Some(match mode {
            QueryMode::Lexical => Request::Search {
                id: 1,
                query,
                limit,
                offset,
                filters,
            },
            QueryMode::Semantic | QueryMode::Hybrid => Request::SemanticSearch {
                id: 1,
                query,
                mode: match mode {
                    QueryMode::Semantic => semantic::Mode::Semantic,
                    _ => semantic::Mode::Hybrid,
                },
                approximate: self.approximate,
                limit,
                offset,
                filters,
            },
        }))
    }
}

pub(super) fn invalid_request(message: impl Into<String>) -> CliError {
    // main recognizes a preformatted envelope even without a --json flag.
    CliError {
        code: 2,
        kind: "invalid_request",
        message: serde_json::to_string(&Reply::failure(Some(1), "invalid_request", message))
            .expect("a fixed request-error envelope is serializable"),
        hint: None,
        retryable: false,
    }
}

/// Publish only after the caller has destroyed storage readers and bridge
/// runtimes, still under the original deadline and resident-memory supervisor.
/// A service failure is NOT a successful CLI invocation. Size refusal is also
/// a failure, with no truncated or replacement success written to stdout.
pub(super) fn publish(
    reply: &Reply,
    output: &mut impl Write,
    diagnostics: &mut impl Write,
) -> CliResult<()> {
    let (bytes, failure) = match protocol::encode_line(reply) {
        Ok(bytes) => (
            bytes,
            (!reply.ok).then(|| {
                reply
                    .error
                    .as_ref()
                    .map_or("search_failed", |error| error.kind)
            }),
        ),
        Err(_) => (
            protocol::encode_line(&Reply::failure(
                reply.id,
                "response_too_large",
                "response exceeded the 1 MiB encoded limit; request fewer hits",
            ))
            .map_err(super::cli_io_error)?,
            Some("response_too_large"),
        ),
    };
    let write = |destination: &mut dyn Write| -> io::Result<()> {
        destination.write_all(&bytes)?;
        destination.flush()
    };
    if let Some(kind) = failure {
        write(diagnostics).map_err(super::cli_io_error)?;
        Err(CliError::already_reported(
            match kind {
                "invalid_request" => 2,
                "bridge_shutdown_failed" => super::deadline::WATCHDOG_FAILURE_EXIT_CODE,
                _ => 1,
            },
            kind,
            false,
        ))
    } else {
        write(output).map_err(super::cli_io_error)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ServiceCli;
    use super::*;
    use clap::{CommandFactory, Parser};
    use serde_json::json;

    #[test]
    fn one_shot_and_stdio_are_mutually_exclusive_and_query_options_are_scoped() {
        ServiceCli::command().debug_assert();
        let base = ["cass", "serve", "--index", "/not-opened"];
        for extra in [
            vec![],
            vec!["--stdio", "--query", "needle"],
            vec!["--query", "needle", "--mcp"],
            vec!["--stdio", "--limit", "1"],
            vec!["--stdio", "--agent", "codex"],
            vec!["--query", "needle", "--reranker-model", "/model"],
        ] {
            assert!(ServiceCli::try_parse_from(base.into_iter().chain(extra)).is_err());
        }
        for extra in [vec!["--stdio"], vec!["--query", "needle"]] {
            assert!(ServiceCli::try_parse_from(base.into_iter().chain(extra)).is_ok());
        }
    }

    #[test]
    fn query_request_preserves_filters_and_uses_service_validation() {
        let request = Options {
            query: Some("git fixture".into()),
            limit: Some(3),
            offset: Some(4),
            agents: vec!["codex".into()],
            workspaces: vec!["/work".into()],
            source_id: Some("remote-id".into()),
            created_from: Some(-1),
            created_to: Some(42),
            ..Options::default()
        }
        .request()
        .unwrap()
        .unwrap();
        let Request::Search {
            id,
            query,
            limit,
            offset,
            filters,
        } = request
        else {
            panic!("expected a lexical request");
        };
        assert_eq!(
            (id, query.as_str(), limit, offset),
            (1, "git fixture", 3, 4)
        );
        assert_eq!(filters.agents, ["codex"]);
        assert_eq!(filters.workspaces, ["/work"]);
        assert_eq!(filters.source_id.as_deref(), Some("remote-id"));
        assert_eq!(
            (filters.created_from, filters.created_to),
            (Some(-1), Some(42))
        );
        for (limit, offset) in [(0, 0), (101, 0), (1, 1023), (1, usize::MAX)] {
            assert!(
                Options {
                    query: Some("needle".into()),
                    limit: Some(limit),
                    offset: Some(offset),
                    ..Options::default()
                }
                .request()
                .is_err()
            );
        }
        assert!(
            Options {
                query: Some(" ".into()),
                ..Options::default()
            }
            .request()
            .is_err()
        );
        assert!(
            Options {
                query: Some("needle".into()),
                agents: vec!["a".repeat(protocol::MAX_IDENTITY_BYTES); protocol::MAX_FILTERS],
                ..Options::default()
            }
            .request()
            .is_err()
        );
    }

    #[test]
    fn one_shot_aliases_share_transport_resolution_and_unknown_names_are_usage_errors() {
        let request = Options {
            query: Some("scopeprobe".into()),
            agents: vec!["claude".into(), "Claude-Code".into(), "CODEX".into()],
            ..Options::default()
        }
        .request()
        .unwrap()
        .unwrap();
        let Request::Search { filters, .. } = request else {
            panic!("expected a lexical request");
        };
        assert_eq!(filters.agents, ["claude_code", "codex"]);
        let error = Options {
            query: Some("scopeprobe".into()),
            agents: vec!["codex".into(), "claud_code".into()],
            ..Options::default()
        }
        .request()
        .unwrap_err();
        let error = invalid_request(error);
        assert_eq!(error.code, 2);
        assert!(!error.retryable);
        let envelope: serde_json::Value = serde_json::from_str(&error.message).unwrap();
        assert_eq!(envelope["ok"], false);
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.contains("claud_code"));
        assert!(message.contains("Did you mean 'claude_code'?"));
    }

    #[test]
    fn one_shot_raw_byte_budget_cannot_be_bypassed_by_alias_collapse() {
        let error = Options {
            query: Some("scopeprobe".into()),
            agents: vec![format!("{}claude", " ".repeat(2050)); protocol::MAX_FILTERS],
            ..Options::default()
        }
        .request()
        .unwrap_err();
        assert!(error.contains("64 KiB"));
    }

    fn parse_options(arguments: &[&str]) -> Options {
        let cli = ServiceCli::try_parse_from(arguments).expect("valid one-shot arguments");
        let super::super::ServiceCommand::Serve { once, .. } = cli.command;
        once
    }

    #[test]
    fn lexical_default_and_explicit_mode_need_no_semantic_configuration() {
        for extra in [vec![], vec!["--mode", "lexical"]] {
            let mut arguments = vec![
                "cass",
                "serve",
                "--index",
                "/not-opened",
                "--query",
                "needle",
            ];
            arguments.extend(extra);
            let request = parse_options(&arguments).request().unwrap().unwrap();
            assert!(matches!(request, Request::Search { id: 1, .. }));
        }
    }

    #[test]
    fn semantic_modes_construct_the_same_typed_request_as_stdio() {
        for (name, expected) in [
            ("semantic", semantic::Mode::Semantic),
            ("hybrid", semantic::Mode::Hybrid),
        ] {
            let request = parse_options(&[
                "cass",
                "serve",
                "--data-dir",
                "/not-opened",
                "--db",
                "/archive.db",
                "--semantic-embedder",
                "hash",
                "--query",
                "cache δ",
                "--mode",
                name,
                "--limit",
                "3",
                "--offset",
                "4",
                "--agent",
                "Claude-Code",
                "--agent",
                "codex",
                "--workspace",
                "/work/δ ' %_",
                "--source-id",
                "remote-id",
                "--created-from",
                "-1",
                "--created-to",
                "42",
                "--approximate",
            ])
            .request()
            .unwrap()
            .unwrap();
            let wire: Request = serde_json::from_value(json!({
                "id": 1, "op": "semantic_search", "query": "cache δ", "mode": name,
                "limit": 3, "offset": 4, "approximate": true,
                "filters": { "agents": ["Claude-Code", "codex"],
                    "workspaces": ["/work/δ ' %_"], "source_id": "remote-id",
                    "created_from": -1, "created_to": 42 }
            }))
            .unwrap();
            for actual in [request, wire] {
                let Request::SemanticSearch {
                    id,
                    query,
                    mode,
                    approximate,
                    limit,
                    offset,
                    filters,
                } = actual
                else {
                    panic!("expected a semantic service request");
                };
                assert_eq!(
                    (id, query.as_str(), mode, approximate, limit, offset),
                    (1, "cache δ", expected, true, 3, 4)
                );
                assert_eq!(filters.agents, ["claude_code", "codex"]);
                assert_eq!(filters.workspaces, ["/work/δ ' %_"]);
                assert_eq!(filters.source_id.as_deref(), Some("remote-id"));
                assert_eq!(
                    (filters.created_from, filters.created_to),
                    (Some(-1), Some(42))
                );
            }
        }
    }

    #[test]
    fn semantic_modes_require_an_explicit_embedder_archive_and_data_directory() {
        for mode in ["semantic", "hybrid"] {
            for location in [
                vec!["--index", "/index"],
                vec!["--data-dir", "/data", "--db", "/archive"],
                vec!["--data-dir", "/data", "--semantic-embedder", "hash"],
                vec![
                    "--index",
                    "/index",
                    "--db",
                    "/archive",
                    "--semantic-embedder",
                    "hash",
                ],
            ] {
                let mut arguments = vec!["cass", "serve", "--query", "needle", "--mode", mode];
                arguments.extend(location);
                assert!(ServiceCli::try_parse_from(arguments).is_err());
            }
            for embedder in ["hash", "minilm", "multilingual-minilm", "external"] {
                let options = parse_options(&[
                    "cass",
                    "serve",
                    "--query",
                    "needle",
                    "--mode",
                    mode,
                    "--data-dir",
                    "/data",
                    "--db",
                    "/archive",
                    "--semantic-embedder",
                    embedder,
                ]);
                let request = options.request().unwrap().unwrap();
                assert!(matches!(
                    request,
                    Request::SemanticSearch {
                        approximate: false,
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn one_shot_modes_do_not_change_stdio_or_allow_mixed_transports() {
        let base = [
            "cass",
            "serve",
            "--data-dir",
            "/data",
            "--db",
            "/archive",
            "--semantic-embedder",
            "hash",
        ];
        for extra in [vec!["--stdio"], vec!["--stdio", "--mcp"]] {
            let arguments = base.into_iter().chain(extra).collect::<Vec<_>>();
            assert!(parse_options(&arguments).request().unwrap().is_none());
        }
        for extra in [
            vec!["--stdio", "--mode", "hybrid"],
            vec!["--stdio", "--query", "needle", "--mode", "semantic"],
            vec!["--query", "needle", "--mode", "semantic", "--mcp"],
            vec![
                "--query",
                "needle",
                "--mode",
                "hybrid",
                "--reranker-model",
                "/model",
            ],
            vec!["--query", "needle", "--mode", "unknown"],
            vec!["--query", "needle", "--approximate"],
        ] {
            assert!(ServiceCli::try_parse_from(base.into_iter().chain(extra)).is_err());
        }
    }

    #[test]
    fn approximate_lexical_requests_fail_before_service_admission() {
        for mode in [None, Some(QueryMode::Lexical)] {
            let error = Options {
                query: Some("needle".into()),
                mode,
                approximate: true,
                ..Options::default()
            }
            .request()
            .unwrap_err();
            let error = invalid_request(error);
            assert_eq!(error.code, 2);
            assert!(!error.retryable);
            assert!(
                error
                    .message
                    .contains("requires --mode semantic or --mode hybrid")
            );
        }
        let options = parse_options(&[
            "cass",
            "serve",
            "--index",
            "/not-opened",
            "--query",
            "needle",
            "--mode",
            "lexical",
            "--approximate",
        ]);
        assert!(options.request().is_err());
    }

    #[test]
    fn every_mode_keeps_shared_query_scope_and_page_admission() {
        for mode in [QueryMode::Lexical, QueryMode::Semantic, QueryMode::Hybrid] {
            let cases = [
                Options {
                    query: Some(" ".into()),
                    ..Options::default()
                },
                Options {
                    query: Some("é".repeat(2049)),
                    ..Options::default()
                },
                Options {
                    limit: Some(0),
                    ..Options::default()
                },
                Options {
                    limit: Some(101),
                    ..Options::default()
                },
                Options {
                    offset: Some(usize::MAX),
                    ..Options::default()
                },
                Options {
                    offset: Some(1023),
                    limit: Some(1),
                    ..Options::default()
                },
                Options {
                    agents: vec!["codex".into(), "nonsense-agent".into()],
                    ..Options::default()
                },
                Options {
                    workspaces: vec!["/work".into(); 33],
                    ..Options::default()
                },
                Options {
                    source_id: Some(" local ".into()),
                    ..Options::default()
                },
                Options {
                    created_from: Some(42),
                    created_to: Some(41),
                    ..Options::default()
                },
            ];
            for mut options in cases {
                options.mode = Some(mode);
                options.query.get_or_insert_with(|| "needle".into());
                assert!(
                    options.request().is_err(),
                    "mode {mode:?} bypassed admission"
                );
            }
        }
    }

    #[test]
    fn semantic_modes_preserve_raw_byte_admission_before_alias_normalization() {
        for mode in [QueryMode::Semantic, QueryMode::Hybrid] {
            let error = Options {
                query: Some("needle".into()),
                mode: Some(mode),
                agents: vec![format!("{}claude", " ".repeat(2050)); protocol::MAX_FILTERS],
                ..Options::default()
            }
            .request()
            .unwrap_err();
            assert!(error.contains("64 KiB"));
        }
    }

    #[test]
    fn constructed_options_cannot_silently_drop_mode_without_a_query() {
        for options in [
            Options {
                mode: Some(QueryMode::Semantic),
                ..Options::default()
            },
            Options {
                approximate: true,
                ..Options::default()
            },
        ] {
            assert!(options.request().unwrap_err().contains("require --query"));
        }
        assert!(Options::default().request().unwrap().is_none());
    }

    #[test]
    fn success_and_authoritative_empty_replies_use_only_stdout() {
        for hits in [json!([]), json!([{"conversation_id": 7}])] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let reply = Reply::success(1, json!({"hits": hits}));
            publish(&reply, &mut out, &mut err).unwrap();
            assert!(err.is_empty());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&out).unwrap()["ok"],
                true
            );
            assert_eq!(out.last(), Some(&b'\n'));
        }
    }

    #[test]
    fn runtime_and_size_failures_are_nonzero_and_never_reach_stdout() {
        for reply in [
            Reply::failure(Some(1), "search_failed", "query fuel exhausted"),
            Reply::success(
                1,
                json!({"oversized": "x".repeat(protocol::MAX_RESPONSE_BYTES)}),
            ),
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let failure = publish(&reply, &mut out, &mut err).unwrap_err();
            assert_eq!(failure.code, 1);
            assert!(failure.was_already_reported());
            assert!(out.is_empty());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&err).unwrap()["ok"],
                false
            );
            assert!(err.len() <= protocol::MAX_RESPONSE_BYTES);
        }
    }

    #[test]
    fn incomplete_bridge_teardown_has_a_supervisor_failure_exit() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let failure = publish(
            &Reply::failure(Some(1), "bridge_shutdown_failed", "teardown failed"),
            &mut out,
            &mut err,
        )
        .unwrap_err();
        assert_eq!(
            failure.code,
            super::super::deadline::WATCHDOG_FAILURE_EXIT_CODE
        );
        assert!(failure.was_already_reported());
        assert!(out.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&err).unwrap()["error"]["kind"],
            "bridge_shutdown_failed"
        );
    }

    #[test]
    fn output_transport_errors_are_not_reported_as_success() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(publish(&Reply::success(1, json!({})), &mut Broken, &mut Vec::new()).is_err());
        assert!(
            publish(
                &Reply::failure(Some(1), "search_failed", "refused"),
                &mut Vec::new(),
                &mut Broken
            )
            .is_err()
        );
    }
}
