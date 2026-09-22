//! A caller-owned, persistent lexical search session over standard I/O.
//!
//! The reader is loaded lazily and retained until explicit reload, shutdown, or
//! EOF. Search is index-only. Canonical follow-up requires an explicit --db
//! and exact source/conversation/message coordinates; it never opens raw files.
//! Neither lane starts models, writers, automatic refresh, or detached children.
//! Frame/page bounds do not cap reader RSS or interrupt a native engine call.

#[path = "search_service/protocol.rs"]
mod protocol;
#[path = "search_service/mcp.rs"]
mod mcp;
#[path = "search_service/canonical.rs"]
mod canonical;
#[cfg(test)]
#[path = "search_service/tests.rs"]
mod tests;

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchClientOptions, SearchFilters};
use coding_agent_search::sources::provenance::SourceFilter;
use serde_json::{Value, json};

use protocol::{Filters, Frame, MAX_IDENTITY_BYTES, Reply, Request};

#[derive(Debug, Parser)]
#[command(name = "cass", disable_version_flag = true)]
struct ServiceCli {
    #[command(subcommand)]
    command: ServiceCommand,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Reuse one read-only lexical index reader for a stream of JSON requests.
    /// Does not load models or check archive freshness. See docs/SEARCH_SERVICE.md.
    Serve {
        /// Published lexical index directory, instead of --data-dir.
        #[arg(
            long,
            value_name = "PATH",
            required_unless_present = "data_dir",
            conflicts_with = "data_dir"
        )]
        index: Option<PathBuf>,
        /// Select the compiled lexical index beneath this explicit data directory.
        /// Resolves the path only; never opens or repairs the canonical database.
        #[arg(
            long,
            value_name = "PATH",
            required_unless_present = "index",
            conflicts_with = "index"
        )]
        data_dir: Option<PathBuf>,
        /// Use newline-delimited JSON requests and responses on standard I/O.
        #[arg(long, required = true)]
        stdio: bool,
        /// Speak MCP instead of the CASS JSON-lines protocol on the same stdio transport.
        #[arg(long, requires = "stdio")]
        mcp: bool,
        /// Opt in to canonical view requests against this fixed read-only archive.
        /// Search, status and startup still never open the database.
        #[arg(long, value_name = "PATH")]
        db: Option<PathBuf>,
    },
}

/// This independent parser follows the existing logical-archive dispatch seam:
/// no ordinary CLI readiness probe or runtime starts before serving requests.
pub fn run(args: Vec<String>) -> coding_agent_search::CliResult<()> {
    let cli = match ServiceCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error.print().map_err(cli_io_error)?;
            return if code == 0 {
                Ok(())
            } else {
                Err(coding_agent_search::CliError::already_reported(
                    code,
                    "argument_parsing",
                    false,
                ))
            };
        }
    };
    let ServiceCommand::Serve { index, data_dir, stdio: _, mcp, db } = cli.command;
    let index = match (index, data_dir) {
        (Some(index), None) => index,
        (None, Some(data_dir)) => {
            coding_agent_search::search::tantivy::expected_index_dir(&data_dir)
        }
        _ => return Err(coding_agent_search::CliError {
            code: 2,
            kind: "argument_parsing",
            message: "serve requires exactly one of --index or --data-dir".into(),
            hint: None,
            retryable: false,
        }),
    };
    let index = if index.is_absolute() {
        index
    } else {
        std::env::current_dir().map_err(cli_io_error)?.join(index)
    };
    let mut session = Session::new(index);
    session.archive = db.map(|path| {
        if path.is_absolute() { Ok(path) }
        else { std::env::current_dir().map(|cwd| cwd.join(path)) }
    }).transpose().map_err(cli_io_error)?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    if mcp {
        mcp::serve_io(&mut session, &mut input, &mut output)
    } else {
        serve_io(&mut session, &mut input, &mut output)
    }
    .map_err(cli_io_error)
}

fn cli_io_error(error: io::Error) -> coding_agent_search::CliError {
    coding_agent_search::CliError {
        code: 1,
        kind: "search-service-io",
        message: error.to_string(),
        hint: None,
        retryable: false,
    }
}

struct Session {
    index: PathBuf,
    archive: Option<PathBuf>,
    canonical_read_attempts: u64,
    canonical_reads_completed: u64,
    client: Option<SearchClient>,
    open_attempts: u64,
    successful_opens: u64,
    queries_completed: u64,
}

impl Session {
    fn new(index: PathBuf) -> Self {
        Self {
            index,
            archive: None,
            canonical_read_attempts: 0,
            canonical_reads_completed: 0,
            client: None,
            open_attempts: 0,
            successful_opens: 0,
            queries_completed: 0,
        }
    }

    fn ensure_loaded(&mut self) -> Result<()> {
        if self.client.is_none() {
            self.open_attempts = self.open_attempts.saturating_add(1);
            let client = open_snapshot(&self.index)?;
            self.successful_opens = self.successful_opens.saturating_add(1);
            self.client = Some(client);
        }
        Ok(())
    }

    fn status(&self) -> Value {
        json!({
            "service": "cass_lexical_stdio",
            "mode": "lexical",
            "loaded": self.client.is_some(),
            // A session-local epoch is NOT an archive generation certificate.
            "reader_epoch": self.client.as_ref().map(|_| self.successful_opens),
            "open_attempts": self.open_attempts,
            "successful_opens": self.successful_opens,
            "queries_completed": self.queries_completed,
            "snapshot_policy": "pinned_until_reload",
            "freshness": "not_checked",
            "canonical_view_enabled": self.archive.is_some(),
            "canonical_database_accessed": self.canonical_read_attempts != 0,
            "canonical_read_attempts": self.canonical_read_attempts,
            "canonical_reads_completed": self.canonical_reads_completed,
            "models_loaded": false,
            "maintenance_performed": false,
            "limits": {
                "request_bytes": protocol::MAX_REQUEST_BYTES,
                "response_bytes": protocol::MAX_RESPONSE_BYTES,
                "query_bytes": protocol::MAX_QUERY_BYTES,
                "limit": protocol::MAX_LIMIT,
                "page_window": protocol::MAX_WINDOW,
                "canonical_context": canonical::MAX_CONTEXT,
                "canonical_content_bytes": canonical::MAX_CONTENT_BYTES,
            }
        })
    }

    fn search(
        &mut self,
        query: &str,
        filters: Filters,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        let reused = self.client.is_some();
        let open_started = Instant::now();
        self.ensure_loaded()?;
        let setup_ms = open_started.elapsed().as_millis();
        let client = self.client.as_ref().context("search reader is unavailable")?;
        let started = Instant::now();
        // Only engine-owned filters are exposed. In particular session_paths
        // and the synthetic 'remote' group currently trigger a potentially
        // corpus-sized post-filter retry in SearchClient; this bounded service
        // does not advertise them. source_id always means one literal ID.
        let filters = SearchFilters {
            agents: filters.agents.into_iter().collect(),
            workspaces: filters.workspaces.into_iter().collect(),
            source_filter: filters.source_id.map_or(SourceFilter::All, SourceFilter::SourceId),
            created_from: filters.created_from,
            created_to: filters.created_to,
            session_paths: HashSet::new(),
        };
        // No payload hydration and no query-prefix response cache: every call
        // uses the production lexical parser, scorer and result reducer against
        // the SAME admitted reader, without opening a canonical archive.
        let mut hits = client.search(
            query,
            filters,
            limit + 1,
            offset,
            FieldMask::new(false, true, true, false),
        )?;
        ensure!(hits.len() <= limit + 1, "search backend exceeded the requested page");
        let has_next = hits.len() > limit;
        let next_offset = offset.checked_add(limit).filter(|next| {
            has_next && next.checked_add(limit).and_then(|n| n.checked_add(1))
                .is_some_and(|n| n <= protocol::MAX_WINDOW)
        });
        hits.truncate(limit);
        let mut summaries = Vec::with_capacity(hits.len());
        for hit in hits {
            ensure!(hit.score.is_finite(), "search backend returned a non-finite score");
            ensure!(hit.line_number != Some(0), "search backend returned an invalid message ordinal");
            for value in [
                Some(hit.source_path.as_str()),
                Some(hit.source_id.as_str()),
                Some(hit.agent.as_str()),
                Some(hit.workspace.as_str()),
                Some(hit.origin_kind.as_str()),
                hit.origin_host.as_deref(),
                hit.workspace_original.as_deref(),
            ].into_iter().flatten() {
                ensure!(value.len() <= MAX_IDENTITY_BYTES,
                    "search identity exceeds the service's 4096-byte bound; identities are never truncated");
            }
            summaries.push(json!({
                "title": prefix(&hit.title, 256),
                "snippet": prefix(&hit.snippet, 800),
                "score": hit.score,
                "source_path": hit.source_path,
                "source_id": hit.source_id,
                "conversation_id": hit.conversation_id,
                "message_index": hit.line_number,
                "agent": hit.agent,
                "workspace": hit.workspace,
                "workspace_original": hit.workspace_original,
                "created_at": hit.created_at,
                "match_type": hit.match_type,
                "origin_kind": hit.origin_kind,
                "origin_host": hit.origin_host,
            }));
        }
        self.queries_completed = self.queries_completed.saturating_add(1);
        Ok(json!({
            "hits": summaries,
            "count": summaries.len(),
            "limit": limit,
            "offset": offset,
            // An observed extra hit proves another page, but bounded engine
            // overfetch does not establish exhaustion. Null means unknown.
            "has_more": if has_next { Some(true) } else { None },
            "next_offset": next_offset,
            "page_window_exhausted": has_next && next_offset.is_none(),
            "preview_only": true,
            "reader_reused": reused,
            "setup_ms": setup_ms,
            "search_ms": started.elapsed().as_millis(),
            "snapshot": self.status(),
        }))
    }

    fn handle(&mut self, request: Request) -> (Reply, bool) {
        match request {
            Request::View { id, source_path, source_id, conversation_id, message_index, context } => {
                let request = canonical::View {
                    source_path: &source_path, source_id: &source_id,
                    conversation_id, message_index, context,
                };
                if let Err(message) = request.validate() {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                let Some(db) = &self.archive else {
                    return (Reply::failure(Some(id), "canonical_access_disabled",
                        "canonical view requires an explicit --db at service startup; search remains index-only"), false);
                };
                self.canonical_read_attempts = self.canonical_read_attempts.saturating_add(1);
                let reply = match canonical::read(db, &request) {
                    Ok(result) => {
                        self.canonical_reads_completed = self.canonical_reads_completed.saturating_add(1);
                        Reply::success(id, result)
                    }
                    Err(error) => Reply::failure(Some(id), canonical::error_kind(&error), format!("{error:#}")),
                };
                (reply, false)
            }
            Request::Status { id } => (Reply::success(id, self.status()), false),
            Request::Shutdown { id } => {
                drop(self.client.take());
                (Reply::success(id, json!({"shutdown": true})), true)
            }
            Request::Reload { id } => {
                // Drop first: never retain two multi-GB generations during
                // reload. A failed reload leaves no silently stale fallback.
                drop(self.client.take());
                let started = Instant::now();
                let reply = match self.ensure_loaded() {
                    Ok(()) => Reply::success(id, json!({
                        "setup_ms": started.elapsed().as_millis(),
                        "snapshot": self.status(),
                    })),
                    Err(error) => Reply::failure(Some(id), "index_unavailable", format!("{error:#}")),
                };
                (reply, false)
            }
            Request::Search { id, query, limit, offset, filters } => {
                // Invalid work must not load an index or allocate from caller k.
                if let Err(message) = protocol::validate_search(&query, limit, offset, &filters) {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                let reply = match self.search(&query, filters, limit, offset) {
                    Ok(result) => Reply::success(id, result),
                    Err(error) => Reply::failure(Some(id), "search_failed", format!("{error:#}")),
                };
                (reply, false)
            }
        }
    }
}

fn open_snapshot(index: &Path) -> Result<SearchClient> {
    SearchClient::open_with_options(
        index,
        None,
        SearchClientOptions {
            enable_reload: false,
            enable_warm: false,
            strict_read_only: true,
        },
    )?
    .context("no readable lexical index; build it separately with cass index before retrying")
}

fn prefix(value: &str, maximum_chars: usize) -> &str {
    value.char_indices().nth(maximum_chars).map_or(value, |(at, _)| &value[..at])
}

fn serve_io(
    session: &mut Session,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<()> {
    loop {
        let bytes = match protocol::read_frame(input)? {
            Frame::End => return Ok(()),
            Frame::TooLarge => {
                protocol::write_reply(output, &Reply::failure(
                    None,
                    "request_too_large",
                    "request exceeds 64 KiB; this session is closing without processing the remainder",
                ))?;
                return Ok(());
            }
            Frame::Line(bytes) => bytes,
        };
        let request = match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) => request,
            Err(error) => {
                protocol::write_reply(output, &Reply::failure(
                    None, "invalid_request", error.to_string(),
                ))?;
                continue;
            }
        };
        let (reply, shutdown) = session.handle(request);
        protocol::write_reply(output, &reply)?;
        if shutdown {
            return Ok(());
        }
    }
}
