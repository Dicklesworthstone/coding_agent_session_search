// `unsafe` is denied crate-wide outside tests; the only sanctioned sites carry
// `#[allow(unsafe_code)]` + a SAFETY comment (startup env writes, unavoidable FFI per AGENTS.md).
#![cfg_attr(not(test), deny(unsafe_code))]

mod codex_layout_cli;
mod lexical_quarantine_cli;
mod logical_archive;
mod raw_mirror_cli;
mod search_service;
pub mod semantic_reclaim_cli;

fn env_requests_robot_output() -> bool {
    let cass_output_format = dotenvy::var("CASS_OUTPUT_FORMAT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| {
            matches!(
                value.as_str(),
                "json" | "jsonl" | "compact" | "sessions" | "toon"
            )
        });
    let toon_default_format = dotenvy::var("TOON_DEFAULT_FORMAT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "json" | "toon"));
    cass_output_format || toon_default_format
}

fn is_robot_format_name(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    matches!(
        value.as_str(),
        "json" | "jsonl" | "compact" | "sessions" | "toon"
    )
}

fn raw_command_name(args: &[String]) -> Option<&str> {
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();

        if arg == "--" {
            return args.get(index + 1).map(String::as_str);
        }

        if matches!(
            arg,
            "--color"
                | "--progress"
                | "--wrap"
                | "--db"
                | "--trace-file"
                | "--data-dir"
                | "--stale-threshold"
                | "--robot-format"
                | "--format"
                | "--output"
                | "--output-format"
                | "--output_format"
        ) {
            index += 2;
            continue;
        }

        if arg.starts_with("--color=")
            || arg.starts_with("--progress=")
            || arg.starts_with("--wrap=")
            || arg.starts_with("--db=")
            || arg.starts_with("--trace-file=")
            || arg.starts_with("--data-dir=")
            || arg.starts_with("--stale-threshold=")
            || arg.starts_with("--robot-format=")
            || arg.starts_with("--format=")
            || arg.starts_with("--output=")
            || arg.starts_with("--output-format=")
            || arg.starts_with("--output_format=")
        {
            index += 1;
            continue;
        }

        if matches!(
            arg,
            "--json"
                | "--robot"
                | "-json"
                | "-robot"
                | "--nowrap"
                | "--quiet"
                | "-q"
                | "--verbose"
                | "-v"
                | "--robot-help"
        ) {
            index += 1;
            continue;
        }

        if arg.starts_with('-') {
            index += 1;
            continue;
        }

        return Some(arg);
    }
    None
}

fn is_robot_mode_args() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let command_name = raw_command_name(&args);
    for (index, arg) in args.iter().enumerate() {
        if matches!(arg.as_str(), "--json" | "--robot" | "-json" | "-robot") {
            return true;
        }
        if arg == "--robot-format" || arg.starts_with("--robot-format=") {
            return true;
        }
        if let Some(value) = arg.strip_prefix("--format=")
            && is_robot_format_name(value)
            && command_name != Some("export")
        {
            return true;
        }
        if let Some(value) = arg
            .strip_prefix("--output=")
            .or_else(|| arg.strip_prefix("--output-format="))
            .or_else(|| arg.strip_prefix("--output_format="))
            && is_robot_format_name(value)
            && command_name != Some("export")
        {
            return true;
        }
        if arg == "--format"
            && args
                .get(index + 1)
                .is_some_and(|value| is_robot_format_name(value))
            && command_name != Some("export")
        {
            return true;
        }
        if matches!(
            arg.as_str(),
            "--output" | "--output-format" | "--output_format"
        ) && args
            .get(index + 1)
            .is_some_and(|value| is_robot_format_name(value))
            && command_name != Some("export")
        {
            return true;
        }
    }
    env_requests_robot_output()
}

fn handle_fatal_error(err: coding_agent_search::CliError) -> ! {
    if err.was_already_reported() {
        std::process::exit(err.code);
    }

    // Robot-mode success payloads use stdout; fatal diagnostics, including
    // structured error envelopes, stay on stderr so stdout remains data-only.
    if err.message.trim().starts_with('{') {
        // Pre-formatted JSON error envelope from a robot-mode subcommand.
        eprintln!("{}", err.message);
    } else if is_robot_mode_args() {
        // Wrap unstructured error for robot consumers.
        let payload = serde_json::json!({
            "error": {
                "code": err.code,
                "kind": err.kind,
                "message": err.message,
                "hint": err.hint,
                "retryable": err.retryable,
            }
        });
        eprintln!("{payload}");
    } else {
        // Human-readable output stays on stderr per Unix convention. The
        // remedy goes on its own line: a person needs it as much as a robot
        // envelope does, and without it a missing index or an exhausted query
        // budget (GH #518) never named the command that fixes it.
        eprintln!("{}", err.message);
        if let Some(hint) = err.hint.as_deref().map(str::trim)
            && !hint.is_empty()
            && !err.message.contains(hint)
        {
            eprintln!("Hint: {hint}");
        }
    }
    std::process::exit(err.code);
}

#[allow(unsafe_code)]
fn apply_default_tantivy_writer_thread_cap() {
    let configured = dotenvy::var("CASS_TANTIVY_MAX_WRITER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);
    if configured.is_none() {
        // Keep explicit operator tuning authoritative, otherwise use the same
        // memory-aware default as the search layer before frankensearch opens
        // any Tantivy writers.
        let default_cap =
            coding_agent_search::search::tantivy::default_tantivy_max_writer_threads();
        // SAFETY: single-threaded startup before any runtime thread exists.
        unsafe {
            std::env::set_var("CASS_TANTIVY_MAX_WRITER_THREADS", default_cap.to_string());
        }
    }
}

/// Bound the frankensqlite per-cursor `read_witnesses` Vec so a long B-tree
/// descent against a multi-GB index cannot balloon RSS into the multi-GB range.
///
/// The frankensqlite default is `0` ("unbounded") to preserve historical SSI
/// provenance semantics. cass is a read-mostly analytical workload that does
/// not need the per-cursor witness cache — the canonical SSI evidence still
/// flows into the pager regardless of this cap, so the cap is safe to apply
/// here without weakening isolation.
///
/// Issue #252 reproduced this regression on v0.5.1: `SELECT COUNT(*)` over a
/// 3.3 GB index allocated ~5.5 GB RSS because the cursor's `read_witnesses`
/// vec grew one entry per page touched. Capping at 16384 keeps the cursor
/// cache well under a few MB while leaving the SSI source of truth intact.
///
/// Operators who need full per-cursor provenance can override by exporting
/// `FSQLITE_READ_WITNESS_CAP=0` (or any value) before launching cass.
#[allow(unsafe_code)]
fn apply_default_fsqlite_read_witness_cap() {
    // The env var is parsed once by frankensqlite at first cursor construction
    // and cached in a process-wide OnceLock, so a later `set_var` after a
    // cursor opens would have no effect. We must set it here, in main, before
    // any code path that touches the SQL store. Use `var_os` so we don't
    // accidentally clobber an explicit operator override (including the empty
    // string, which is meaningful to operators who want frankensqlite to
    // observe and reject the value rather than fall back to our default).
    if std::env::var_os("FSQLITE_READ_WITNESS_CAP").is_none() {
        // SAFETY: set_var is sound at single-threaded program startup, which
        // is exactly where this runs (main, before any runtime is built).
        unsafe {
            std::env::set_var("FSQLITE_READ_WITNESS_CAP", "16384");
        }
    }
}

/// Keep serialized archive reads off the CPU-sized outer scheduler pool.
///
/// Timeline and sessions execute their SQL through the thread-affine sync
/// bridge; extra outer workers cannot parallelize those reads. Use the same
/// current-thread driver as search/health without bypassing normal parsing,
/// startup, command dispatch, error reporting, or bridge/runtime shutdown.
/// Indexing and every other command retain the multithreaded runtime.
fn command_uses_current_thread(command: Option<&coding_agent_search::Commands>) -> bool {
    matches!(
        command,
        Some(
            coding_agent_search::Commands::Search { .. }
                | coding_agent_search::Commands::Health { .. }
                | coding_agent_search::Commands::Timeline { .. }
                | coding_agent_search::Commands::Sessions { .. }
        )
    )
}

fn main() -> anyhow::Result<()> {
    // (cass #356) Restore the default SIGPIPE disposition so an early-exiting
    // pipe consumer (`cass ... --json | head`) terminates cass quietly with
    // the conventional 141 instead of panicking on EPIPE — which, under the
    // release profile's panic=abort, dumps a SIGABRT crash report per run.
    // Daemon/socket I/O is unaffected: Rust's std sends with MSG_NOSIGNAL on
    // Linux and sets SO_NOSIGPIPE on Apple platforms, so only pipe/stdio
    // writes regain the default kill-on-EPIPE behavior.
    #[cfg(unix)]
    sigpipe::reset();

    // Offline recovery must work even when normal startup cannot open the
    // archive. Use the same handler as the companion, before environment-driven
    // setup, provider initialization, database probes, or runtime construction.
    if let Some(args) = semantic_reclaim_cli::cass_args(&std::env::args_os().collect::<Vec<_>>()) {
        let code = semantic_reclaim_cli::run_cass(args);
        // The handler has finished and released its locks before process exit.
        if code != 0 {
            std::process::exit(i32::from(code));
        }
        return Ok(());
    }

    // (cass #308) The semantic stack is now pure-Rust (frankensearch `native`
    // frankentorch embedder + reranker), so there is no prebuilt ONNX Runtime and
    // no AVX/AVX2 static-init hazard. The old pre-AVX2 preflight + `-baseline`
    // artifact (#256/#307) are obsolete: a single binary runs on any x86_64 CPU,
    // with NEON (aarch64) / AVX2+FMA-when-present (x86) selected at runtime by the
    // int8 GEMM kernels.

    // Load .env early; ignore if missing.
    dotenvy::dotenv().ok();

    // Apply cass-tuned defaults before any code path constructs a frankensqlite
    // cursor (which caches the FSQLITE_READ_WITNESS_CAP value once and ignores
    // later mutations). The Health fast path below may open the SQL store, so
    // this must run before try_run_with_parsed_fast.
    apply_default_fsqlite_read_witness_cap();

    // Independent commands are dispatched before the main CLI parses;
    // registering their trees lets help, completions, the man page,
    // capabilities and introspect list them. Built only when one of those
    // asks.
    coding_agent_search::register_external_subcommands(|| {
        let mut commands = search_service::clap_commands();
        commands.extend(logical_archive::clap_commands());
        commands.extend(codex_layout_cli::clap_commands());
        commands.extend(lexical_quarantine_cli::clap_commands());
        commands.extend(semantic_reclaim_cli::clap_commands());
        commands
    });

    let raw_args: Vec<String> = std::env::args().collect();
    if raw_command_name(&raw_args) == Some("codex-layout-migrate") {
        // The proof owns the archive/receipt locks. Do not enter ordinary
        // startup, automatic maintenance or model initialization on this path.
        let result = codex_layout_cli::run(raw_args);
        if !coding_agent_search::shutdown_thread_local_bridge_runtimes() {
            tracing::warn!("Codex migration bridge teardown exceeded its deadline");
        }
        return match result {
            Ok(()) => Ok(()),
            Err(error) => handle_fatal_error(error),
        };
    }
    if raw_command_name(&raw_args) == Some("lexical-quarantine") {
        return match lexical_quarantine_cli::run(raw_args) {
            Ok(()) => Ok(()),
            Err(error) => handle_fatal_error(error),
        };
    }
    if raw_command_name(&raw_args) == Some("archive") {
        // Explicit interchange stays outside ordinary search/maintenance setup.
        let result = logical_archive::run(raw_args);
        if !coding_agent_search::shutdown_thread_local_bridge_runtimes() {
            tracing::warn!("logical archive bridge teardown exceeded its deadline");
        }
        return match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let (code, kind, retryable) = logical_archive::classify_failure(&error);
                let payload = serde_json::json!({
                    "error": {
                        "code": code,
                        "kind": kind,
                        "message": error.to_string(),
                        "retryable": retryable
                    }
                });
                eprintln!("{payload}");
                std::process::exit(code);
            }
        };
    }
    if raw_command_name(&raw_args) == Some("serve") {
        let result = search_service::run(raw_args);
        if !coding_agent_search::shutdown_thread_local_bridge_runtimes() {
            tracing::warn!("search service bridge teardown exceeded its deadline");
        }
        return match result {
            Ok(()) => Ok(()),
            Err(err) => handle_fatal_error(err),
        };
    }
    let parsed = match coding_agent_search::parse_cli(raw_args) {
        Ok(parsed) => parsed,
        Err(err) => handle_fatal_error(err),
    };

    let parsed = match coding_agent_search::try_run_with_parsed_fast(parsed) {
        Ok(result) => {
            return match result {
                Ok(()) => Ok(()),
                Err(err) => handle_fatal_error(err),
            };
        }
        Err(parsed) => *parsed,
    };

    apply_default_tantivy_writer_thread_cap();
    raw_mirror_cli::maintain(&parsed.cli);

    let runtime = if command_uses_current_thread(parsed.cli.command.as_ref()) {
        asupersync::runtime::RuntimeBuilder::current_thread().build()?
    } else {
        asupersync::runtime::RuntimeBuilder::multi_thread().build()?
    };

    let result = runtime.block_on(coding_agent_search::run_with_parsed(parsed));
    // Tear down scheduler threads while normal thread operations are still
    // available. On Windows, deferring a cached bridge runtime's final drop to
    // the CRT thread-local destructor phase makes JoinHandle::join fail with
    // `threads should not terminate unexpectedly` after a successful command.
    let bridges_shutdown = coding_agent_search::shutdown_thread_local_bridge_runtimes();
    let runtime_shutdown = runtime.shutdown_timeout(std::time::Duration::from_secs(30));
    if !bridges_shutdown || !runtime_shutdown {
        tracing::warn!("asupersync runtime teardown exceeded 30 seconds");
    }

    match result {
        Ok(()) => Ok(()),
        Err(err) => handle_fatal_error(err),
    }
}

#[cfg(test)]
mod command_runtime_tests {
    use super::command_uses_current_thread;
    use clap::Parser;
    use coding_agent_search::Cli;

    fn current_thread(args: &[&str]) -> bool {
        let cli = Cli::try_parse_from(args).expect("valid CLI runtime-policy fixture");
        command_uses_current_thread(cli.command.as_ref())
    }

    #[test]
    fn timeline_and_sessions_use_the_serial_read_runtime() {
        let cases: &[&[&str]] = &[
            &["cass", "timeline"],
            &["cass", "timeline", "--today", "--json"],
            &["cass", "timeline", "--since", "5m", "--json"],
            &[
                "cass",
                "timeline",
                "--agent",
                "claude_code",
                "--source",
                "local",
                "--group-by",
                "hour",
                "--robot-format",
                "compact",
            ],
            &["cass", "--quiet", "--robot-format", "json", "timeline"],
            &["cass", "sessions"],
        ];
        for args in cases {
            assert!(current_thread(args), "serialized read: {args:?}");
        }
    }

    #[test]
    fn existing_search_and_health_runtime_policy_is_preserved() {
        for args in [&["cass", "search", "probe"][..], &["cass", "health"][..]] {
            assert!(current_thread(args), "existing current-thread path: {args:?}");
        }
    }

    #[test]
    fn indexing_watch_and_default_paths_keep_the_parallel_runtime() {
        for args in [
            &["cass", "index"][..],
            &["cass", "index", "--full"][..],
            &["cass", "index", "--watch"][..],
            &["cass", "tui", "--once"][..],
            &["cass"][..],
        ] {
            assert!(!current_thread(args), "parallel path: {args:?}");
        }
    }
}
