//! A caller-owned, persistent lexical search session over standard I/O.
//!
//! The reader is loaded lazily and retained until explicit reload, shutdown, or
//! EOF. Search is index-only unless --lexical-fallback is explicitly enabled.
//! That recovery lane admits only small archives after a Quill fuel refusal.
//! Canonical follow-up requires an explicit --db
//! and exact source/conversation/message coordinates; it never opens raw files.
//! Separate opt-in semantic retrieval retains local models and vector assets.
//! No lane starts writers, automatic refresh, downloads, or detached children.
//! Request deadlines terminate the worker, including stalled native calls.
//! Sampled resident-memory limits terminate an over-budget worker; they are
//! not kernel allocation limits or a peak-RSS guarantee between samples.

#[path = "search_service/admission.rs"]
mod admission;
#[path = "search_service/canonical.rs"]
mod canonical;
#[path = "search_service/deadline.rs"]
mod deadline;
#[path = "search_service/lexical_fallback.rs"]
mod lexical_fallback;
#[path = "search_service/mcp.rs"]
mod mcp;
#[path = "search_service/memory.rs"]
mod memory;
#[path = "search_service/once.rs"]
mod once;
#[path = "search_service/protocol.rs"]
mod protocol;
#[path = "search_service/refinement.rs"]
mod refinement;
#[path = "search_service/semantic.rs"]
mod semantic;
#[cfg(test)]
#[path = "search_service/tests.rs"]
mod tests;

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use coding_agent_search::search::query::{
    FieldMask, SearchClient, SearchClientOptions, SearchFilters, SearchHit,
};
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
    /// Reuse read-only search readers, or run one supervised --query and exit.
    /// Semantic retrieval and refinement require explicit opt-in. See docs/SEARCH_SERVICE.md.
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
            conflicts_with = "index",
            required_if_eq_any = [("mode", "semantic"), ("mode", "hybrid")]
        )]
        data_dir: Option<PathBuf>,
        /// Use newline-delimited JSON requests and responses on standard I/O.
        #[arg(
            long,
            required_unless_present_any = ["query", "transcript"],
            conflicts_with_all = ["query", "transcript"]
        )]
        stdio: bool,
        #[command(flatten)]
        once: once::Options,
        /// Speak MCP instead of the CASS JSON-lines protocol on the same stdio transport.
        #[arg(long, requires = "stdio")]
        mcp: bool,
        /// Opt in to canonical view requests against this fixed read-only archive.
        /// Search stays index-only unless --lexical-fallback is also supplied.
        #[arg(
            long,
            value_name = "PATH",
            required_if_eq_any = [("mode", "semantic"), ("mode", "hybrid")]
        )]
        db: Option<PathBuf>,
        /// Recover Quill fuel refusals using this explicit archive's SQLite lane.
        /// Admits at most 64 MiB of database plus WAL; shares the request deadline.
        /// Results identify the changed backend and are not Quill-score equivalent.
        #[arg(long, requires = "db")]
        lexical_fallback: bool,
        /// Opt in to candidate-only refinement with this installed local reranker.
        /// Never downloads files; ordinary search, startup and status stay model-free.
        #[arg(long, value_name = "PATH")]
        reranker_model: Option<PathBuf>,
        /// Opt in to retained semantic/hybrid retrieval using installed assets.
        /// Requires explicit --data-dir and --db; ordinary search stays lexical.
        #[arg(
            long,
            value_enum,
            requires_all = ["data_dir", "db"],
            conflicts_with = "index",
            required_if_eq_any = [("mode", "semantic"), ("mode", "hybrid")]
        )]
        semantic_embedder: Option<semantic::EmbedderChoice>,
        /// Whole-request deadline, including setup, search, teardown and output for --query.
        /// For --stdio, starts at the first frame byte; idle sessions are not timed out.
        /// Expiry exits the worker with code 124.
        #[arg(
            long,
            default_value_t = deadline::DEFAULT_TIMEOUT_MS,
            value_parser = deadline::parse_timeout_ms
        )]
        request_timeout_ms: u64,
        /// Shared local control directory for nonblocking cross-process reader admission.
        /// Writes only pool metadata; never place this directory inside a search index.
        #[arg(long, value_name = "PATH")]
        admission_dir: Option<PathBuf>,
        /// Maximum reader-owning processes sharing the same admission directory.
        #[arg(long, requires = "admission_dir", value_parser = admission::parse_slots)]
        admission_slots: Option<u32>,
        /// Terminate this worker with exit 126 when sampled resident memory exceeds this many MiB.
        /// Sampling includes idle readers; it is not a kernel-enforced allocation limit.
        #[arg(long, default_value_t = memory::DEFAULT_MAX_RESIDENT_MIB, value_parser = memory::parse_max_resident_mib)]
        max_resident_mib: u64,
    },
}

/// This independent parser follows the existing logical-archive dispatch seam:
/// no ordinary CLI readiness probe or runtime starts before serving requests.
/// The `serve` subcommand tree, for the main CLI's help and command schemas.
pub fn clap_commands() -> Vec<clap::Command> {
    use clap::CommandFactory;
    ServiceCli::command()
        .find_subcommand("serve")
        .cloned()
        .into_iter()
        .collect()
}

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
    let ServiceCommand::Serve {
        index,
        data_dir,
        stdio: _,
        once,
        mcp,
        db,
        lexical_fallback,
        reranker_model,
        semantic_embedder,
        request_timeout_ms,
        admission_dir,
        admission_slots,
        max_resident_mib,
    } = cli.command;
    let one_shot = once.request().map_err(once::invalid_request)?;
    // One budget covers ALL one-shot work, including the initial memory probe,
    // reader setup, optional recovery, native teardown and blocked output. This
    // guard stays alive until run returns; no stage gets a fresh deadline.
    let _one_shot_deadline = one_shot
        .as_ref()
        .map(|_| deadline::Deadline::start(Duration::from_millis(request_timeout_ms)))
        .transpose()
        .map_err(cli_io_error)?;
    let semantic_data_dir = data_dir.clone();
    let index = match (index, data_dir) {
        (Some(index), None) => index,
        (None, Some(data_dir)) => {
            coding_agent_search::search::tantivy::expected_index_dir(&data_dir)
        }
        _ => {
            return Err(coding_agent_search::CliError {
                code: 2,
                kind: "argument_parsing",
                message: "serve requires exactly one of --index or --data-dir".into(),
                hint: None,
                retryable: false,
            });
        }
    };
    let index = if index.is_absolute() {
        index
    } else {
        std::env::current_dir().map_err(cli_io_error)?.join(index)
    };
    let mut session = Session::new(index);
    session.lexical_recovery.enabled = lexical_fallback;
    session.request_timeout = Duration::from_millis(request_timeout_ms);
    if one_shot.is_some() {
        session.timeout_scope = "validated_arguments_through_reader_teardown_and_response_flush";
    }
    session.archive = db
        .map(|path| {
            if path.is_absolute() {
                Ok(path)
            } else {
                std::env::current_dir().map(|cwd| cwd.join(path))
            }
        })
        .transpose()
        .map_err(cli_io_error)?;
    if let (Some(embedder), Some(data_dir)) = (semantic_embedder, semantic_data_dir) {
        let data_dir = if data_dir.is_absolute() {
            data_dir
        } else {
            std::env::current_dir()
                .map_err(cli_io_error)?
                .join(data_dir)
        };
        session.semantic = semantic::Semantic::new(data_dir, embedder);
    }
    session.admission_pool = admission_dir
        .map(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            };
            admission::Pool::new(path, admission_slots.unwrap_or(admission::DEFAULT_SLOTS))
        })
        .transpose()
        .map_err(cli_io_error)?;
    // Start before serving any storage request and keep it through reader
    // teardown. The probe never opens the index, archive or pool directory.
    let memory_guard =
        memory::Guard::start(max_resident_mib).map_err(|error| coding_agent_search::CliError {
            code: memory::MONITOR_FAILURE_EXIT_CODE,
            kind: "memory-monitor-unavailable",
            message: error.to_string(),
            hint: Some(
                "Resident-memory supervision could not start; no storage reader was opened.".into(),
            ),
            retryable: false,
        })?;
    session.memory_observation = Some(memory_guard.observation());
    session.refiner = refinement::Refiner::new(
        reranker_model
            .map(|path| {
                if path.is_absolute() {
                    Ok(path)
                } else {
                    std::env::current_dir().map(|cwd| cwd.join(path))
                }
            })
            .transpose()
            .map_err(cli_io_error)?,
    );
    if let Some(request) = one_shot {
        let (mut reply, _) = session.handle(request);
        // Do not publish successful results before storage destruction has
        // completed. In particular, never reopen stdin or rearm the watchdog.
        drop(session);
        if !coding_agent_search::shutdown_thread_local_bridge_runtimes() {
            reply = Reply::failure(
                Some(1),
                "bridge_shutdown_failed",
                "search bridge teardown did not complete; no successful result was published",
            );
        }
        let result = once::publish(&reply, &mut io::stdout().lock(), &mut io::stderr().lock());
        drop(memory_guard);
        return result;
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let result = if mcp {
        mcp::serve_io(&mut session, &mut input, &mut output)
    } else {
        serve_io(&mut session, &mut input, &mut output)
    };
    // EOF and transport errors must also release native readers under a guard.
    // Ordinary Rust scope unwinding alone would leave destructor work unbounded.
    let _teardown = deadline::Deadline::start(session.request_timeout)
        .unwrap_or_else(|_| std::process::exit(deadline::WATCHDOG_FAILURE_EXIT_CODE));
    drop(session);
    // Cached bridge runtimes own scheduler threads even after the readers are
    // gone. Tear them down before either supervisor stops; main's later
    // idempotent shutdown must not be the first guarded attempt.
    let bridges_shutdown = coding_agent_search::shutdown_thread_local_bridge_runtimes();
    // Join the sampler while the teardown deadline still protects this process.
    drop(memory_guard);
    result.map_err(cli_io_error)?;
    if !bridges_shutdown {
        return Err(coding_agent_search::CliError {
            code: deadline::WATCHDOG_FAILURE_EXIT_CODE,
            kind: "bridge_shutdown_failed",
            message: "search service bridge teardown did not complete".into(),
            hint: None,
            retryable: false,
        });
    }
    Ok(())
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
    request_timeout: Duration,
    timeout_scope: &'static str,
    archive: Option<PathBuf>,
    memory_observation: Option<std::sync::Arc<memory::Observation>>,
    refiner: refinement::Refiner,
    semantic: semantic::Semantic,
    lexical_recovery: lexical_fallback::Recovery,
    canonical_read_attempts: u64,
    canonical_reads_completed: u64,
    client: Option<SearchClient>,
    // Declaration/drop order matters: reader destruction precedes lease release.
    reader_lease: Option<admission::Lease>,
    admission_pool: Option<admission::Pool>,
    open_attempts: u64,
    successful_opens: u64,
    queries_completed: u64,
}

impl Session {
    fn new(index: PathBuf) -> Self {
        Self {
            index,
            request_timeout: Duration::from_millis(deadline::DEFAULT_TIMEOUT_MS),
            timeout_scope: "first_frame_byte_through_response_flush",
            archive: None,
            memory_observation: None,
            refiner: refinement::Refiner::default(),
            semantic: semantic::Semantic::default(),
            lexical_recovery: lexical_fallback::Recovery::default(),
            canonical_read_attempts: 0,
            canonical_reads_completed: 0,
            client: None,
            reader_lease: None,
            admission_pool: None,
            open_attempts: 0,
            successful_opens: 0,
            queries_completed: 0,
        }
    }

    fn ensure_loaded(&mut self) -> Result<()> {
        if self.client.is_none() {
            let lease = if self.reader_lease.is_none() {
                self.acquire_reader_lease()?
            } else {
                None
            };
            self.open_attempts = self.open_attempts.saturating_add(1);
            let client = open_snapshot(&self.index)?;
            self.successful_opens = self.successful_opens.saturating_add(1);
            self.client = Some(client);
            if lease.is_some() {
                self.reader_lease = lease;
            }
        }
        Ok(())
    }

    fn acquire_reader_lease(&self) -> Result<Option<admission::Lease>> {
        self.admission_pool
            .as_ref()
            .map(admission::Pool::acquire)
            .transpose()
            .map_err(Into::into)
    }

    fn unload(&mut self) {
        // Keep the shared slot until BOTH model and index owners are released.
        self.refiner.unload();
        self.semantic.unload();
        drop(self.client.take());
        drop(self.reader_lease.take());
    }

    fn status(&self) -> Value {
        let memory = self.memory_observation.as_ref().map(|observation| {
            let sample = observation.sample();
            json!({
                "enabled": true,
                "scope": "this_process_including_idle_and_teardown",
                "measurement": "sampled_resident_bytes",
                "limit_bytes": sample.limit_bytes,
                "sampled_bytes": sample.current_bytes,
                "sampled_peak_bytes": sample.peak_bytes,
                "sample_age_ms": sample.age_ms,
                "samples": sample.samples,
                "sample_interval_ms": memory::SAMPLE_INTERVAL_MS,
                "limit_exit_code": memory::MEMORY_EXIT_CODE,
                "monitor_failure_exit_code": memory::MONITOR_FAILURE_EXIT_CODE,
                "kernel_enforced": false,
            })
        });
        json!({
            "service": "cass_lexical_stdio",
            "mode": "lexical",
            "default_operation": "search",
            "loaded": self.client.is_some(),
            // A session-local epoch is NOT an archive generation certificate.
            "reader_epoch": self.client.as_ref().map(|_| self.successful_opens),
            "open_attempts": self.open_attempts,
            "successful_opens": self.successful_opens,
            "queries_completed": self.queries_completed,
            "snapshot_policy": "pinned_until_reload",
            "freshness": "not_checked",
            "canonical_view_enabled": self.archive.is_some(),
            "canonical_database_accessed": self.canonical_read_attempts != 0
                || self.semantic.archive_access_attempted()
                || self.lexical_recovery.archive_access_attempted(),
            "canonical_read_attempts": self.canonical_read_attempts,
            "canonical_reads_completed": self.canonical_reads_completed,
            "models_loaded": self.refiner.loaded() || self.semantic.model_loaded(),
            "refinement": self.refiner.status(),
            "semantic": self.semantic.status(),
            "lexical_fallback": self.lexical_recovery.status(),
            "maintenance_performed": false,
            "memory_supervision": memory.unwrap_or_else(|| json!({"enabled": false})),
            "reader_admission": {
                "enabled": self.admission_pool.is_some(),
                "lease_held": self.reader_lease.is_some(),
                "slots": self.admission_pool.as_ref().map(admission::Pool::slots),
                "scope": "cooperating_processes_using_one_local_pool",
                "bounds_total_rss": false,
            },
            "limits": {
                "request_bytes": protocol::MAX_REQUEST_BYTES,
                "response_bytes": protocol::MAX_RESPONSE_BYTES,
                "query_bytes": protocol::MAX_QUERY_BYTES,
                "limit": protocol::MAX_LIMIT,
                "page_window": protocol::MAX_WINDOW,
                "canonical_context": canonical::MAX_CONTEXT,
                "canonical_content_bytes": canonical::MAX_CONTENT_BYTES,
                "canonical_batch_views": canonical::MAX_BATCH_VIEWS,
                "canonical_batch_messages": canonical::MAX_BATCH_MESSAGES,
                "canonical_batch_content_bytes": canonical::MAX_CONTENT_BYTES,
                "transcript_messages": canonical::transcript::MAX_MESSAGES,
                "transcript_content_bytes": canonical::transcript::MAX_PAGE_BYTES,
                "request_timeout_ms": self.request_timeout.as_millis(),
                "timeout_exit_code": deadline::TIMEOUT_EXIT_CODE,
                "timeout_scope": self.timeout_scope,
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
        let client = self
            .client
            .as_ref()
            .context("search reader is unavailable")?;
        let started = Instant::now();
        // Only engine-owned filters are exposed. In particular session_paths
        // and the synthetic 'remote' group currently trigger a potentially
        // corpus-sized post-filter retry in SearchClient; this bounded service
        // does not advertise them. source_id always means one literal ID.
        let filters = SearchFilters {
            agents: filters.agents.into_iter().collect(),
            workspaces: filters.workspaces.into_iter().collect(),
            source_filter: filters
                .source_id
                .map_or(SourceFilter::All, SourceFilter::SourceId),
            created_from: filters.created_from,
            created_to: filters.created_to,
            session_paths: HashSet::new(),
        };
        // No payload hydration and no query-prefix response cache: every call
        // uses the production lexical parser, scorer and result reducer against
        // the SAME admitted reader, without opening a canonical archive.
        let primary = client.search(
            query,
            filters.clone(),
            limit + 1,
            offset,
            FieldMask::new(false, true, true, false),
        );
        // A valid empty primary result remains authoritative. Only a work-
        // budget refusal enters the explicit fallback, including a refusal in
        // SearchClient's internal overfetch or federated search. Never restart
        // the request deadline or increase the engine's fuel allowance here.
        let (mut hits, fallback) = match primary {
            Ok(hits) => (hits, None),
            Err(error) => {
                let page = self.lexical_recovery.recover(
                    self.archive.as_deref(),
                    error,
                    lexical_fallback::Search {
                        query,
                        filters,
                        limit: limit + 1,
                        offset,
                    },
                )?;
                (page.hits, Some(page.metadata))
            }
        };
        let degraded = fallback.is_some();
        ensure!(
            hits.len() <= limit + 1,
            "search backend exceeded the requested page"
        );
        let has_next = hits.len() > limit;
        let next_offset = offset.checked_add(limit).filter(|next| {
            has_next
                && next
                    .checked_add(limit)
                    .and_then(|n| n.checked_add(1))
                    .is_some_and(|n| n <= protocol::MAX_WINDOW)
        });
        hits.truncate(limit);
        let summaries = summarize_hits(hits)?;
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
            "backend": if degraded { "frankensqlite" } else { "quill" },
            "degraded": degraded,
            "fallback": fallback,
            "reader_reused": reused && !degraded,
            "result_reader_epoch": if degraded { None } else { Some(self.successful_opens) },
            "setup_ms": setup_ms,
            "search_ms": started.elapsed().as_millis(),
            "snapshot": self.status(),
        }))
    }

    fn handle(&mut self, request: Request) -> (Reply, bool) {
        match request {
            Request::Transcript {
                id,
                source_path,
                source_id,
                conversation_id,
                cursor,
                limit,
            } => {
                let page = canonical::transcript::Page {
                    source_path: &source_path,
                    source_id: &source_id,
                    conversation_id,
                    cursor: cursor.as_ref(),
                    limit,
                };
                if let Err(message) = page.validate() {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                let Some(db) = &self.archive else {
                    return (
                        Reply::failure(
                            Some(id),
                            "canonical_access_disabled",
                            "transcript requires an explicit --db at service startup",
                        ),
                        false,
                    );
                };
                // Hold admission through this page's reader teardown, but never
                // pin an archive transaction while waiting for another request.
                let _lease = if self.reader_lease.is_none() {
                    match self.acquire_reader_lease() {
                        Ok(lease) => lease,
                        Err(error) => {
                            return (
                                Reply::failure(
                                    Some(id),
                                    admission_error_kind(&error, "admission_unavailable"),
                                    format!("{error:#}"),
                                ),
                                false,
                            );
                        }
                    }
                } else {
                    None
                };
                self.canonical_read_attempts = self.canonical_read_attempts.saturating_add(1);
                let reply = match canonical::transcript::read(db, &page) {
                    Ok(result) => {
                        self.canonical_reads_completed =
                            self.canonical_reads_completed.saturating_add(1);
                        Reply::success(id, result)
                    }
                    Err(error) => Reply::failure(
                        Some(id),
                        canonical::transcript::error_kind(&error),
                        format!("{error:#}"),
                    ),
                };
                (reply, false)
            }
            Request::ViewBatch { id, views } => {
                if views.is_empty() || views.len() > canonical::MAX_BATCH_VIEWS {
                    return (
                        Reply::failure(
                            Some(id),
                            "invalid_request",
                            "views must contain between 1 and 8 canonical windows",
                        ),
                        false,
                    );
                }
                let requests = views
                    .iter()
                    .map(protocol::ViewSelection::view)
                    .collect::<Vec<_>>();
                if let Err(message) = canonical::validate_batch(&requests) {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                let Some(db) = &self.archive else {
                    return (
                        Reply::failure(
                            Some(id),
                            "canonical_access_disabled",
                            "canonical batch access requires an explicit --db at service startup",
                        ),
                        false,
                    );
                };
                // Reuse the held index lease, or retain one temporary lease
                // through the entire batch's archive open, reads and teardown.
                let _view_lease = if self.reader_lease.is_none() {
                    match self.acquire_reader_lease() {
                        Ok(lease) => lease,
                        Err(error) => {
                            return (
                                Reply::failure(
                                    Some(id),
                                    admission_error_kind(&error, "admission_unavailable"),
                                    format!("{error:#}"),
                                ),
                                false,
                            );
                        }
                    }
                } else {
                    None
                };
                // Counters measure archive opens/transactions, not the number
                // of requested windows. There is only one reader in this batch.
                self.canonical_read_attempts = self.canonical_read_attempts.saturating_add(1);
                let reply = match canonical::read_batch(db, &requests) {
                    Ok(result) => {
                        self.canonical_reads_completed =
                            self.canonical_reads_completed.saturating_add(1);
                        Reply::success(id, result)
                    }
                    Err(error) => Reply::failure(
                        Some(id),
                        canonical::error_kind(&error),
                        format!("{error:#}"),
                    ),
                };
                (reply, false)
            }
            Request::Refine {
                id,
                query,
                lexical_query,
                filters,
                candidate_limit,
                limit,
            } => {
                if let Err(message) =
                    refinement::validate(&query, &lexical_query, &filters, candidate_limit, limit)
                {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                if !self.refiner.enabled() {
                    return (
                        Reply::failure(
                            Some(id),
                            "refinement_disabled",
                            "refinement requires --reranker-model at startup; ordinary search remains lexical",
                        ),
                        false,
                    );
                }
                let reply =
                    match self.refine(&query, &lexical_query, filters, candidate_limit, limit) {
                        Ok(result) => Reply::success(id, result),
                        Err(error) => Reply::failure(
                            Some(id),
                            admission_error_kind(&error, "refinement_failed"),
                            format!("{error:#}"),
                        ),
                    };
                (reply, false)
            }
            Request::View {
                id,
                source_path,
                source_id,
                conversation_id,
                message_index,
                context,
            } => {
                let request = canonical::View {
                    source_path: &source_path,
                    source_id: &source_id,
                    conversation_id,
                    message_index,
                    context,
                };
                if let Err(message) = request.validate() {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                let Some(db) = &self.archive else {
                    return (
                        Reply::failure(
                            Some(id),
                            "canonical_access_disabled",
                            "canonical view requires an explicit --db at service startup; search remains index-only",
                        ),
                        false,
                    );
                };
                // A loaded lexical reader already holds this process's lease.
                // Otherwise cover the entire per-view reader lifetime temporarily.
                let _view_lease = if self.reader_lease.is_none() {
                    match self.acquire_reader_lease() {
                        Ok(lease) => lease,
                        Err(error) => {
                            return (
                                Reply::failure(
                                    Some(id),
                                    admission_error_kind(&error, "admission_unavailable"),
                                    format!("{error:#}"),
                                ),
                                false,
                            );
                        }
                    }
                } else {
                    None
                };
                self.canonical_read_attempts = self.canonical_read_attempts.saturating_add(1);
                let reply = match canonical::read(db, &request) {
                    Ok(result) => {
                        self.canonical_reads_completed =
                            self.canonical_reads_completed.saturating_add(1);
                        Reply::success(id, result)
                    }
                    Err(error) => Reply::failure(
                        Some(id),
                        canonical::error_kind(&error),
                        format!("{error:#}"),
                    ),
                };
                (reply, false)
            }
            Request::Status { id } => (Reply::success(id, self.status()), false),
            Request::Unload { id } => {
                self.unload();
                (Reply::success(id, self.status()), false)
            }
            Request::Shutdown { id } => {
                self.unload();
                (Reply::success(id, json!({"shutdown": true})), true)
            }
            Request::Reload { id } => {
                // Drop first: never retain two multi-GB generations during
                // reload. A failed reload leaves no silently stale fallback.
                self.unload();
                let started = Instant::now();
                let reply = match self.ensure_loaded() {
                    Ok(()) => Reply::success(
                        id,
                        json!({
                            "setup_ms": started.elapsed().as_millis(),
                            "snapshot": self.status(),
                        }),
                    ),
                    Err(error) => Reply::failure(
                        Some(id),
                        admission_error_kind(&error, "index_unavailable"),
                        format!("{error:#}"),
                    ),
                };
                (reply, false)
            }
            Request::SemanticSearch {
                id,
                query,
                mode,
                approximate,
                limit,
                offset,
                filters,
            } => {
                if let Err(message) = protocol::validate_search(&query, limit, offset, &filters) {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                if !self.semantic.enabled() {
                    return (
                        Reply::failure(
                            Some(id),
                            "semantic_disabled",
                            "semantic_search requires --semantic-embedder, --data-dir and --db at startup",
                        ),
                        false,
                    );
                }
                let reply =
                    match self.semantic_search(&query, mode, approximate, filters, limit, offset) {
                        Ok(result) => Reply::success(id, result),
                        Err(error) => Reply::failure(
                            Some(id),
                            admission_error_kind(&error, semantic::error_kind(&error)),
                            format!("{error:#}"),
                        ),
                    };
                (reply, false)
            }
            Request::Search {
                id,
                query,
                limit,
                offset,
                filters,
            } => {
                // Invalid work must not load an index or allocate from caller k.
                if let Err(message) = protocol::validate_search(&query, limit, offset, &filters) {
                    return (Reply::failure(Some(id), "invalid_request", message), false);
                }
                let reply = match self.search(&query, filters, limit, offset) {
                    Ok(result) => Reply::success(id, result),
                    Err(error) => Reply::failure(
                        Some(id),
                        admission_error_kind(&error, "search_failed"),
                        format!("{error:#}"),
                    ),
                };
                (reply, false)
            }
        }
    }
}

fn admission_error_kind(error: &anyhow::Error, fallback: &'static str) -> &'static str {
    error
        .downcast_ref::<admission::Refusal>()
        .map_or(fallback, admission::Refusal::kind)
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
    value
        .char_indices()
        .nth(maximum_chars)
        .map_or(value, |(at, _)| &value[..at])
}

fn summarize_hits(hits: Vec<SearchHit>) -> Result<Vec<Value>> {
    let mut summaries = Vec::with_capacity(hits.len());
    for hit in hits {
        ensure!(
            hit.score.is_finite(),
            "search backend returned a non-finite score"
        );
        ensure!(
            hit.line_number != Some(0),
            "search backend returned an invalid message ordinal"
        );
        for value in [
            Some(hit.source_path.as_str()),
            Some(hit.source_id.as_str()),
            Some(hit.agent.as_str()),
            Some(hit.workspace.as_str()),
            Some(hit.origin_kind.as_str()),
            hit.origin_host.as_deref(),
            hit.workspace_original.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                value.len() <= MAX_IDENTITY_BYTES,
                "search identity exceeds the service's 4096-byte bound; identities are never truncated"
            );
        }
        summaries.push(json!({
            "title": prefix(&hit.title, 256), "snippet": prefix(&hit.snippet, 800),
            "score": hit.score, "source_path": hit.source_path, "source_id": hit.source_id,
            "conversation_id": hit.conversation_id, "message_index": hit.line_number,
            "agent": hit.agent, "workspace": hit.workspace,
            "workspace_original": hit.workspace_original, "created_at": hit.created_at,
            "match_type": hit.match_type, "origin_kind": hit.origin_kind, "origin_host": hit.origin_host,
        }));
    }
    Ok(summaries)
}

fn serve_io(
    session: &mut Session,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<()> {
    loop {
        let Some(_request_deadline) =
            deadline::Deadline::for_frame(input, session.request_timeout)?
        else {
            return Ok(());
        };
        let bytes = match protocol::read_frame(input)? {
            Frame::End => return Ok(()),
            Frame::TooLarge => {
                protocol::write_reply(
                    output,
                    &Reply::failure(
                        None,
                        "request_too_large",
                        "request exceeds 64 KiB; this session is closing without processing the remainder",
                    ),
                )?;
                return Ok(());
            }
            Frame::Line(bytes) => bytes,
        };
        let request = match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) => request,
            Err(error) => {
                protocol::write_reply(
                    output,
                    &Reply::failure(None, "invalid_request", error.to_string()),
                )?;
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
