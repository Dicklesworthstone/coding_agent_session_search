pub mod analytics;
pub mod bakeoff;
pub mod bookmarks;
pub mod connectors;
#[cfg(unix)]
pub mod daemon;
pub mod encryption;
pub mod export;
pub mod html_export;
pub mod indexer;
pub mod model;
pub mod pages;
pub mod search;
pub mod sources;
pub mod storage;
pub mod tui_asciicast;
pub mod ui;
pub mod update_check;

use anyhow::Result;
use base64::prelude::*;
use chrono::Utc;
use clap::{Arg, ArgAction, Command, CommandFactory, Parser, Subcommand, ValueEnum, ValueHint};
use indexer::IndexOptions;
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

const CONTRACT_VERSION: &str = "1";
const DEFAULT_STALE_THRESHOLD_SECS: u64 = 1800;

fn read_watch_once_paths_env() -> Option<Vec<std::path::PathBuf>> {
    dotenvy::var("CASS_TEST_WATCH_PATHS")
        .ok()
        .map(|list| {
            list.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
}

/// Command-line interface.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "cass",
    version,
    about = "Unified TUI search over coding agent histories"
)]
pub struct Cli {
    /// Path to the `SQLite` database (defaults to platform data dir)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Deterministic machine-first help (wide, no TUI)
    #[arg(long, default_value_t = false)]
    pub robot_help: bool,

    /// Trace command execution to JSONL file (spans)
    #[arg(long, env = "CASS_TRACE_FILE")]
    pub trace_file: Option<PathBuf>,

    /// Reduce log noise (warnings and errors only)
    #[arg(long, short = 'q', default_value_t = false)]
    pub quiet: bool,

    /// Increase verbosity (show debug information)
    #[arg(long, short = 'v', default_value_t = false)]
    pub verbose: bool,

    /// Color behavior for CLI output
    #[arg(long, value_enum, default_value_t = ColorPref::Auto)]
    pub color: ColorPref,

    /// Progress output style
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    pub progress: ProgressMode,

    /// Wrap informational output to N columns
    #[arg(long)]
    pub wrap: Option<usize>,

    /// Disable wrapping entirely
    #[arg(long, default_value_t = false)]
    pub nowrap: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Launch interactive TUI
    Tui {
        /// Render once and exit (headless-friendly)
        #[arg(long, default_value_t = false)]
        once: bool,

        /// Delete persisted UI state (`tui_state.json`) before launch
        #[arg(long, default_value_t = false)]
        reset_state: bool,

        /// Record terminal output to asciicast v2 file (for demos/bug repro)
        #[arg(long, value_hint = ValueHint::FilePath)]
        asciicast: Option<PathBuf>,

        /// Override data dir (matches index --data-dir)
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Run in inline mode (UI anchored within terminal, scrollback preserved)
        #[arg(long, default_value_t = false)]
        inline: bool,

        /// Height of the inline UI in rows (default: 12, ignored without --inline)
        #[arg(long, default_value_t = 12)]
        ui_height: u16,

        /// Anchor the inline UI to top or bottom of the terminal (default: bottom)
        #[arg(long, value_parser = ["top", "bottom"], default_value = "bottom")]
        anchor: String,

        /// Record input events to a macro file for replay/debugging
        #[arg(long, value_hint = ValueHint::FilePath)]
        record_macro: Option<PathBuf>,

        /// Play back a previously recorded macro file
        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "record_macro")]
        play_macro: Option<PathBuf>,
    },
    /// Run indexer
    Index {
        /// Perform full rebuild
        #[arg(long)]
        full: bool,

        /// Force Tantivy index rebuild even if schema matches
        #[arg(long, default_value_t = false, visible_alias = "force")]
        force_rebuild: bool,

        /// Watch for changes and reindex automatically
        #[arg(long)]
        watch: bool,

        /// Trigger a single watch cycle for specific paths (comma-separated or repeated)
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        watch_once: Option<Vec<PathBuf>>,

        /// Build semantic vector index after text indexing
        #[arg(long)]
        semantic: bool,

        /// Build HNSW index for approximate nearest neighbor search (requires --semantic).
        /// Enables O(log n) search with `--approximate` flag at query time.
        #[arg(long, default_value_t = false)]
        build_hnsw: bool,

        /// Embedder to use for semantic indexing (hash, fastembed)
        #[arg(long, default_value = "fastembed")]
        embedder: String,

        /// Override data dir (index + db). Defaults to platform data dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Output as JSON (for automation)
        #[arg(long, visible_alias = "robot")]
        json: bool,

        /// Idempotency key for safe retries. If the same key is used with identical parameters,
        /// the cached result is returned. Keys expire after 24 hours.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Generate shell completions to stdout
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Generate man page to stdout
    Man,
    /// Machine-focused docs for automation agents
    RobotDocs {
        /// Topic to print
        #[arg(value_enum)]
        topic: RobotTopic,
    },
    /// Run a one-off search and print results to stdout
    Search {
        /// The query string
        query: String,
        /// Filter by agent slug (can be specified multiple times)
        #[arg(long)]
        agent: Vec<String>,
        /// Filter by workspace path (can be specified multiple times)
        #[arg(long)]
        workspace: Vec<String>,
        /// Max results
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Offset for pagination (start at Nth result)
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Output as JSON (--robot also works). Equivalent to --robot-format json
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Robot output format: json (pretty), jsonl (streaming), compact (single-line), sessions (paths), toon (token-optimized).
        /// Env: CASS_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
        #[arg(long, value_enum)]
        robot_format: Option<RobotFormat>,
        /// Include extended metadata in robot output (`elapsed_ms`, `wildcard_fallback`, `cache_stats`)
        #[arg(long)]
        robot_meta: bool,
        /// Select specific fields in JSON output (comma-separated). Use 'minimal' for `source_path,line_number,agent`
        /// or 'summary' for `source_path,line_number,agent,title,score`. Example: --fields `source_path,line_number`
        #[arg(long, value_delimiter = ',')]
        fields: Option<Vec<String>>,
        /// Truncate content/snippet fields to max N characters (UTF-8 safe, adds '...' and _truncated indicator)
        #[arg(long)]
        max_content_length: Option<usize>,
        /// Soft token budget for robot output (approx; 4 chars ≈ 1 token). Adjusts truncation.
        #[arg(long)]
        max_tokens: Option<usize>,
        /// Request ID to echo in robot _meta for correlation
        #[arg(long)]
        request_id: Option<String>,
        /// Cursor for pagination (base64-encoded offset/limit payload from previous result)
        #[arg(long)]
        cursor: Option<String>,
        /// Human-readable display format: table (aligned columns), lines (one-liner), markdown
        #[arg(long, value_enum)]
        display: Option<DisplayFormat>,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Filter to last N days
        #[arg(long)]
        days: Option<u32>,
        /// Filter to today only
        #[arg(long)]
        today: bool,
        /// Filter to yesterday only
        #[arg(long)]
        yesterday: bool,
        /// Filter to last 7 days
        #[arg(long)]
        week: bool,
        /// Filter to entries since ISO date (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)
        #[arg(long)]
        since: Option<String>,
        /// Filter to entries until ISO date
        #[arg(long)]
        until: Option<String>,
        /// Server-side aggregation by field(s). Comma-separated: `agent,workspace,date,match_type`
        /// Returns buckets with counts instead of full results. Use with --limit to get both.
        #[arg(long, value_delimiter = ',')]
        aggregate: Option<Vec<String>>,
        /// Include query explanation in output (shows parsed query, index strategy, cost estimate)
        #[arg(long)]
        explain: bool,
        /// Validate and analyze query without executing (returns explanation, estimated cost, warnings)
        #[arg(long)]
        dry_run: bool,
        /// Timeout in milliseconds. Returns partial results and error if exceeded.
        #[arg(long)]
        timeout: Option<u64>,
        /// Highlight matching terms in output (uses **bold** markers in text, <mark> in HTML)
        #[arg(long)]
        highlight: bool,
        /// Filter by source: 'local', 'remote', 'all', or a specific source hostname
        #[arg(long)]
        source: Option<String>,
        /// Filter to sessions from file (one path per line). Use '-' for stdin.
        /// Enables chained searches: `cass search "query1" --robot-format sessions | cass search "query2" --sessions-from -`
        #[arg(long)]
        sessions_from: Option<String>,
        /// Search mode: lexical (default), semantic, or hybrid
        #[arg(long, value_enum)]
        mode: Option<crate::search::query::SearchMode>,

        /// Use approximate nearest neighbor (ANN) search with HNSW for faster semantic/hybrid queries.
        /// Trades slight accuracy loss for O(log n) search complexity instead of O(n).
        /// Only affects semantic and hybrid modes; ignored for lexical search.
        /// Requires an HNSW index built with `cass index --semantic --approximate`.
        #[arg(long, default_value_t = false)]
        approximate: bool,

        // ==========================================================================
        // Model / Reranker / Daemon flags (bd-3bbv)
        // ==========================================================================
        /// Embedding model to use for semantic search.
        /// Available models depend on what's been downloaded.
        /// Use `cass models --list` to see available options.
        #[arg(long)]
        model: Option<String>,

        /// Enable reranking of search results for improved relevance.
        /// Requires a reranker model to be available.
        #[arg(long, default_value_t = false)]
        rerank: bool,

        /// Reranker model to use (requires --rerank).
        /// Use `cass models --list` to see available options.
        #[arg(long)]
        reranker: Option<String>,

        /// Use daemon for warm model inference (faster repeated queries).
        /// If daemon is unavailable, falls back to direct inference.
        #[arg(long, default_value_t = false)]
        daemon: bool,

        /// Disable daemon usage even if available (force direct inference).
        #[arg(long, default_value_t = false)]
        no_daemon: bool,

        // ==========================================================================
        // Two-tier progressive search flags (bd-3dcw)
        // ==========================================================================
        /// Enable two-tier progressive search: fast results immediately, refined via daemon.
        /// Returns initial results from fast embedder (~1ms), then refines with quality
        /// embedder via daemon (~130ms). Best of both worlds for interactive search.
        #[arg(long, default_value_t = false)]
        two_tier: bool,

        /// Fast-only search: use lightweight embedder for instant results, no refinement.
        /// Ideal for real-time search-as-you-type scenarios where latency is critical.
        #[arg(long, default_value_t = false)]
        fast_only: bool,

        /// Quality-only search: wait for full transformer model results.
        /// Higher latency (~130ms) but most accurate semantic matching.
        /// Requires daemon to be available; falls back to fast if unavailable.
        #[arg(long, default_value_t = false)]
        quality_only: bool,
    },
    /// Show statistics about indexed data
    Stats {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Filter by source: 'local', 'remote', 'all', or a specific source hostname
        #[arg(long)]
        source: Option<String>,
        /// Show breakdown by source
        #[arg(long)]
        by_source: bool,
    },
    /// Output diagnostic information for troubleshooting
    Diag {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Include verbose information (file sizes, timestamps)
        #[arg(long, short)]
        verbose: bool,
    },
    /// Quick health check for agents: index freshness, db stats, recommended action
    Status {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON (default for robot consumption)
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Include _meta block (elapsed, freshness, data_dir/db_path)
        #[arg(long, default_value_t = false)]
        robot_meta: bool,
        /// Staleness threshold in seconds (default: 1800 = 30 minutes)
        #[arg(long, default_value_t = 1800)]
        stale_threshold: u64,
    },
    /// Discover available features, versions, and limits for agent introspection
    Capabilities {
        /// Output as JSON (default for robot consumption)
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Quick state/health check (alias of status)
    State {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON (default for robot consumption)
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Include _meta block (elapsed, freshness, data_dir/db_path)
        #[arg(long, default_value_t = false)]
        robot_meta: bool,
        /// Staleness threshold in seconds (default: 1800 = 30 minutes)
        #[arg(long, default_value_t = 1800)]
        stale_threshold: u64,
    },
    /// Show API + contract version info
    ApiVersion {
        /// Output as JSON (default for robot consumption)
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Full API schema introspection - commands, arguments, and response schemas
    Introspect {
        /// Output as JSON (default for robot consumption)
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// View a source file at a specific line (follow up on search results)
    View {
        /// Path to the source file
        path: PathBuf,
        /// Line number to show (1-indexed)
        #[arg(long, short = 'n')]
        line: Option<usize>,
        /// Number of context lines before/after
        #[arg(long, short = 'C', default_value_t = 5)]
        context: usize,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Minimal health check (<50ms). Exit 0=healthy, 1=unhealthy. For agent pre-flight checks.
    Health {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON (`{"healthy": bool, "latency_ms": N}`)
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Include _meta block (elapsed, freshness, data_dir/db_path)
        #[arg(long, default_value_t = false)]
        robot_meta: bool,
        /// Staleness threshold in seconds (default: 300)
        #[arg(long, default_value = "300")]
        stale_threshold: u64,
    },
    /// Diagnose and repair cass installation issues. Safe by default - never deletes user data.
    /// Use --fix to apply automatic repairs (rebuilds derived data only, preserves source sessions).
    Doctor {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Apply safe fixes automatically (rebuilds index/db from source data)
        #[arg(long)]
        fix: bool,
        /// Run all checks verbosely (show passed checks too)
        #[arg(long, short)]
        verbose: bool,
        /// Force index rebuild even if index appears healthy
        #[arg(long, visible_alias = "force")]
        force_rebuild: bool,
    },
    /// Find related sessions for a given source path
    Context {
        /// Path to the source session file
        path: PathBuf,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Maximum results per relation type (default: 5)
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// Export a conversation to markdown or other formats
    Export {
        /// Path to session file
        path: PathBuf,
        /// Output format
        #[arg(long, value_enum, default_value_t = ConvExportFormat::Markdown)]
        format: ConvExportFormat,
        /// Output file (stdout if not specified)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        /// Include tool use details in export
        #[arg(long)]
        include_tools: bool,
    },
    /// Export session as beautiful, self-contained HTML (with optional encryption)
    #[command(name = "export-html")]
    ExportHtml {
        /// Path to session file
        session: PathBuf,

        /// Output directory (default: current directory)
        #[arg(long)]
        output_dir: Option<PathBuf>,

        /// Custom filename (default: auto-generated from session metadata)
        #[arg(long)]
        filename: Option<String>,

        /// Enable password encryption (Web Crypto compatible)
        #[arg(long)]
        encrypt: bool,

        /// Password for encryption (required if --encrypt)
        #[arg(long)]
        password: Option<String>,

        /// Read password from stdin (secure, no echo)
        #[arg(long)]
        password_stdin: bool,

        /// Include tool calls in export (default: true)
        #[arg(long, default_value_t = true)]
        include_tools: bool,

        /// Show message timestamps
        #[arg(long, default_value_t = true)]
        show_timestamps: bool,

        /// Disable CDN references (fully offline, larger file)
        #[arg(long)]
        no_cdns: bool,

        /// Default theme (dark or light)
        #[arg(long, default_value = "dark")]
        theme: String,

        /// Validate without writing file
        #[arg(long)]
        dry_run: bool,

        /// Show export plan without executing
        #[arg(long)]
        explain: bool,

        /// Open file in browser after export
        #[arg(long)]
        open: bool,

        /// JSON output (for automation)
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Show messages around a specific line in a session file
    Expand {
        /// Path to session file
        path: PathBuf,
        /// Line number to show context around
        #[arg(long, short = 'n')]
        line: usize,
        /// Number of messages before/after (default: 3)
        #[arg(long, short = 'C', default_value_t = 3)]
        context: usize,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Show activity timeline for a time range
    Timeline {
        /// Start time (ISO date, 'today', 'yesterday', 'Nd' for N days ago)
        #[arg(long)]
        since: Option<String>,
        /// End time (ISO date or relative)
        #[arg(long)]
        until: Option<String>,
        /// Show today only
        #[arg(long)]
        today: bool,
        /// Filter by agent (can be repeated)
        #[arg(long)]
        agent: Vec<String>,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Group by: hour, day, or none
        #[arg(long, value_enum, default_value_t = TimelineGrouping::Hour)]
        group_by: TimelineGrouping,
        /// Filter by source: 'local', 'remote', 'all', or a specific source hostname
        #[arg(long)]
        source: Option<String>,
    },
    /// Export encrypted searchable archive for static hosting (P4.x)
    Pages {
        /// Export only (skip wizard and encryption) to specified directory
        #[arg(long)]
        export_only: Option<PathBuf>,

        /// Verify an existing export bundle (for CI/CD)
        #[arg(long)]
        verify: Option<PathBuf>,

        /// Filter by agent (comma-separated)
        #[arg(long, value_delimiter = ',')]
        agents: Option<Vec<String>>,

        /// Filter by workspace (comma-separated)
        #[arg(long, value_delimiter = ',')]
        workspaces: Option<Vec<String>>,

        /// Filter entries since ISO date or relative time
        #[arg(long)]
        since: Option<String>,

        /// Filter entries until ISO date or relative time
        #[arg(long)]
        until: Option<String>,

        /// Path mode: relative (default), basename, full, hash
        #[arg(long, value_enum, default_value_t = crate::pages::export::PathMode::Relative)]
        path_mode: crate::pages::export::PathMode,

        /// Deployment target: local, github, cloudflare
        #[arg(long, value_enum)]
        target: Option<PagesDeployTarget>,

        /// Cloudflare project name (also used for GitHub repo name)
        #[arg(long, alias = "repo")]
        project: Option<String>,

        /// Cloudflare production branch (default: main)
        #[arg(long)]
        branch: Option<String>,

        /// Cloudflare account ID (or CLOUDFLARE_ACCOUNT_ID env)
        #[arg(long)]
        account_id: Option<String>,

        /// Cloudflare API token (or CLOUDFLARE_API_TOKEN env)
        #[arg(long)]
        api_token: Option<String>,

        /// Dry run (don't write files)
        #[arg(long)]
        dry_run: bool,

        /// Scan for secrets and exit (no export)
        #[arg(long)]
        scan_secrets: bool,

        /// Fail with non-zero exit if secrets are detected (for CI)
        #[arg(long)]
        fail_on_secrets: bool,

        /// Allowlist regex patterns to suppress findings (repeatable or comma-separated)
        #[arg(long, value_delimiter = ',')]
        secrets_allow: Vec<String>,

        /// Denylist regex patterns to force findings (repeatable or comma-separated)
        #[arg(long, value_delimiter = ',')]
        secrets_deny: Vec<String>,

        /// Output results as JSON (for verify and secret scan)
        #[arg(long, visible_alias = "robot")]
        json: bool,

        /// Verbose output (show detailed check results)
        #[arg(long, short)]
        verbose: bool,

        /// Export without encryption (DANGEROUS - all content publicly readable)
        #[arg(long)]
        no_encryption: bool,

        /// Acknowledge unencrypted export risks (required in robot/JSON mode with --no-encryption)
        #[arg(long)]
        i_understand_unencrypted_risks: bool,

        /// Include message attachments (images, PDFs, code snapshots)
        #[arg(long)]
        include_attachments: bool,

        /// Preview an existing export locally (starts HTTP server)
        #[arg(long)]
        preview: Option<PathBuf>,

        /// Port for preview server (default: 8080)
        #[arg(long, default_value = "8080")]
        port: u16,

        /// Don't auto-open browser when starting preview server
        #[arg(long)]
        no_open: bool,

        /// JSON config file for non-interactive export (use "-" for stdin)
        #[arg(long)]
        config: Option<String>,

        /// Validate config file without running export
        #[arg(long)]
        validate_config: bool,

        /// Show example config file
        #[arg(long)]
        example_config: bool,
    },
    /// Manage remote sources (P5.x)
    #[command(subcommand)]
    Sources(SourcesCommand),
    /// Manage semantic search models
    #[command(subcommand)]
    Models(ModelsCommand),
    /// Import data from external sources
    #[command(subcommand)]
    Import(ImportCommand),
    /// Token usage, cost, tool, and model analytics
    ///
    /// Subcommands: status, tokens, tools, models, cost, rebuild, validate.
    /// All subcommands share time-range, dimensional, and output flags.
    ///
    /// # Examples
    ///
    /// ```bash
    /// cass analytics status --json
    /// cass analytics tokens --days 7 --group-by day --json
    /// cass analytics cost --agent claude --since 2026-01-01 --json
    /// cass analytics rebuild --json
    /// ```
    #[command(subcommand)]
    Analytics(AnalyticsCommand),
}

/// Subcommands for importing external data
#[derive(Subcommand, Debug, Clone)]
pub enum ImportCommand {
    /// Import ChatGPT web export (conversations.json)
    ///
    /// Splits the exported conversations.json into individual files that
    /// the ChatGPT connector can index without encryption keys.
    /// After importing, run `cass index` to index the conversations.
    Chatgpt {
        /// Path to conversations.json from ChatGPT web export
        #[arg(value_hint = ValueHint::FilePath)]
        path: PathBuf,

        /// Output directory (default: ChatGPT app support dir on macOS, or ~/.local/share/cass/chatgpt/ on Linux)
        #[arg(long)]
        output_dir: Option<PathBuf>,

        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
}

/// Subcommands for managing remote sources (P5.x)
#[derive(Subcommand, Debug, Clone)]
pub enum SourcesCommand {
    /// List configured sources
    List {
        /// Show detailed information
        #[arg(long, short)]
        verbose: bool,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Add a new remote source
    Add {
        /// Source URL (e.g., user@host or ssh://user@host)
        url: String,
        /// Friendly name for this source (becomes source_id)
        #[arg(long)]
        name: Option<String>,
        /// Use preset paths for platform (macos-defaults, linux-defaults)
        #[arg(long)]
        preset: Option<String>,
        /// Paths to sync (can be specified multiple times)
        #[arg(long = "path", short = 'p')]
        paths: Vec<String>,
        /// Skip connectivity test
        #[arg(long)]
        no_test: bool,
    },
    /// Remove a configured source
    Remove {
        /// Name of source to remove
        name: String,
        /// Also delete synced session data from index
        #[arg(long)]
        purge: bool,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Diagnose source connectivity and configuration issues
    Doctor {
        /// Check only specific source (defaults to all)
        #[arg(long, short)]
        source: Option<String>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Synchronize sessions from remote sources
    Sync {
        /// Sync only specific source(s)
        #[arg(long, short)]
        source: Option<Vec<String>>,
        /// Don't re-index after sync
        #[arg(long)]
        no_index: bool,
        /// Show detailed transfer information
        #[arg(long, short)]
        verbose: bool,
        /// Dry run - show what would be synced without actually syncing
        #[arg(long)]
        dry_run: bool,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Manage path mappings for a source (P6.3)
    #[command(subcommand)]
    Mappings(MappingsAction),
    /// Auto-discover SSH hosts from ~/.ssh/config
    Discover {
        /// Platform preset for default paths (macos-defaults, linux-defaults)
        #[arg(long, default_value = "linux-defaults")]
        preset: String,
        /// Skip hosts that are already configured as sources
        #[arg(long)]
        skip_existing: bool,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Interactive wizard to discover, configure, and set up remote sources.
    ///
    /// This wizard automates configuring cass to search across multiple machines.
    /// It discovers SSH hosts from ~/.ssh/config, checks each for existing cass
    /// installations and agent session data, then guides you through selecting
    /// which machines to configure for remote search.
    ///
    /// # Workflow Phases
    ///
    /// 1. **Discovery**: Parses ~/.ssh/config to find configured hosts
    /// 2. **Probing**: Connects to each host via SSH to check cass status and data
    /// 3. **Selection**: Interactive selection of which hosts to configure
    /// 4. **Installation**: Installs cass on hosts that don't have it (optional)
    /// 5. **Indexing**: Runs `cass index` on remotes (optional)
    /// 6. **Configuration**: Generates and saves sources.toml entries
    /// 7. **Sync**: Downloads session data to local machine (optional)
    ///
    /// # Examples
    ///
    /// ```bash
    /// # Interactive wizard (recommended for first-time setup)
    /// cass sources setup
    ///
    /// # Configure specific hosts only
    /// cass sources setup --hosts css,csd,yto
    ///
    /// # Preview without making changes
    /// cass sources setup --dry-run
    ///
    /// # Resume an interrupted setup
    /// cass sources setup --resume
    ///
    /// # Non-interactive for scripting (uses auto-detected defaults)
    /// cass sources setup --non-interactive --hosts css,csd
    ///
    /// # Skip installation and indexing, just configure
    /// cass sources setup --hosts css --skip-install --skip-index
    ///
    /// # JSON output for automation
    /// cass sources setup --json --hosts css
    /// ```
    ///
    /// # State and Resume
    ///
    /// If setup is interrupted (Ctrl+C, connection lost), state is saved to
    /// `~/.config/cass/setup_state.json`. Resume with `cass sources setup --resume`.
    ///
    /// # See Also
    ///
    /// - `cass sources list` - List configured sources
    /// - `cass sources sync` - Sync data from sources
    /// - `cass sources discover` - Just discover hosts (no setup)
    /// - `cass robot-docs sources` - Machine-readable sources documentation
    Setup {
        /// Preview what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Skip interactive prompts (use auto-detected defaults for scripting)
        #[arg(long)]
        non_interactive: bool,
        /// Configure only these hosts (comma-separated SSH aliases, skips discovery/selection)
        #[arg(long, value_delimiter = ',')]
        hosts: Option<Vec<String>>,
        /// Skip cass installation on remotes that don't have it
        #[arg(long)]
        skip_install: bool,
        /// Skip running `cass index` on remotes
        #[arg(long)]
        skip_index: bool,
        /// Skip syncing data after setup completes
        #[arg(long)]
        skip_sync: bool,
        /// SSH connection timeout in seconds
        #[arg(long, default_value = "10")]
        timeout: u64,
        /// Resume from previous interrupted setup (reads ~/.config/cass/setup_state.json)
        #[arg(long)]
        resume: bool,
        /// Show detailed progress output
        #[arg(long, short)]
        verbose: bool,
        /// Output progress as JSON (implies non-interactive, for scripting)
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
}

/// Subcommands for managing semantic search models
#[derive(Subcommand, Debug, Clone)]
pub enum ModelsCommand {
    /// Show model installation status
    Status {
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Download and install the semantic search model
    Install {
        /// Model to install (default: all-minilm-l6-v2)
        #[arg(long, default_value = "all-minilm-l6-v2")]
        model: String,
        /// Custom mirror URL for downloading
        #[arg(long)]
        mirror: Option<String>,
        /// Install from local file (for air-gapped environments)
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Verify model integrity (SHA256 checksums)
    Verify {
        /// Attempt to repair corrupted files
        #[arg(long)]
        repair: bool,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Remove model files to free disk space
    Remove {
        /// Model to remove (default: all-minilm-l6-v2)
        #[arg(long, default_value = "all-minilm-l6-v2")]
        model: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Check for model updates
    CheckUpdate {
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

/// Subcommands for managing path mappings (P6.3)
#[derive(Subcommand, Debug, Clone)]
pub enum MappingsAction {
    /// List path mappings for a source
    List {
        /// Source name
        source: String,
        /// Output as JSON
        #[arg(long, visible_alias = "robot")]
        json: bool,
    },
    /// Add a path mapping
    Add {
        /// Source name
        source: String,
        /// Remote path prefix to match
        #[arg(long)]
        from: String,
        /// Local path prefix to replace with
        #[arg(long)]
        to: String,
        /// Only apply to specific agents (comma-separated)
        #[arg(long, value_delimiter = ',')]
        agents: Option<Vec<String>>,
    },
    /// Remove a path mapping by index
    Remove {
        /// Source name
        source: String,
        /// Index of mapping to remove (from list output, 0-based)
        index: usize,
    },
    /// Test how a path would be rewritten
    Test {
        /// Source name
        source: String,
        /// Path to test
        path: String,
        /// Optional agent to simulate (for agent-specific rules)
        #[arg(long)]
        agent: Option<String>,
    },
}

/// Time bucketing for analytics aggregation.
#[derive(Copy, Clone, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum AnalyticsBucketing {
    /// Group by hour
    Hour,
    /// Group by day
    #[default]
    Day,
    /// Group by week (ISO weeks)
    Week,
    /// Group by calendar month
    Month,
}

impl std::fmt::Display for AnalyticsBucketing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hour => write!(f, "hour"),
            Self::Day => write!(f, "day"),
            Self::Week => write!(f, "week"),
            Self::Month => write!(f, "month"),
        }
    }
}

/// Subcommands for analytics (token usage, cost, tool, and model breakdowns).
///
/// All subcommands share time-range, dimensional-filter, and output flags
/// via clap's `flatten` on [`AnalyticsCommon`].
#[derive(Subcommand, Debug, Clone)]
pub enum AnalyticsCommand {
    /// Summary of analytics data health: row counts, freshness, coverage, drift warnings
    Status {
        #[command(flatten)]
        common: AnalyticsCommon,
    },
    /// Token usage over time, with dimensional breakdowns
    Tokens {
        #[command(flatten)]
        common: AnalyticsCommon,
        /// Time bucket for aggregation
        #[arg(long, value_enum, default_value_t = AnalyticsBucketing::Day)]
        group_by: AnalyticsBucketing,
    },
    /// Per-tool invocation counts and derived metrics
    Tools {
        #[command(flatten)]
        common: AnalyticsCommon,
        /// Time bucket for aggregation
        #[arg(long, value_enum, default_value_t = AnalyticsBucketing::Day)]
        group_by: AnalyticsBucketing,
        /// Maximum tools to return (default 20)
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Top models by usage and coverage statistics
    #[command(name = "models")]
    AnalyticsModels {
        #[command(flatten)]
        common: AnalyticsCommon,
        /// Time bucket for aggregation
        #[arg(long, value_enum, default_value_t = AnalyticsBucketing::Day)]
        group_by: AnalyticsBucketing,
    },
    /// USD cost estimates with pricing coverage
    Cost {
        #[command(flatten)]
        common: AnalyticsCommon,
        /// Time bucket for aggregation
        #[arg(long, value_enum, default_value_t = AnalyticsBucketing::Day)]
        group_by: AnalyticsBucketing,
    },
    /// Rebuild / backfill analytics rollup tables with progress output
    Rebuild {
        #[command(flatten)]
        common: AnalyticsCommon,
        /// Force full rebuild even if rollups appear fresh
        #[arg(long)]
        force: bool,
    },
    /// Check rollup invariants and detect drift between raw data and aggregates
    Validate {
        #[command(flatten)]
        common: AnalyticsCommon,
        /// Attempt automatic repair of detected drift
        #[arg(long)]
        fix: bool,
    },
}

/// Shared flags for all analytics subcommands.
///
/// Provides a single, reusable argument model so every analytics
/// command has identical time-range, dimensional-filter, and output
/// behavior.
#[derive(Debug, Clone, clap::Args)]
pub struct AnalyticsCommon {
    // ---- Time range ----
    /// Filter to entries since ISO date (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)
    #[arg(long)]
    pub since: Option<String>,

    /// Filter to entries until ISO date
    #[arg(long)]
    pub until: Option<String>,

    /// Filter to last N days
    #[arg(long)]
    pub days: Option<u32>,

    // ---- Dimensional filters ----
    /// Filter by agent slug (can be specified multiple times)
    #[arg(long)]
    pub agent: Vec<String>,

    /// Filter by workspace path (can be specified multiple times)
    #[arg(long)]
    pub workspace: Vec<String>,

    /// Filter by source: 'local', 'remote', 'all', or a specific hostname
    #[arg(long)]
    pub source: Option<String>,

    // ---- Output ----
    /// Output as JSON (robot-safe)
    #[arg(long, visible_alias = "robot")]
    pub json: bool,

    /// Override data dir
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Bridge impls: CLI types → analytics library types
// ---------------------------------------------------------------------------

impl From<AnalyticsBucketing> for analytics::GroupBy {
    fn from(b: AnalyticsBucketing) -> Self {
        match b {
            AnalyticsBucketing::Hour => analytics::GroupBy::Hour,
            AnalyticsBucketing::Day => analytics::GroupBy::Day,
            AnalyticsBucketing::Week => analytics::GroupBy::Week,
            AnalyticsBucketing::Month => analytics::GroupBy::Month,
        }
    }
}

impl From<&AnalyticsCommon> for analytics::AnalyticsFilter {
    fn from(common: &AnalyticsCommon) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let since_ms: Option<i64> = if let Some(d) = common.days {
            Some(now_ms - (d as i64) * 86_400_000)
        } else {
            common.since.as_deref().and_then(parse_datetime_str)
        };

        let until_ms: Option<i64> = common.until.as_deref().and_then(parse_datetime_str);

        let source = match common.source.as_deref() {
            None | Some("all") => analytics::SourceFilter::All,
            Some("local") => analytics::SourceFilter::Local,
            Some("remote") => analytics::SourceFilter::Remote,
            Some(specific) => analytics::SourceFilter::Specific(specific.into()),
        };

        analytics::AnalyticsFilter {
            since_ms,
            until_ms,
            agents: common.agent.clone(),
            source,
            workspace_ids: vec![], // workspace lookup not yet wired
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ColorPref {
    Auto,
    Never,
    Always,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ProgressMode {
    Auto,
    Bars,
    Plain,
    None,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum RobotTopic {
    Commands,
    Env,
    Paths,
    Schemas,
    Guide,
    ExitCodes,
    Examples,
    Contracts,
    Wrap,
    Sources,
    Analytics,
}

/// Output format for robot/automation mode
#[derive(Copy, Clone, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum RobotFormat {
    /// Pretty-printed JSON object (default, backward compatible)
    #[default]
    Json,
    /// Newline-delimited JSON: one object per line with optional _meta header
    Jsonl,
    /// Compact single-line JSON (no pretty printing)
    Compact,
    /// Session paths only: one source_path per line (for chained searches)
    Sessions,
    /// Token-Optimized Object Notation (encodes via toon_rust crate)
    Toon,
}

/// Human-readable display format for CLI output (non-JSON)
#[derive(Copy, Clone, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum DisplayFormat {
    /// Aligned columns with headers (default human-readable)
    #[default]
    Table,
    /// One-liner per result with key info
    Lines,
    /// Markdown with role headers and code blocks
    Markdown,
}

/// Conversation export format (for export command)
#[derive(Copy, Clone, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum ConvExportFormat {
    /// Markdown with headers and formatting
    #[default]
    Markdown,
    /// Plain text
    Text,
    /// JSON array of messages
    Json,
    /// HTML with styling
    Html,
}

/// Timeline grouping options
#[derive(Copy, Clone, Debug, Default, ValueEnum, PartialEq, Eq)]
pub enum TimelineGrouping {
    /// Group by hour
    #[default]
    Hour,
    /// Group by day
    Day,
    /// No grouping (flat list)
    None,
}

/// Deployment target for pages export.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum PagesDeployTarget {
    /// Local export only
    Local,
    /// GitHub Pages deployment
    #[value(name = "github")]
    GitHub,
    /// Cloudflare Pages deployment
    #[value(name = "cloudflare")]
    Cloudflare,
}

impl PagesDeployTarget {
    fn to_wizard_target(self) -> crate::pages::wizard::DeployTarget {
        match self {
            PagesDeployTarget::GitHub => crate::pages::wizard::DeployTarget::GitHubPages,
            PagesDeployTarget::Cloudflare => crate::pages::wizard::DeployTarget::CloudflarePages,
            PagesDeployTarget::Local => crate::pages::wizard::DeployTarget::Local,
        }
    }

    fn as_config_value(self) -> &'static str {
        match self {
            PagesDeployTarget::GitHub => "github",
            PagesDeployTarget::Cloudflare => "cloudflare",
            PagesDeployTarget::Local => "local",
        }
    }
}

/// Aggregation field types for --aggregate flag
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateField {
    Agent,
    Workspace,
    Date,
    MatchType,
}

impl AggregateField {
    /// Parse field name to enum
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "agent" => Some(Self::Agent),
            "workspace" => Some(Self::Workspace),
            "date" => Some(Self::Date),
            "match_type" | "matchtype" => Some(Self::MatchType),
            _ => None,
        }
    }

    /// Get the field name as a string
    #[allow(dead_code)]
    fn as_str(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Workspace => "workspace",
            Self::Date => "date",
            Self::MatchType => "match_type",
        }
    }
}

/// A single bucket in an aggregation result
#[derive(Debug, Clone, Serialize)]
pub struct AggregationBucket {
    /// The grouped key value
    pub key: String,
    /// Count of items in this bucket
    pub count: u64,
}

/// Aggregation result for a single field
#[derive(Debug, Clone, Serialize)]
pub struct FieldAggregation {
    /// Top buckets (limited to 10 by default)
    pub buckets: Vec<AggregationBucket>,
    /// Total count of items that didn't fit in top buckets
    pub other_count: u64,
}

/// Container for all aggregation results
#[derive(Debug, Clone, Default, Serialize)]
pub struct Aggregations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<FieldAggregation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<FieldAggregation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<FieldAggregation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_type: Option<FieldAggregation>,
}

impl Aggregations {
    fn is_empty(&self) -> bool {
        self.agent.is_none()
            && self.workspace.is_none()
            && self.date.is_none()
            && self.match_type.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct CliError {
    pub code: i32,
    pub kind: &'static str,
    pub message: String,
    pub hint: Option<String>,
    pub retryable: bool,
}

pub type CliResult<T = ()> = std::result::Result<T, CliError>;

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    fn usage(message: impl Into<String>, hint: Option<String>) -> Self {
        CliError {
            code: 2,
            kind: "usage",
            message: message.into(),
            hint,
            retryable: false,
        }
    }

    fn unknown(message: impl Into<String>) -> Self {
        CliError {
            code: 9,
            kind: "unknown",
            message: message.into(),
            hint: None,
            retryable: false,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProgressResolved {
    Bars,
    Plain,
    None,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct WrapConfig {
    width: Option<usize>,
    nowrap: bool,
}

impl WrapConfig {
    fn new(width: Option<usize>, nowrap: bool) -> Self {
        WrapConfig { width, nowrap }
    }

    fn effective_width(&self) -> Option<usize> {
        if self.nowrap { None } else { self.width }
    }
}

/// Normalize common robot-mode invocation mistakes to make the CLI more forgiving for AI agents.
///
/// This function applies multiple layers of normalization to maximize acceptance of
/// commands where intent is clear, even if syntax is imperfect:
///
/// 1. **Single-dash long flags**: `-robot` → `--robot`, `-limit` → `--limit`
/// 2. **Case normalization**: `--Robot`, `--LIMIT` → `--robot`, `--limit`
/// 3. **Subcommand aliases**: `find`/`query`/`q` → `search`, `ls`/`list` → `stats`, etc.
/// 4. **Flag-as-subcommand**: `--robot-docs` → `robot-docs` subcommand
/// 5. **Global flag hoisting**: Moves global flags to front regardless of position
///
/// Returns normalized argv plus an optional correction note teaching proper syntax.
fn normalize_args(raw: Vec<String>) -> (Vec<String>, Option<String>) {
    if raw.is_empty() {
        return (raw, None);
    }
    let prog = &raw[0];
    let mut globals: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut sub_seen = false;
    let mut corrections: Vec<String> = Vec::new();

    // Known long flags (without --) for single-dash and case normalization
    const KNOWN_LONG_FLAGS: &[&str] = &[
        "robot",
        "json",
        "limit",
        "offset",
        "agent",
        "workspace",
        "fields",
        "max-tokens",
        "request-id",
        "cursor",
        "since",
        "until",
        "days",
        "today",
        "week",
        "full",
        "watch",
        "data-dir",
        "verbose",
        "quiet",
        "color",
        "progress",
        "wrap",
        "nowrap",
        "db",
        "trace-file",
        "robot-help",
        "robot-docs",
        "help",
        "version",
        "force",
        "dry-run",
        "no-cache",
        "source",
        "sessions-from",
        "mode",
        "highlight",
        "timeout",
        "explain",
        "aggregate",
        "display",
        // Subcommand-specific flags
        "line",
        "context",
        "output",
        "format",
        "encrypt",
        "password",
        "theme",
        // Added flags
        "watch-once",
        "semantic",
        "embedder",
        "idempotency-key",
        "model",
        "rerank",
        "reranker",
        "daemon",
        "no-daemon",
        "preview",
        "port",
        "config",
        "validate-config",
        "example-config",
        "skip-install",
        "skip-index",
        "skip-sync",
        "resume",
        "non-interactive",
        // Missing flags added
        "approximate",
        "build-hnsw",
        "export-only",
        "verify",
        "scan-secrets",
        "fail-on-secrets",
        "secrets-allow",
        "secrets-deny",
        "no-encryption",
        "i-understand-unencrypted-risks",
        "include-attachments",
        "no-open",
    ];

    // Subcommand aliases for common mistakes
    const SUBCOMMAND_ALIASES: &[(&str, &str)] = &[
        // Search aliases
        ("find", "search"),
        ("query", "search"),
        ("q", "search"),
        ("lookup", "search"),
        ("grep", "search"),
        // Stats aliases
        ("ls", "stats"),
        ("list", "stats"),
        ("info", "stats"),
        ("summary", "stats"),
        // Status aliases
        ("st", "status"),
        ("state", "status"),
        // Index aliases
        ("reindex", "index"),
        ("idx", "index"),
        ("rebuild", "index"),
        // View aliases
        ("show", "view"),
        ("get", "view"),
        ("read", "view"),
        // Diag aliases
        ("diagnose", "diag"),
        ("debug", "diag"),
        ("check", "diag"),
        // Capabilities aliases
        ("caps", "capabilities"),
        ("cap", "capabilities"),
        // Introspect aliases
        ("inspect", "introspect"),
        ("intro", "introspect"),
        // Robot-docs aliases
        ("docs", "robot-docs"),
        ("help-robot", "robot-docs"),
        ("robotdocs", "robot-docs"),
    ];

    // Short flags that should remain as single-dash
    const VALID_SHORT_FLAGS: &[&str] = &["-q", "-v", "-h", "-V"];

    // Global flags that take a value via separate argument (--flag VALUE)
    // Note: --data-dir is NOT a global flag - it's per-subcommand
    let global_with_value = |s: &str| {
        matches!(
            s,
            "--color" | "--progress" | "--wrap" | "--db" | "--trace-file"
        )
    };

    // Global flags that take a value via `=` syntax or are standalone
    // Note: --data-dir is NOT a global flag - it's per-subcommand
    let is_global = |s: &str| {
        s == "--color"
            || s.starts_with("--color=")
            || s == "--progress"
            || s.starts_with("--progress=")
            || s == "--wrap"
            || s.starts_with("--wrap=")
            || s == "--nowrap"
            || s == "--db"
            || s.starts_with("--db=")
            || s == "--quiet"
            || s == "-q"
            || s == "--verbose"
            || s == "-v"
            || s == "--trace-file"
            || s.starts_with("--trace-file=")
            || s == "--robot-help"
    };

    /// Normalize a single argument: single-dash → double-dash, case → lowercase
    fn normalize_single_arg(arg: &str, corrections: &mut Vec<String>) -> String {
        // Skip if already valid short flag
        if VALID_SHORT_FLAGS.contains(&arg) {
            return arg.to_string();
        }

        // Handle single-dash long flags: -robot → --robot, -limit=5 → --limit=5
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            let (flag_part, value_part) = if let Some(idx) = arg.find('=') {
                (&arg[1..idx], Some(&arg[idx..]))
            } else {
                (&arg[1..], None)
            };
            let flag_lower = flag_part.to_lowercase();
            if KNOWN_LONG_FLAGS.contains(&flag_lower.as_str()) {
                let corrected = if let Some(val) = value_part {
                    format!("--{flag_lower}{val}")
                } else {
                    format!("--{flag_lower}")
                };
                corrections.push(format!(
                    "'{arg}' → '{corrected}' (use double-dash for long flags)"
                ));
                return corrected;
            }
        }

        // Handle case normalization for double-dash flags: --Robot → --robot
        if let Some(stripped) = arg.strip_prefix("--") {
            let (flag_part, value_part) = if let Some(idx) = stripped.find('=') {
                (&stripped[..idx], Some(&stripped[idx..]))
            } else {
                (stripped, None)
            };
            let flag_lower = flag_part.to_lowercase();
            if flag_part != flag_lower && KNOWN_LONG_FLAGS.contains(&flag_lower.as_str()) {
                let corrected = if let Some(val) = value_part {
                    format!("--{flag_lower}{val}")
                } else {
                    format!("--{flag_lower}")
                };
                corrections.push(format!("'{arg}' → '{corrected}' (flags are lowercase)"));
                return corrected;
            }
        }

        arg.to_string()
    }

    let args: Vec<_> = raw.iter().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];

        // First, normalize the argument (single-dash, case)
        let normalized_arg = normalize_single_arg(arg, &mut corrections);

        // Handle --robot-docs and --robot-docs=topic (flag used as subcommand)
        if normalized_arg == "--robot-docs" {
            rest.push("robot-docs".into());
            corrections
                .push("'--robot-docs' → 'robot-docs' (it's a subcommand, not a flag)".into());
            i += 1;
            continue;
        }
        if let Some(topic) = normalized_arg.strip_prefix("--robot-docs=") {
            rest.push("robot-docs".into());
            if !topic.is_empty() {
                rest.push(topic.to_string());
            }
            corrections.push(format!(
                "'{}' → 'robot-docs {topic}' (robot-docs is a subcommand)",
                arg
            ));
            i += 1;
            continue;
        }

        // Check for subcommand aliases (only before first subcommand seen)
        if !sub_seen && !normalized_arg.starts_with('-') {
            let lower = normalized_arg.to_lowercase();
            if let Some(&(alias, canonical)) = SUBCOMMAND_ALIASES
                .iter()
                .find(|(a, _)| a.eq_ignore_ascii_case(&lower))
            {
                rest.push(canonical.to_string());
                corrections.push(format!(
                    "'{alias}' → '{canonical}' (canonical subcommand name)"
                ));
                sub_seen = true;
                i += 1;
                continue;
            }
        }

        // Handle global flags
        if is_global(&normalized_arg) {
            globals.push(normalized_arg.clone());
            // Only note if globals appear after subcommand (moved to front)
            if sub_seen && !corrections.iter().any(|c| c.contains("moved to front")) {
                corrections.push("Global flags moved to front of command".into());
            }
            // If this global takes a value and doesn't use `=` syntax, consume the next arg
            if global_with_value(&normalized_arg)
                && !normalized_arg.contains('=')
                && i + 1 < args.len()
                && !args[i + 1].starts_with('-')
            {
                globals.push(args[i + 1].to_string());
                i += 1;
            }
            i += 1;
            continue;
        }

        if !sub_seen && !normalized_arg.starts_with('-') {
            sub_seen = true;
        }
        rest.push(normalized_arg);
        i += 1;
    }

    let mut normalized = Vec::with_capacity(1 + globals.len() + rest.len());
    normalized.push(prog.clone());
    normalized.extend(globals);
    normalized.extend(rest);

    let note = if corrections.is_empty() {
        None
    } else {
        Some(format!(
            "Auto-corrected: {}. Canonical form: {}",
            corrections.join("; "),
            if normalized.len() > 1 {
                normalized[1..].join(" ")
            } else {
                String::new()
            }
        ))
    };
    (normalized, note)
}

/// Build a friendly parse error with actionable, context-aware examples for AI agents.
///
/// This function analyzes what the agent was likely trying to do and provides
/// targeted examples that match their apparent intent.
fn format_friendly_parse_error(err: clap::Error, raw: &[String], normalized: &[String]) -> String {
    let is_robot = raw
        .iter()
        .any(|s| s == "--json" || s == "--robot" || s == "-robot" || s == "-json");

    // Detect what the agent was probably trying to do
    let raw_str = raw.join(" ").to_lowercase();
    let intent = detect_command_intent(&raw_str);

    if is_robot {
        let mut err_map = serde_json::Map::new();
        err_map.insert("status".into(), "error".into());
        err_map.insert("error".into(), err.to_string().into());
        err_map.insert("kind".into(), "argument_parsing".into());

        if raw != normalized && normalized.len() > 1 {
            err_map.insert(
                "normalized_attempt".into(),
                normalized[1..].join(" ").into(),
            );
        }

        // Context-aware examples based on detected intent
        let examples = get_contextual_examples(&intent);
        err_map.insert("examples".into(), serde_json::json!(examples));

        // Context-aware hints
        let hints = get_contextual_hints(&intent, &raw_str);
        err_map.insert("hints".into(), serde_json::json!(hints));

        // Common mistakes for this intent
        if let Some(common_mistakes) = get_common_mistakes(&intent) {
            err_map.insert("common_mistakes".into(), serde_json::json!(common_mistakes));
        }

        // Quick reference for flags
        err_map.insert(
            "flag_syntax".into(),
            serde_json::json!({
                "correct": ["--limit 5", "--robot", "--json"],
                "incorrect": ["-limit 5", "limit=5", "--Limit"]
            }),
        );

        return serde_json::to_string_pretty(&err_map).unwrap_or_else(|_| err.to_string());
    }

    // Human-readable format
    let mut parts = Vec::new();
    parts.push("Argument parsing failed; command intent unclear.".to_string());
    parts.push(format!("Error: {err}"));
    if raw != normalized && normalized.len() > 1 {
        parts.push(format!(
            "Attempted normalization: {}",
            normalized[1..].join(" ")
        ));
    }
    parts.push(String::new());
    parts.push(format!(
        "Based on your command, you may be trying to: {intent}"
    ));
    parts.push(String::new());
    parts.push("Correct examples:".to_string());
    for ex in get_contextual_examples(&intent) {
        parts.push(format!("  {ex}"));
    }
    parts.push(String::new());
    parts.push("Quick syntax reference:".to_string());
    parts.push("  - Long flags use double-dash: --robot, --limit 5".to_string());
    parts.push("  - Flag values use space or equals: --limit 5 or --limit=5".to_string());
    parts.push("  - Subcommands come first: cass search \"query\"".to_string());
    parts.join("\n")
}

/// Detect the likely command intent from the raw argument string.
fn detect_command_intent(raw_str: &str) -> String {
    if raw_str.contains("search")
        || raw_str.contains("find")
        || raw_str.contains("query")
        || raw_str.contains("grep")
    {
        "search for sessions or messages".to_string()
    } else if raw_str.contains("doc") || raw_str.contains("help") || raw_str.contains("robot") {
        "get robot-mode documentation".to_string()
    } else if raw_str.contains("stats") || raw_str.contains("ls") || raw_str.contains("list") {
        "view statistics or list sessions".to_string()
    } else if raw_str.contains("index")
        || raw_str.contains("rebuild")
        || raw_str.contains("reindex")
    {
        "rebuild or manage the search index".to_string()
    } else if raw_str.contains("view") || raw_str.contains("show") || raw_str.contains("get") {
        "view a specific session".to_string()
    } else if raw_str.contains("cap") || raw_str.contains("introspect") {
        "discover tool capabilities".to_string()
    } else if raw_str.contains("diag") || raw_str.contains("debug") || raw_str.contains("check") {
        "run diagnostics".to_string()
    } else if raw_str.contains("status") {
        "check status".to_string()
    } else if raw_str.contains("health") {
        "run health check".to_string()
    } else {
        "run a cass command".to_string()
    }
}

/// Get context-aware examples based on detected intent.
fn get_contextual_examples(intent: &str) -> Vec<&'static str> {
    if intent.contains("search") {
        vec![
            "cass search \"error handling\" --robot --limit 10",
            "cass search \"authentication\" --robot --agent claude",
            "cass search \"database\" --robot --since 2024-01-01",
            "cass search \"TODO\" --robot --workspace /path/to/project",
        ]
    } else if intent.contains("documentation") {
        vec![
            "cass robot-docs commands",
            "cass robot-docs schemas",
            "cass robot-docs examples",
            "cass --robot-help",
        ]
    } else if intent.contains("statistics") || intent.contains("list") {
        vec![
            "cass stats --robot",
            "cass stats --robot --agent claude",
            "cass stats --robot --workspace /path",
            "cass stats --robot --since 2024-01-01",
        ]
    } else if intent.contains("index") {
        vec![
            "cass index --robot",
            "cass index --robot --force",
            "cass index --robot --data-dir /custom/path",
        ]
    } else if intent.contains("view") {
        vec![
            "cass view <session-id> --robot",
            "cass view <session-id> --robot --full",
            "cass view <session-id> --robot --fields content,timestamp",
        ]
    } else if intent.contains("capabilities") {
        vec!["cass capabilities --json", "cass introspect --json"]
    } else if intent.contains("diagnostics") {
        vec!["cass diag --robot", "cass diag --robot --verbose"]
    } else if intent.contains("status") {
        vec!["cass status --robot", "cass status --robot --watch"]
    } else if intent.contains("health") {
        vec!["cass health --json"]
    } else {
        vec![
            "cass --robot-help                    # Get robot-mode documentation",
            "cass search \"query\" --robot         # Search sessions",
            "cass capabilities --json             # Discover capabilities",
            "cass stats --robot                   # View statistics",
        ]
    }
}

/// Get context-aware hints based on detected intent and raw command.
fn get_contextual_hints(intent: &str, raw_str: &str) -> Vec<String> {
    let mut hints = Vec::new();

    // Check for common syntax mistakes
    if raw_str.contains("-robot") && !raw_str.contains("--robot") {
        hints.push("Use '--robot' (double-dash), not '-robot'".to_string());
    }
    if raw_str.contains("-json") && !raw_str.contains("--json") {
        hints.push("Use '--json' (double-dash), not '-json'".to_string());
    }
    // Only flag bare `limit=` without leading dash as problematic
    if (raw_str.contains(" limit=") || raw_str.starts_with("limit="))
        && !raw_str.contains("--limit=")
        && !raw_str.contains("-limit=")
    {
        hints.push("Use '--limit 5' or '--limit=5', not 'limit=5'".to_string());
    }
    if raw_str.contains("--robot-docs") {
        hints.push(
            "'robot-docs' is a subcommand: use 'cass robot-docs' not 'cass --robot-docs'"
                .to_string(),
        );
    }

    // Intent-specific hints
    if intent.contains("search") && !raw_str.contains("search") {
        hints.push(
            "Use the 'search' subcommand explicitly: cass search \"your query\" --robot"
                .to_string(),
        );
    }

    if hints.is_empty() {
        hints.push(format!("For {intent}, try: cass --robot-help"));
    }

    hints
}

/// Get common mistakes for a given intent.
///
/// Note: Only include mistakes that would actually fail after normalization.
/// Commands that get auto-corrected and succeed (like `cass ls --robot` → `cass stats --robot`)
/// should NOT be listed here since the user would never see this error message.
fn get_common_mistakes(intent: &str) -> Option<serde_json::Value> {
    let mistakes = if intent.contains("search") {
        vec![
            // query="foo" without subcommand - normalization adds "search" but the syntax is wrong
            ("cass query=\"foo\" --robot", "cass search \"foo\" --robot"),
            // Bare limit= without dashes
            (
                "cass search \"query\" limit=5",
                "cass search \"query\" --limit 5",
            ),
            // Missing query entirely
            (
                "cass search --robot --limit 5",
                "cass search \"your query\" --robot --limit 5",
            ),
        ]
    } else if intent.contains("documentation") {
        vec![
            // Flag syntax for subcommand (--robot-docs gets normalized but shown for education)
            ("cass --robot-docs", "cass robot-docs"),
            ("cass --robot-docs=commands", "cass robot-docs commands"),
            // Adding --robot to robot-docs (which doesn't accept it)
            ("cass robot-docs --robot", "cass robot-docs"),
        ]
    } else if intent.contains("statistics") {
        // Note: `cass ls --robot` actually works (normalizes to `cass stats --robot`)
        // so we show mistakes that would actually fail
        vec![
            // Missing required output flag for piping
            ("cass stats | jq .", "cass stats --json | jq ."),
        ]
    } else {
        return None;
    };

    Some(serde_json::json!(
        mistakes
            .iter()
            .map(|(wrong, right)| { serde_json::json!({"wrong": wrong, "correct": right}) })
            .collect::<Vec<_>>()
    ))
}

/// Heuristic recovery for command-line errors to help agents.
/// Returns `(corrected_args, correction_note)` if a likely intent is found.
fn heuristic_parse_recovery(
    err: &clap::Error,
    raw_args: &[String],
) -> Option<(Vec<String>, String)> {
    // Only attempt recovery for "unknown argument" or "unrecognized subcommand" errors
    let is_unknown = err.kind() == clap::error::ErrorKind::UnknownArgument
        || err.kind() == clap::error::ErrorKind::InvalidSubcommand;

    if !is_unknown || raw_args.len() < 2 {
        return None;
    }

    let prog = &raw_args[0];
    let args = &raw_args[1..];
    let mut corrected = Vec::new();
    corrected.push(prog.clone());

    let mut made_correction = false;
    let mut notes = Vec::new();

    // 1. Detect implicit "search" subcommand
    // If the first arg isn't a known subcommand or flag, and looks like a query, assume "search".
    let known_cmds = [
        "search",
        "index",
        "stats",
        "status",
        "diag",
        "view",
        "capabilities",
        "introspect",
        "robot-docs",
        "tui",
        "help",
        "--help",
        "-h",
        "--version",
        "-V",
    ];
    if !args.is_empty() && !args[0].starts_with('-') && !known_cmds.contains(&args[0].as_str()) {
        corrected.push("search".to_string());
        // If the arg looks like `query="foo"`, strip the key
        if args[0].starts_with("query=") || args[0].starts_with("q=") {
            let val = args[0].split_once('=').map(|(_, v)| v).unwrap_or(&args[0]);
            corrected.push(val.to_string());
            notes.push(format!(
                "Assumed 'search' subcommand and stripped query key from '{}'",
                args[0]
            ));
        } else {
            corrected.push(args[0].clone());
            notes.push(format!(
                "Assumed 'search' subcommand for positional argument '{}'",
                args[0]
            ));
        }
        made_correction = true;
        corrected.extend_from_slice(&args[1..]);
    } else {
        // Just copy original structure to start
        corrected.extend_from_slice(args);
    }

    // 2. Fuzzy match flags and fix key=value syntax
    let mut final_args = Vec::new();
    final_args.push(corrected[0].clone()); // prog

    for arg in corrected.iter().skip(1) {
        if arg.starts_with("--") {
            // Split --flag=value or --flag
            let (flag, value) = if let Some((f, v)) = arg.split_once('=') {
                (f, Some(v))
            } else {
                (arg.as_str(), None)
            };

            // Known flags for fuzzy matching
            let known_flags = [
                "--robot",
                "--json",
                "--limit",
                "--offset",
                "--agent",
                "--workspace",
                "--fields",
                "--max-tokens",
                "--request-id",
                "--cursor",
                "--since",
                "--until",
                "--days",
                "--today",
                "--week",
                "--full",
                "--watch",
                "--data-dir",
                "--verbose",
                "--quiet",
            ];

            // Check for exact match
            if known_flags.contains(&flag) {
                final_args.push(arg.clone());
                continue;
            }

            // Check for typos (levenshtein distance <= 2)
            let best_match = known_flags
                .iter()
                .min_by_key(|k| strsim::levenshtein(flag, k))
                .filter(|k| strsim::levenshtein(flag, k) <= 2);

            if let Some(&correction) = best_match {
                if let Some(v) = value {
                    final_args.push(format!("{correction}={v}"));
                } else {
                    final_args.push(correction.to_string());
                }
                notes.push(format!("Corrected typo '{flag}' to '{correction}'"));
                made_correction = true;
            } else {
                // Keep as is if no good guess
                final_args.push(arg.clone());
            }
        } else if arg.contains('=') && !arg.starts_with('-') {
            // 3. Handle `limit=5` (missing --)
            let (key, val) = arg.split_once('=').unwrap();
            let flag_candidate = format!("--{key}");
            // Quick check if adding -- makes it a valid flag
            let known_flags = [
                "--limit",
                "--offset",
                "--agent",
                "--workspace",
                "--days",
                "--since",
                "--until",
                "--source",
                "--output",
                "--format",
                "--model",
                "--reranker",
                "--embedder",
                "--timeout",
                "--fields",
                "--max-tokens",
            ];
            if known_flags.contains(&flag_candidate.as_str()) {
                final_args.push(flag_candidate);
                final_args.push(val.to_string());
                notes.push(format!(
                    "Interpreted '{arg}' as flag '{key}' with value '{val}'"
                ));
                made_correction = true;
            } else {
                final_args.push(arg.clone());
            }
        } else {
            final_args.push(arg.clone());
        }
    }

    if made_correction {
        Some((final_args, notes.join("; ")))
    } else {
        None
    }
}

pub struct ParsedCli {
    pub cli: Cli,
    raw_args: Vec<String>,
    parse_note: Option<String>,
    heuristic_note: Option<String>,
}

pub fn parse_cli(raw_args: Vec<String>) -> CliResult<ParsedCli> {
    // First normalization pass (global flags lift)
    let (normalized_args, parse_note) = normalize_args(raw_args.clone());

    let (cli, heuristic_note) = match Cli::try_parse_from(&normalized_args) {
        Ok(cli) => (cli, None),
        Err(err) => {
            // Let clap handle help/version natively (exit 0, print to stdout)
            use clap::error::ErrorKind;
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.exit();
            }
            // Attempt heuristic recovery
            if let Some((recovered_args, note)) = heuristic_parse_recovery(&err, &normalized_args) {
                // Try parsing again with recovered args
                match Cli::try_parse_from(&recovered_args) {
                    Ok(cli) => (cli, Some(note)),
                    Err(retry_err) => {
                        // Check again for help/version in case recovered args triggered it
                        if matches!(
                            retry_err.kind(),
                            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                        ) {
                            retry_err.exit();
                        }
                        // Recovery failed to produce valid args, fail with original error + friendly help
                        let friendly =
                            format_friendly_parse_error(err, &raw_args, &normalized_args);
                        if friendly.trim().starts_with('{') {
                            return Err(CliError {
                                code: 2,
                                kind: "usage",
                                message: friendly,
                                hint: None,
                                retryable: false,
                            });
                        }
                        return Err(CliError::usage("Could not parse arguments", Some(friendly)));
                    }
                }
            } else {
                // No recovery possible
                let friendly = format_friendly_parse_error(err, &raw_args, &normalized_args);
                if friendly.trim().starts_with('{') {
                    return Err(CliError {
                        code: 2,
                        kind: "usage",
                        message: friendly,
                        hint: None,
                        retryable: false,
                    });
                }
                return Err(CliError::usage("Could not parse arguments", Some(friendly)));
            }
        }
    };

    Ok(ParsedCli {
        cli,
        raw_args,
        parse_note,
        heuristic_note,
    })
}

pub async fn run() -> CliResult<()> {
    let parsed = parse_cli(std::env::args().collect())?;
    run_with_parsed(parsed).await
}

pub async fn run_with_parsed(parsed: ParsedCli) -> CliResult<()> {
    let ParsedCli {
        cli,
        raw_args,
        parse_note,
        heuristic_note,
    } = parsed;

    let stdout_is_tty = io::stdout().is_terminal();
    let stderr_is_tty = io::stderr().is_terminal();
    configure_color(cli.color, stdout_is_tty, stderr_is_tty);

    let wrap_cfg = WrapConfig::new(cli.wrap, cli.nowrap);
    let progress_resolved = resolve_progress(cli.progress, stdout_is_tty);

    let start_ts = Utc::now();
    let start_instant = Instant::now();
    let command_label = describe_command(&cli);

    // Output correction notices for AI agents
    // These teach the agent proper syntax while still honoring their intent
    // Detect robot mode from raw args (more reliable than pattern matching complex enums)
    let is_robot_mode = raw_args
        .iter()
        .any(|s| s == "--json" || s == "--robot" || s == "-json" || s == "-robot")
        || matches!(&cli.command, Some(Commands::Capabilities { .. }))
        || matches!(&cli.command, Some(Commands::Introspect { .. }));
    let is_doc_mode = cli.robot_help || matches!(&cli.command, Some(Commands::RobotDocs { .. }));

    // Combine all correction notes
    let all_notes: Vec<&str> = [parse_note.as_deref(), heuristic_note.as_deref()]
        .into_iter()
        .flatten()
        .collect();

    // Suppress correction chatter for robot/doc modes; still show for humans
    if !all_notes.is_empty() && !is_doc_mode && !is_robot_mode {
        // Human-readable correction notice
        eprintln!("Note: Your command was auto-corrected:");
        for note in &all_notes {
            eprintln!("  • {note}");
        }
        eprintln!("Tip: Run 'cass --help' for proper syntax.");
    }

    let result = execute_cli(
        &cli,
        wrap_cfg,
        progress_resolved,
        stdout_is_tty,
        stderr_is_tty,
    )
    .await;

    if let Some(path) = &cli.trace_file {
        let duration_ms = start_instant.elapsed().as_millis();
        let exit_code = result.as_ref().map_or_else(|e| e.code, |()| 0);
        if let Err(trace_err) = write_trace_line(
            path,
            &command_label,
            &cli,
            &start_ts,
            duration_ms,
            exit_code,
            result.as_ref().err(),
        ) {
            eprintln!("trace-write error: {trace_err}");
        }
    }

    result
}

async fn execute_cli(
    cli: &Cli,
    wrap: WrapConfig,
    progress: ProgressResolved,
    stdout_is_tty: bool,
    stderr_is_tty: bool,
) -> CliResult<()> {
    let command = cli.command.clone().unwrap_or(Commands::Tui {
        once: false,
        reset_state: false,
        asciicast: None,
        data_dir: None,
        inline: false,
        ui_height: 12,
        anchor: "bottom".to_string(),
        record_macro: None,
        play_macro: None,
    });

    if cli.robot_help {
        print_robot_help(wrap)?;
        return Ok(());
    }

    if let Commands::RobotDocs { topic } = command.clone() {
        print_robot_docs(topic, wrap)?;
        return Ok(());
    }

    // TUI preflight: call out env profiles that commonly make the UI look
    // degraded (global no-color, conservative TERM profiles).
    if matches!(command, Commands::Tui { .. }) {
        warn_tui_terminal_profile(stderr_is_tty);
    }

    // Block TUI in non-TTY contexts unless TUI_HEADLESS is set (for testing)
    if matches!(command, Commands::Tui { .. })
        && !stdout_is_tty
        && dotenvy::var("TUI_HEADLESS").is_err()
    {
        return Err(CliError::usage(
            "No subcommand provided; in non-TTY contexts TUI is disabled.",
            Some("Use an explicit subcommand, e.g., `cass search --json ...` or `cass --robot-help`.".to_string()),
        ));
    }

    // Auto-quiet in robot mode: suppress INFO logs for clean JSON output
    // This ensures AI agents get parseable stdout without log noise on stderr
    let robot_mode = is_robot_mode(&command);
    let filter = if cli.quiet || robot_mode {
        // Robot mode implies quiet unless verbose is explicitly requested
        if cli.verbose {
            EnvFilter::new("debug")
        } else {
            EnvFilter::new("warn")
        }
    } else if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    match &command {
        Commands::Tui { data_dir, .. } => {
            let log_dir = data_dir.clone().unwrap_or_else(default_data_dir);
            std::fs::create_dir_all(&log_dir).ok();

            let file_appender = tracing_appender::rolling::daily(&log_dir, "cass.log");
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(non_blocking)
                        .compact()
                        .with_target(false)
                        .with_ansi(false),
                )
                .init();

            maybe_prompt_for_update(matches!(command, Commands::Tui { once: true, .. }))
                .await
                .map_err(|e| CliError {
                    code: 9,
                    kind: "update-check",
                    message: format!("update check failed: {e}"),
                    hint: None,
                    retryable: false,
                })?;
            if let Commands::Tui {
                once,
                reset_state,
                asciicast,
                data_dir,
                inline,
                ui_height,
                anchor,
                record_macro,
                play_macro,
            } = command.clone()
            {
                info!(once, inline, ui_height, %anchor, record_macro = ?record_macro, play_macro = ?play_macro, "launching ftui runtime");

                let inline_config = if inline {
                    let ui_anchor = if anchor == "top" {
                        ui::ftui_adapter::UiAnchor::Top
                    } else {
                        ui::ftui_adapter::UiAnchor::Bottom
                    };
                    Some(ui::app::InlineTuiConfig {
                        ui_height,
                        anchor: ui_anchor,
                    })
                } else {
                    None
                };

                let macro_config = ui::app::MacroConfig {
                    record_path: record_macro,
                    play_path: play_macro,
                };
                let tui_data_dir = data_dir.clone().unwrap_or_else(default_data_dir);
                if reset_state {
                    let state_path = tui_data_dir.join("tui_state.json");
                    match std::fs::remove_file(&state_path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(CliError {
                                code: 9,
                                kind: "tui-reset-state",
                                message: format!(
                                    "failed to remove persisted state {}: {e}",
                                    state_path.display()
                                ),
                                hint: Some(
                                    "Check file permissions or rerun without --reset-state."
                                        .to_string(),
                                ),
                                retryable: false,
                            });
                        }
                    }
                }

                let run_result = if let Some(path) = asciicast {
                    tui_asciicast::run_tui_with_asciicast(&path, !once)
                } else {
                    ui::app::run_tui_ftui(inline_config, macro_config, Some(tui_data_dir))
                };

                run_result.map_err(|e| CliError {
                    code: 9,
                    kind: "tui",
                    message: format!("tui failed: {e}"),
                    hint: None,
                    retryable: false,
                })?;
            }
        }
        Commands::Index { .. }
        | Commands::Search { .. }
        | Commands::Stats { .. }
        | Commands::Diag { .. }
        | Commands::Status { .. }
        | Commands::View { .. }
        | Commands::Pages { .. }
        | Commands::Import(..)
        | Commands::Analytics(..) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .compact()
                .with_target(false)
                .with_ansi(
                    matches!(cli.color, ColorPref::Always)
                        || (matches!(cli.color, ColorPref::Auto) && stderr_is_tty),
                )
                .init();

            match command {
                Commands::Index {
                    full,
                    force_rebuild,
                    watch,
                    watch_once,
                    data_dir,
                    semantic,
                    build_hnsw,
                    embedder,
                    json,
                    idempotency_key,
                } => {
                    run_index_with_data(
                        cli.db.clone(),
                        full,
                        force_rebuild,
                        watch,
                        watch_once,
                        data_dir,
                        semantic,
                        build_hnsw,
                        embedder,
                        progress,
                        json,
                        idempotency_key,
                    )?;
                }
                Commands::Search {
                    query,
                    agent,
                    workspace,
                    limit,
                    offset,
                    json,
                    robot_format,
                    robot_meta,
                    fields,
                    max_content_length,
                    max_tokens,
                    request_id,
                    cursor,
                    display,
                    data_dir,
                    days,
                    today,
                    yesterday,
                    week,
                    since,
                    until,
                    aggregate,
                    explain,
                    dry_run,
                    timeout,
                    highlight,
                    source,
                    sessions_from,
                    mode,
                    approximate,
                    model,
                    rerank,
                    reranker,
                    daemon,
                    no_daemon,
                    two_tier,
                    fast_only,
                    quality_only,
                } => {
                    // Validate mutually exclusive two-tier flags
                    let tier_count = [two_tier, fast_only, quality_only]
                        .iter()
                        .filter(|&&b| b)
                        .count();
                    if tier_count > 1 {
                        return Err(CliError::usage(
                            "Cannot specify multiple tier flags",
                            Some(
                                "Use only one of --two-tier, --fast-only, or --quality-only"
                                    .to_string(),
                            ),
                        ));
                    }

                    // Validate mutually exclusive flags
                    if daemon && no_daemon {
                        return Err(CliError::usage(
                            "Cannot specify both --daemon and --no-daemon",
                            Some(
                                "Use --daemon to enable daemon or --no-daemon to disable it"
                                    .to_string(),
                            ),
                        ));
                    }

                    // Warn about reranker without rerank flag
                    if reranker.is_some() && !rerank {
                        eprintln!(
                            "Warning: --reranker specified but --rerank not enabled; reranker will be ignored"
                        );
                    }

                    // Build semantic options from new flags
                    let semantic_opts = SemanticSearchOptions {
                        model: model.clone(),
                        rerank,
                        reranker: reranker.clone(),
                        use_daemon: daemon && !no_daemon,
                        approximate,
                    };

                    run_cli_search(
                        &query,
                        &agent,
                        &workspace,
                        &limit,
                        &offset,
                        &json,
                        robot_format,
                        robot_meta,
                        fields,
                        max_content_length,
                        max_tokens,
                        request_id.clone(),
                        cursor.clone(),
                        display,
                        &data_dir,
                        cli.db.clone(),
                        wrap,
                        progress,
                        robot_mode,
                        TimeFilter::new(
                            days,
                            today,
                            yesterday,
                            week,
                            since.as_deref(),
                            until.as_deref(),
                        ),
                        aggregate,
                        explain,
                        dry_run,
                        timeout,
                        highlight,
                        source,
                        sessions_from,
                        mode,
                        semantic_opts,
                    )?;
                }
                Commands::Stats {
                    data_dir,
                    json,
                    source,
                    by_source,
                } => {
                    run_stats(
                        &data_dir,
                        cli.db.clone(),
                        json,
                        source.as_deref(),
                        by_source,
                    )?;
                }
                Commands::Diag {
                    data_dir,
                    json,
                    verbose,
                } => {
                    run_diag(&data_dir, cli.db.clone(), json, verbose)?;
                }
                Commands::Status {
                    data_dir,
                    json,
                    robot_meta,
                    stale_threshold,
                } => {
                    run_status(&data_dir, cli.db.clone(), json, stale_threshold, robot_meta)?;
                }
                Commands::View {
                    path,
                    line,
                    context,
                    json,
                } => {
                    run_view(&path, line, context, json || robot_mode)?;
                }
                Commands::Pages {
                    export_only,
                    verify,
                    agents,
                    workspaces,
                    since,
                    until,
                    path_mode,
                    target,
                    project,
                    branch,
                    account_id,
                    api_token,
                    dry_run,
                    scan_secrets,
                    fail_on_secrets,
                    secrets_allow,
                    secrets_deny,
                    json,
                    verbose,
                    no_encryption,
                    i_understand_unencrypted_risks,
                    include_attachments,
                    preview,
                    port,
                    no_open,
                    config,
                    validate_config,
                    example_config,
                } => {
                    // Handle --example-config (show example config and exit)
                    if example_config {
                        println!("{}", crate::pages::config_input::example_config());
                        return Ok(());
                    }

                    // Handle --config based export
                    if let Some(ref config_path) = config {
                        let mut pages_config = crate::pages::config_input::PagesConfig::load(
                            config_path,
                        )
                        .map_err(|e| CliError {
                            code: 2,
                            kind: "pages",
                            message: format!("Failed to load config: {e}"),
                            hint: Some(
                                "Check config file syntax with --example-config".to_string(),
                            ),
                            retryable: false,
                        })?;

                        if let Some(target) = target {
                            pages_config.deployment.target = target.as_config_value().to_string();
                        }
                        if let Some(project) = project.as_ref() {
                            pages_config.deployment.repo = Some(project.to_string());
                        }
                        if let Some(branch) = branch.as_ref() {
                            pages_config.deployment.branch = Some(branch.to_string());
                        }
                        if let Some(account_id) = account_id.as_ref() {
                            pages_config.deployment.account_id = Some(account_id.to_string());
                        }
                        if let Some(api_token) = api_token.as_ref() {
                            pages_config.deployment.api_token = Some(api_token.to_string());
                        }

                        let cli_cf_creds_provided = account_id.is_some() || api_token.is_some();
                        if target.is_none() && cli_cf_creds_provided {
                            pages_config.deployment.target = "cloudflare".to_string();
                        }

                        let target_name = pages_config.deployment.target.to_lowercase();
                        if target.is_some() && cli_cf_creds_provided && target_name != "cloudflare"
                        {
                            return Err(CliError {
                                code: 2,
                                kind: "pages",
                                message: format!(
                                    "Cloudflare credentials provided via CLI but deployment.target is '{target_name}'"
                                ),
                                hint: Some(
                                    "Set deployment.target to \"cloudflare\" or remove Cloudflare credentials."
                                        .to_string(),
                                ),
                                retryable: false,
                            });
                        }

                        // Resolve environment variables
                        pages_config.resolve_env_vars().map_err(|e| CliError {
                            code: 2,
                            kind: "pages",
                            message: format!("Failed to resolve env vars: {e}"),
                            hint: Some(
                                "Ensure referenced environment variables are set".to_string(),
                            ),
                            retryable: false,
                        })?;

                        // Validate configuration
                        let validation = pages_config.validate();

                        if validate_config {
                            // Just validate and output result
                            println!("{}", serde_json::to_string_pretty(&validation).unwrap());
                            if !validation.valid {
                                return Err(CliError {
                                    code: 2,
                                    kind: "pages",
                                    message: "Configuration validation failed".to_string(),
                                    hint: Some("Review errors in JSON output".to_string()),
                                    retryable: false,
                                });
                            }
                            return Ok(());
                        }

                        if !validation.valid {
                            if json || robot_mode {
                                println!("{}", serde_json::to_string_pretty(&validation).unwrap());
                            } else {
                                eprintln!("Configuration errors:");
                                for err in &validation.errors {
                                    eprintln!("  - {}", err);
                                }
                            }
                            return Err(CliError {
                                code: 2,
                                kind: "pages",
                                message: "Configuration validation failed".to_string(),
                                hint: Some("Fix errors listed above".to_string()),
                                retryable: false,
                            });
                        }

                        // Print warnings
                        if !validation.warnings.is_empty() && !json && !robot_mode {
                            eprintln!("Warnings:");
                            for warn in &validation.warnings {
                                eprintln!("  - {}", warn);
                            }
                            eprintln!();
                        }

                        // Get database path
                        let db_path = cli.db.clone().unwrap_or_else(|| {
                            directories::ProjectDirs::from(
                                "com",
                                "dicklesworthstone",
                                "coding-agent-search",
                            )
                            .map(|dirs| dirs.data_dir().join("agent_search.db"))
                            .unwrap_or_else(default_db_path)
                        });

                        // Convert config to WizardState and run export
                        let wizard_state =
                            pages_config.to_wizard_state(db_path.clone()).map_err(|e| {
                                CliError {
                                    code: 9,
                                    kind: "pages",
                                    message: format!("Failed to create wizard state: {e}"),
                                    hint: None,
                                    retryable: false,
                                }
                            })?;

                        // Run the export using the config
                        run_config_based_export(
                            &pages_config,
                            &wizard_state,
                            &db_path,
                            dry_run,
                            json || robot_mode,
                            verbose,
                        )
                        .map_err(|e| CliError {
                            code: 9,
                            kind: "pages",
                            message: format!("Export failed: {e}"),
                            hint: None,
                            retryable: false,
                        })?;

                        return Ok(());
                    }

                    // Handle --validate-config without --config
                    if validate_config {
                        return Err(CliError {
                            code: 2,
                            kind: "pages",
                            message: "--validate-config requires --config".to_string(),
                            hint: Some("Use --config <path> --validate-config".to_string()),
                            retryable: false,
                        });
                    }

                    // Handle --preview first (starts local preview server)
                    if let Some(preview_dir) = preview {
                        let config = crate::pages::preview::PreviewConfig {
                            site_dir: preview_dir,
                            port,
                            open_browser: !no_open,
                        };
                        crate::pages::preview::start_preview_server(config)
                            .await
                            .map_err(|e| CliError {
                                code: 9,
                                kind: "pages",
                                message: format!("Preview server failed: {e}"),
                                hint: Some(
                                    "Check that the directory exists and port is available"
                                        .to_string(),
                                ),
                                retryable: false,
                            })?;
                        return Ok(());
                    }

                    // Check for unencrypted export in robot mode
                    if no_encryption && (json || robot_mode) && !i_understand_unencrypted_risks {
                        let error = crate::pages::confirmation::robot_mode_blocked_error();
                        eprintln!("{}", serde_json::to_string_pretty(&error).unwrap());
                        return Err(CliError {
                            code: crate::pages::confirmation::EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED,
                            kind: "pages",
                            message: "Unencrypted exports are not allowed in robot mode"
                                .to_string(),
                            hint: Some(
                                "Use --i-understand-unencrypted-risks flag if you really need this"
                                    .to_string(),
                            ),
                            retryable: false,
                        });
                    }
                    // Handle --verify first
                    if let Some(verify_path) = verify {
                        let result = crate::pages::verify::verify_bundle(&verify_path, verbose)
                            .map_err(|e| CliError {
                                code: 9,
                                kind: "pages",
                                message: format!("Verification failed: {e}"),
                                hint: None,
                                retryable: false,
                            })?;

                        if json {
                            println!("{}", serde_json::to_string_pretty(&result).unwrap());
                        } else {
                            crate::pages::verify::print_result(&result, verbose);
                        }

                        // Exit with non-zero if invalid
                        if result.status != "valid" {
                            return Err(CliError {
                                code: 1,
                                kind: "pages",
                                message: "Bundle verification failed".to_string(),
                                hint: Some(
                                    "Run with --verbose for detailed error information".to_string(),
                                ),
                                retryable: false,
                            });
                        }
                    } else if scan_secrets {
                        let db_path = cli.db.clone().unwrap_or_else(|| {
                            directories::ProjectDirs::from(
                                "com",
                                "dicklesworthstone",
                                "coding-agent-search",
                            )
                            .map(|dirs| dirs.data_dir().join("agent_search.db"))
                            .unwrap_or_else(default_db_path)
                        });

                        let workspaces_path = workspaces
                            .clone()
                            .map(|ws| ws.into_iter().map(PathBuf::from).collect());

                        let filters = crate::pages::secret_scan::SecretScanFilters {
                            agents: agents.clone(),
                            workspaces: workspaces_path,
                            since_ts: since
                                .as_deref()
                                .and_then(crate::ui::time_parser::parse_time_input),
                            until_ts: until
                                .as_deref()
                                .and_then(crate::ui::time_parser::parse_time_input),
                        };

                        let config = crate::pages::secret_scan::SecretScanConfig::from_inputs(
                            &secrets_allow,
                            &secrets_deny,
                        )
                        .map_err(|e| CliError {
                            code: 9,
                            kind: "pages",
                            message: format!("Secret scan config error: {e}"),
                            hint: None,
                            retryable: false,
                        })?;

                        crate::pages::secret_scan::run_secret_scan_cli(
                            &db_path,
                            &filters,
                            &config,
                            json,
                            fail_on_secrets,
                        )
                        .map_err(|e| CliError {
                            code: 9,
                            kind: "pages",
                            message: format!("Secret scan failed: {e}"),
                            hint: None,
                            retryable: false,
                        })?;
                    } else if let Some(output_path) = export_only {
                        // Interactive unencrypted export confirmation (non-robot mode)
                        if no_encryption && !json && !robot_mode {
                            use console::style;
                            use std::io::Write;

                            eprintln!("{}", style("⚠️  SECURITY WARNING").red().bold());
                            eprintln!();
                            for line in crate::pages::confirmation::unencrypted_warning_lines() {
                                eprintln!("{}", line);
                            }
                            eprintln!();
                            eprintln!("To proceed, type exactly:");
                            eprintln!();
                            eprintln!(
                                "  {}",
                                style(crate::pages::confirmation::UNENCRYPTED_ACK_PHRASE).cyan()
                            );
                            eprintln!();
                            eprint!("Your input: ");
                            std::io::stderr().flush().ok();

                            let mut input = String::new();
                            std::io::stdin().read_line(&mut input).map_err(|e| CliError {
                                code: crate::pages::confirmation::EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED,
                                kind: "pages",
                                message: format!("Failed to read input: {e}"),
                                hint: None,
                                retryable: false,
                            })?;

                            match crate::pages::confirmation::validate_unencrypted_ack(&input) {
                                crate::pages::confirmation::StepValidation::Passed => {
                                    // Additional y/N confirmation
                                    eprintln!();
                                    eprint!("Are you ABSOLUTELY SURE? [y/N]: ");
                                    std::io::stderr().flush().ok();

                                    let mut confirm = String::new();
                                    std::io::stdin().read_line(&mut confirm).map_err(|e| CliError {
                                        code: crate::pages::confirmation::EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED,
                                        kind: "pages",
                                        message: format!("Failed to read input: {e}"),
                                        hint: None,
                                        retryable: false,
                                    })?;

                                    if confirm.trim().to_lowercase() != "y" {
                                        eprintln!();
                                        eprintln!("{}", style("Export cancelled.").green());
                                        eprintln!(
                                            "To export with encryption (recommended), remove --no-encryption"
                                        );
                                        return Err(CliError {
                                            code: crate::pages::confirmation::EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED,
                                            kind: "pages",
                                            message: "Unencrypted export not confirmed".to_string(),
                                            hint: Some("Remove --no-encryption to export with encryption (recommended)".to_string()),
                                            retryable: false,
                                        });
                                    }
                                }
                                crate::pages::confirmation::StepValidation::Failed(msg) => {
                                    eprintln!();
                                    eprintln!("{}", style("Export cancelled.").green());
                                    eprintln!("{}", msg);
                                    return Err(CliError {
                                        code: crate::pages::confirmation::EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED,
                                        kind: "pages",
                                        message: "Unencrypted export not confirmed".to_string(),
                                        hint: Some("Remove --no-encryption to export with encryption (recommended)".to_string()),
                                        retryable: false,
                                    });
                                }
                            }
                        }

                        crate::pages::export::run_pages_export(
                            cli.db.clone(),
                            output_path.clone(),
                            agents.clone(),
                            workspaces.clone(),
                            since.clone(),
                            until.clone(),
                            path_mode,
                            dry_run,
                        )
                        .map_err(|e| CliError {
                            code: 9,
                            kind: "pages",
                            message: format!("Export failed: {e}"),
                            hint: None,
                            retryable: false,
                        })?;
                    } else {
                        let cf_creds_provided = account_id.is_some() || api_token.is_some();
                        let target_is_cloudflare =
                            matches!(target, Some(PagesDeployTarget::Cloudflare));
                        let target_is_non_cloudflare = matches!(
                            target,
                            Some(PagesDeployTarget::GitHub | PagesDeployTarget::Local)
                        );

                        if target_is_non_cloudflare && cf_creds_provided {
                            let target_label = match target {
                                Some(PagesDeployTarget::GitHub) => "github",
                                Some(PagesDeployTarget::Local) => "local",
                                _ => "unknown",
                            };
                            return Err(CliError {
                                code: 2,
                                kind: "pages",
                                message: format!(
                                    "Cloudflare credentials provided but --target is {target_label}"
                                ),
                                hint: Some(
                                    "Use --target cloudflare or remove --account-id/--api-token."
                                        .to_string(),
                                ),
                                retryable: false,
                            });
                        }

                        if (target_is_cloudflare || (target.is_none() && cf_creds_provided))
                            && (account_id.is_some() ^ api_token.is_some())
                        {
                            return Err(CliError {
                                code: 2,
                                kind: "pages",
                                message: "Both --account-id and --api-token are required together"
                                    .to_string(),
                                hint: Some(
                                    "Provide both flags (or use CLOUDFLARE_* env vars) when deploying to Cloudflare."
                                        .to_string(),
                                ),
                                retryable: false,
                            });
                        }

                        // Wizard mode: pass flags
                        let mut wizard = crate::pages::wizard::PagesWizard::new();
                        if no_encryption {
                            wizard.set_no_encryption(true);
                        }
                        if include_attachments {
                            wizard.set_include_attachments(true);
                        }
                        if let Some(target) = target {
                            wizard.set_deploy_target(target.to_wizard_target());
                        } else if cf_creds_provided {
                            wizard.set_deploy_target(
                                crate::pages::wizard::DeployTarget::CloudflarePages,
                            );
                        }
                        if let Some(project) = project {
                            wizard.set_repo_name(project);
                        }
                        if let Some(branch) = branch {
                            wizard.set_cloudflare_branch(branch);
                        }
                        if let Some(account_id) = account_id {
                            wizard.set_cloudflare_account_id(account_id);
                        }
                        if let Some(api_token) = api_token {
                            wizard.set_cloudflare_api_token(api_token);
                        }
                        wizard.run().map_err(|e| CliError {
                            code: 9,
                            kind: "pages",
                            message: format!("Wizard failed: {e}"),
                            hint: None,
                            retryable: false,
                        })?;
                    }
                }
                Commands::Analytics(subcmd) => {
                    run_analytics(subcmd, cli.db.clone())?;
                }
                _ => {}
            }
        }
        _ => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .compact()
                .with_target(false)
                .with_ansi(
                    matches!(cli.color, ColorPref::Always)
                        || (matches!(cli.color, ColorPref::Auto) && stderr_is_tty),
                )
                .init();

            match command {
                Commands::Completions { shell } => {
                    let mut cmd = Cli::command();
                    clap_complete::generate(shell, &mut cmd, "cass", &mut std::io::stdout());
                }
                Commands::Man => {
                    let cmd = Cli::command();
                    let man = clap_mangen::Man::new(cmd);
                    man.render(&mut std::io::stdout())
                        .map_err(|e| CliError::unknown(format!("failed to render man: {e}")))?;
                }
                Commands::Capabilities { json } => {
                    run_capabilities(json)?;
                }
                Commands::ApiVersion { json } => {
                    run_api_version(json)?;
                }
                Commands::State {
                    data_dir,
                    json,
                    robot_meta,
                    stale_threshold,
                } => {
                    run_status(&data_dir, None, json, stale_threshold, robot_meta)?;
                }
                Commands::Introspect { json } => {
                    run_introspect(json)?;
                }
                Commands::Health {
                    data_dir,
                    json,
                    robot_meta,
                    stale_threshold,
                } => {
                    run_health(&data_dir, cli.db.clone(), json, stale_threshold, robot_meta)?;
                }
                Commands::Doctor {
                    data_dir,
                    json,
                    fix,
                    verbose,
                    force_rebuild,
                } => {
                    run_doctor(&data_dir, cli.db.clone(), json, fix, verbose, force_rebuild)?;
                }
                Commands::Context {
                    path,
                    data_dir,
                    json,
                    limit,
                } => {
                    run_context(&path, &data_dir, cli.db.clone(), json, limit)?;
                }
                Commands::Export {
                    path,
                    format,
                    output,
                    include_tools,
                } => {
                    run_export(&path, format, output.as_deref(), include_tools)?;
                }
                Commands::ExportHtml {
                    session,
                    output_dir,
                    filename,
                    encrypt,
                    password,
                    password_stdin,
                    include_tools,
                    show_timestamps,
                    no_cdns,
                    theme,
                    dry_run,
                    explain,
                    open,
                    json,
                } => {
                    run_export_html(
                        &session,
                        output_dir.as_deref(),
                        filename.as_deref(),
                        encrypt,
                        password.as_deref(),
                        password_stdin,
                        include_tools,
                        show_timestamps,
                        !no_cdns,
                        &theme,
                        dry_run,
                        explain,
                        open,
                        json,
                    )?;
                }
                Commands::Expand {
                    path,
                    line,
                    context,
                    json,
                } => {
                    run_expand(&path, line, context, json)?;
                }
                Commands::Timeline {
                    since,
                    until,
                    today,
                    agent,
                    data_dir,
                    json,
                    group_by,
                    source,
                } => {
                    run_timeline(
                        since.as_deref(),
                        until.as_deref(),
                        today,
                        &agent,
                        &data_dir,
                        cli.db.clone(),
                        json,
                        group_by,
                        source,
                    )?;
                }
                Commands::Sources(subcmd) => {
                    run_sources_command(subcmd)?;
                }
                Commands::Models(subcmd) => {
                    let subcmd = subcmd.clone();
                    let result = tokio::task::spawn_blocking(move || run_models_command(subcmd))
                        .await
                        .map_err(|err| CliError {
                            code: 70,
                            kind: "runtime",
                            message: format!("models command panicked: {err}"),
                            hint: Some(
                                "Retry the command; if it persists, report the panic output."
                                    .into(),
                            ),
                            retryable: true,
                        })?;
                    result?;
                }
                Commands::Import(subcmd) => {
                    handle_import(subcmd).await?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

async fn handle_import(cmd: ImportCommand) -> CliResult<()> {
    match cmd {
        ImportCommand::Chatgpt {
            path,
            output_dir,
            json,
        } => import_chatgpt_export(&path, output_dir.as_deref(), json).await,
    }
}

// ---------------------------------------------------------------------------
// Analytics command dispatch (br-z9fse.3.1)
// ---------------------------------------------------------------------------

/// Dispatch analytics subcommands.
///
/// Each arm validates its inputs and emits a deterministic JSON envelope on
/// success:
///
/// ```json
/// { "command": "analytics/<sub>", "data": { ... }, "_meta": { "elapsed_ms": N } }
/// ```
///
/// Implementation of the actual data queries is deferred to child beads;
/// these stubs return a well-formed "not yet implemented" response so
/// downstream consumers can parse the envelope immediately.
fn run_analytics(cmd: AnalyticsCommand, db_path: Option<PathBuf>) -> CliResult<()> {
    use std::time::Instant;

    let start = Instant::now();

    // Extract the common args and subcommand label from the variant.
    let (label, common) = match &cmd {
        AnalyticsCommand::Status { common } => ("status", common),
        AnalyticsCommand::Tokens { common, .. } => ("tokens", common),
        AnalyticsCommand::Tools { common, .. } => ("tools", common),
        AnalyticsCommand::AnalyticsModels { common, .. } => ("models", common),
        AnalyticsCommand::Cost { common, .. } => ("cost", common),
        AnalyticsCommand::Rebuild { common, .. } => ("rebuild", common),
        AnalyticsCommand::Validate { common, .. } => ("validate", common),
    };

    let json_mode = common.json;

    // Build a summary of the active filters for _meta.
    let filters = analytics_build_filters(common);

    // Dispatch to per-subcommand implementation.
    let data = match &cmd {
        AnalyticsCommand::Status { common } => run_analytics_status(common, db_path.as_ref())?,
        AnalyticsCommand::Tokens { common, group_by } => {
            run_analytics_tokens(common, *group_by, db_path.as_ref())?
        }
        AnalyticsCommand::Rebuild { common, force } => {
            run_analytics_rebuild(common, *force, db_path.as_ref())?
        }
        AnalyticsCommand::Tools {
            common,
            group_by,
            limit,
        } => run_analytics_tools(common, *group_by, *limit, db_path.as_ref())?,
        AnalyticsCommand::Validate { common, fix } => {
            run_analytics_validate(common, *fix, db_path.as_ref())?
        }
        AnalyticsCommand::Cost { common, group_by } => {
            run_analytics_cost(common, *group_by, db_path.as_ref())?
        }
        AnalyticsCommand::AnalyticsModels { common, group_by } => {
            run_analytics_models(common, *group_by, db_path.as_ref())?
        }
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;

    let envelope = serde_json::json!({
        "command": format!("analytics/{label}"),
        "data": data,
        "_meta": {
            "elapsed_ms": elapsed_ms,
            "filters_applied": filters,
            "data_dir": common.data_dir.as_ref().map(|p| p.display().to_string()),
        }
    });

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).unwrap_or_default()
        );
    } else {
        use colored::Colorize;
        let heading = match &cmd {
            AnalyticsCommand::Status { .. }
            | AnalyticsCommand::Tokens { .. }
            | AnalyticsCommand::Rebuild { .. }
            | AnalyticsCommand::Tools { .. }
            | AnalyticsCommand::Validate { .. }
            | AnalyticsCommand::Cost { .. }
            | AnalyticsCommand::AnalyticsModels { .. } => format!(
                "{} analytics {}",
                "cass".cyan().bold(),
                label.yellow().bold()
            ),
        };
        eprintln!("{heading}");
        if !filters.is_empty() {
            eprintln!("  filters: {}", filters.join(", ").dimmed());
        }
        eprintln!("  elapsed: {elapsed_ms}ms");
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).unwrap_or_default()
        );
    }

    Ok(())
}

/// Collect active filter descriptions for `_meta.filters_applied`.
fn analytics_build_filters(common: &AnalyticsCommon) -> Vec<String> {
    let mut f = Vec::new();
    if common.since.is_some() {
        f.push(format!("since={}", common.since.as_deref().unwrap_or("")));
    }
    if common.until.is_some() {
        f.push(format!("until={}", common.until.as_deref().unwrap_or("")));
    }
    if let Some(d) = common.days {
        f.push(format!("days={d}"));
    }
    for a in &common.agent {
        f.push(format!("agent={a}"));
    }
    for w in &common.workspace {
        f.push(format!("workspace={w}"));
    }
    if let Some(s) = &common.source {
        f.push(format!("source={s}"));
    }
    f
}

// ---------------------------------------------------------------------------
// analytics status — delegates to crate::analytics::query
// ---------------------------------------------------------------------------

/// Run `cass analytics status` — analytics health/quality endpoint.
fn run_analytics_status(
    common: &AnalyticsCommon,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    let lazy =
        crate::storage::sqlite::LazyDb::from_overrides(&common.data_dir, db_path_override.cloned());
    let conn = lazy.get("analytics-status").map_err(lazy_db_to_cli_error)?;
    let filter = analytics::AnalyticsFilter::from(common);

    analytics::query::query_status(&conn, &filter)
        .map(|r| r.to_json())
        .map_err(|e| CliError {
            code: 9,
            kind: "db-error",
            message: e.to_string(),
            hint: Some("Check that the analytics tables exist and are not corrupt.".into()),
            retryable: false,
        })
}

// ---------------------------------------------------------------------------
// analytics tokens — delegates to crate::analytics::query
// ---------------------------------------------------------------------------

/// Run `cass analytics tokens` — time-series token/usage analytics.
fn run_analytics_tokens(
    common: &AnalyticsCommon,
    group_by: AnalyticsBucketing,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    let lazy =
        crate::storage::sqlite::LazyDb::from_overrides(&common.data_dir, db_path_override.cloned());
    let conn = lazy.get("analytics-tokens").map_err(lazy_db_to_cli_error)?;
    let filter = analytics::AnalyticsFilter::from(common);

    analytics::query::query_tokens_timeseries(&conn, &filter, group_by.into())
        .map(|r| r.to_cli_json())
        .map_err(|e| CliError {
            code: 9,
            kind: "db-error",
            message: e.to_string(),
            hint: Some("Check that the analytics tables exist and are not corrupt.".into()),
            retryable: false,
        })
}

// ---------------------------------------------------------------------------
// analytics rebuild (br-z9fse.3.4)
// ---------------------------------------------------------------------------

/// Run `cass analytics rebuild` — rebuild analytics rollup tables.
///
/// Currently rebuilds Track A (message_metrics + usage_hourly + usage_daily).
/// Track B rebuild will be wired when z9fse.13 lands.
fn run_analytics_rebuild(
    common: &AnalyticsCommon,
    _force: bool,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    use crate::storage::sqlite::SqliteStorage;

    let data_dir = common.data_dir.clone().unwrap_or_else(default_data_dir);
    let db_path = db_path_override
        .cloned()
        .unwrap_or_else(|| data_dir.join("agent_search.db"));

    if !db_path.exists() {
        return Err(CliError {
            code: 3,
            kind: "missing-db",
            message: format!(
                "Database not found at {}. Run 'cass index --full' first.",
                db_path.display()
            ),
            hint: Some("Run 'cass index --full' to create the database.".into()),
            retryable: false,
        });
    }

    // Progress diagnostics go to stderr.
    eprintln!("Rebuilding analytics (Track A)...");

    let mut storage = SqliteStorage::open(&db_path).map_err(|e| CliError {
        code: 9,
        kind: "db-error",
        message: format!("Failed to open database: {e}"),
        hint: None,
        retryable: false,
    })?;

    let result = storage.rebuild_analytics().map_err(|e| CliError {
        code: 9,
        kind: "rebuild-error",
        message: format!("Analytics rebuild failed: {e}"),
        hint: Some("Check database integrity with 'cass health --json'.".into()),
        retryable: true,
    })?;

    eprintln!(
        "Rebuild complete: {} message_metrics, {} hourly, {} daily rows in {}ms ({:.0} msg/sec)",
        result.message_metrics_rows,
        result.usage_hourly_rows,
        result.usage_daily_rows,
        result.elapsed_ms,
        result.messages_per_sec
    );

    Ok(serde_json::json!({
        "track": "a",
        "tracks_rebuilt": ["a"],
        "track_a": {
            "message_metrics_rows": result.message_metrics_rows,
            "usage_hourly_rows": result.usage_hourly_rows,
            "usage_daily_rows": result.usage_daily_rows,
            "usage_models_daily_rows": result.usage_models_daily_rows,
            "elapsed_ms": result.elapsed_ms,
            "rows_per_sec": result.messages_per_sec,
        },
        "overall_elapsed_ms": result.elapsed_ms,
    }))
}

// ---------------------------------------------------------------------------
// analytics tools (br-z9fse.3.9)
// ---------------------------------------------------------------------------

/// Run `cass analytics tools` — per-tool invocation counts and derived metrics.
fn run_analytics_tools(
    common: &AnalyticsCommon,
    group_by: AnalyticsBucketing,
    limit: usize,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    let lazy =
        crate::storage::sqlite::LazyDb::from_overrides(&common.data_dir, db_path_override.cloned());
    let conn = lazy.get("analytics-tools").map_err(lazy_db_to_cli_error)?;
    let filter = analytics::AnalyticsFilter::from(common);

    analytics::query::query_tools(&conn, &filter, group_by.into(), limit)
        .map(|r| r.to_cli_json())
        .map_err(|e| CliError {
            code: 9,
            kind: "db-error",
            message: e.to_string(),
            hint: Some("Check that the analytics tables exist and are not corrupt.".into()),
            retryable: false,
        })
}

// ---------------------------------------------------------------------------
// analytics validate (br-z9fse.3.5)
// ---------------------------------------------------------------------------

/// Run `cass analytics validate` — rollup invariant checks and drift detection.
fn run_analytics_validate(
    common: &AnalyticsCommon,
    _fix: bool,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    let lazy =
        crate::storage::sqlite::LazyDb::from_overrides(&common.data_dir, db_path_override.cloned());
    let conn = lazy
        .get("analytics-validate")
        .map_err(lazy_db_to_cli_error)?;

    let config = analytics::ValidateConfig::default();
    let report = analytics::validate::run_validation(&conn, &config);

    // Performance guardrails
    let perf_ts = analytics::validate::perf_query_guardrail(&conn);
    let perf_bd = analytics::validate::perf_breakdown_guardrail(&conn);

    // Summary counts
    let errors = report
        .checks
        .iter()
        .filter(|c| c.severity == analytics::validate::Severity::Error)
        .count();
    let warnings = report
        .checks
        .iter()
        .filter(|c| c.severity == analytics::validate::Severity::Warning)
        .count();
    let drift_entries = report.drift.len();

    // Human-readable stderr output
    {
        use colored::Colorize;
        if errors > 0 {
            eprintln!(
                "  {} {} error(s), {} warning(s), {} drift entries",
                "FAIL".red().bold(),
                errors,
                warnings,
                drift_entries
            );
        } else if warnings > 0 {
            eprintln!(
                "  {} {} warning(s), {} drift entries",
                "WARN".yellow().bold(),
                warnings,
                drift_entries
            );
        } else {
            eprintln!("  {} all checks passed", "OK".green().bold());
        }
        let ts_status = if perf_ts.within_budget {
            "OK".green().to_string()
        } else {
            "SLOW".red().to_string()
        };
        let bd_status = if perf_bd.within_budget {
            "OK".green().to_string()
        } else {
            "SLOW".red().to_string()
        };
        eprintln!(
            "  perf: timeseries {}ms [{}], breakdown {}ms [{}]",
            perf_ts.elapsed_ms, ts_status, perf_bd.elapsed_ms, bd_status
        );
    }

    Ok(serde_json::json!({
        "summary": {
            "errors": errors,
            "warnings": warnings,
            "drift_entries": drift_entries,
            "buckets_checked": report._meta.sampling.buckets_checked,
            "buckets_total": report._meta.sampling.buckets_total,
        },
        "checks": report.checks,
        "drift": report.drift,
        "perf": {
            "timeseries": {
                "elapsed_ms": perf_ts.elapsed_ms,
                "budget_ms": perf_ts.budget_ms,
                "within_budget": perf_ts.within_budget,
            },
            "breakdown": {
                "elapsed_ms": perf_bd.elapsed_ms,
                "budget_ms": perf_bd.budget_ms,
                "within_budget": perf_bd.within_budget,
            }
        }
    }))
}

// ---------------------------------------------------------------------------
// analytics cost (br-z9fse.3.7)
// ---------------------------------------------------------------------------

/// Run `cass analytics cost` — USD cost estimates with pricing coverage.
fn run_analytics_cost(
    common: &AnalyticsCommon,
    group_by: AnalyticsBucketing,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    let lazy =
        crate::storage::sqlite::LazyDb::from_overrides(&common.data_dir, db_path_override.cloned());
    let conn = lazy.get("analytics-cost").map_err(lazy_db_to_cli_error)?;
    let filter = analytics::AnalyticsFilter::from(common);
    let db_err = |e: crate::analytics::AnalyticsError| CliError {
        code: 9,
        kind: "db-error",
        message: e.to_string(),
        hint: Some("Check that the analytics tables exist and are not corrupt.".into()),
        retryable: false,
    };

    // Time-bucketed cost from Track B (token_daily_stats has estimated_cost_usd).
    let ts =
        analytics::query::query_cost_timeseries(&conn, &filter, group_by.into()).map_err(db_err)?;

    // Model breakdown (cost by model family).
    let by_model = analytics::query::query_breakdown(
        &conn,
        &filter,
        analytics::Dim::Model,
        analytics::Metric::EstimatedCostUsd,
        50,
    )
    .map_err(db_err)?;

    // Agent breakdown (cost by agent).
    let by_agent = analytics::query::query_breakdown(
        &conn,
        &filter,
        analytics::Dim::Agent,
        analytics::Metric::EstimatedCostUsd,
        50,
    )
    .map_err(db_err)?;

    // Coverage information from status query.
    let status = analytics::query::query_status(&conn, &filter).map_err(db_err)?;

    // Unknown/unpriced models.
    let unpriced = analytics::query::query_unpriced_models(&conn, 20).map_err(db_err)?;

    // Totals across all buckets.
    let total_cost: f64 = ts.buckets.iter().map(|(_, b)| b.estimated_cost_usd).sum();
    let total_api_tokens: i64 = ts.buckets.iter().map(|(_, b)| b.api_tokens_total).sum();
    let total_messages: i64 = ts.buckets.iter().map(|(_, b)| b.message_count).sum();
    let cost_per_1k_api = if total_api_tokens > 0 {
        Some(total_cost / (total_api_tokens as f64 / 1000.0))
    } else {
        None
    };
    let cost_per_message = if total_messages > 0 {
        Some(total_cost / total_messages as f64)
    } else {
        None
    };

    // Period-over-period delta: compare to prior period of same length.
    let period_delta = compute_cost_period_delta(&ts);

    // Buckets as JSON array (already sorted by key in timeseries result).
    let buckets_json: Vec<serde_json::Value> = ts
        .buckets
        .iter()
        .map(|(key, b)| {
            serde_json::json!({
                "bucket": key,
                "estimated_cost_usd": b.estimated_cost_usd,
                "api_tokens_total": b.api_tokens_total,
                "message_count": b.message_count,
            })
        })
        .collect();

    // Unpriced models JSON.
    let unpriced_json: Vec<serde_json::Value> = unpriced
        .models
        .iter()
        .map(|m| {
            serde_json::json!({
                "model_name": m.model_name,
                "total_tokens": m.total_tokens,
                "row_count": m.row_count,
            })
        })
        .collect();

    let total_token_volume = unpriced.total_priced_tokens + unpriced.total_unpriced_tokens;
    let unpriced_token_share = if total_token_volume > 0 {
        Some((unpriced.total_unpriced_tokens as f64 / total_token_volume as f64) * 100.0)
    } else {
        None
    };

    // Human-readable stderr summary.
    {
        use colored::Colorize;
        eprintln!(
            "  total cost: {}",
            format!("${total_cost:.4}").green().bold()
        );
        if let Some(cpm) = cost_per_message {
            eprintln!("  cost/message: ${cpm:.6}");
        }
        if let Some(cpt) = cost_per_1k_api {
            eprintln!("  cost/1k API tokens: ${cpt:.6}");
        }
        if let Some((delta_pct, _)) = &period_delta {
            let sign = if *delta_pct >= 0.0 { "+" } else { "" };
            let delta_str = format!("{sign}{delta_pct:.1}%");
            let colored_delta = if *delta_pct > 10.0 {
                delta_str.red()
            } else if *delta_pct < -10.0 {
                delta_str.green()
            } else {
                delta_str.yellow()
            };
            eprintln!("  period-over-period: {colored_delta}");
        }
        let cov = status.coverage.pricing_coverage_pct;
        let cov_color = if cov > 80.0 {
            format!("{cov:.1}%").green()
        } else if cov > 50.0 {
            format!("{cov:.1}%").yellow()
        } else {
            format!("{cov:.1}%").red()
        };
        eprintln!("  pricing coverage: {cov_color}");
        if !unpriced.models.is_empty() {
            eprintln!(
                "  unpriced models: {} ({})",
                unpriced.models.len().to_string().red(),
                unpriced_token_share
                    .map(|s| format!("{s:.1}% of token volume"))
                    .unwrap_or_default()
                    .dimmed()
            );
        }
        eprintln!(
            "  breakdowns: {} models, {} agents",
            by_model.rows.len().to_string().cyan(),
            by_agent.rows.len().to_string().cyan()
        );
    }

    Ok(serde_json::json!({
        "totals": {
            "estimated_cost_usd": total_cost,
            "api_tokens_total": total_api_tokens,
            "message_count": total_messages,
            "cost_per_1k_api_tokens": cost_per_1k_api,
            "cost_per_message": cost_per_message,
            "period_delta_pct": period_delta.as_ref().map(|(pct, _)| *pct),
            "prior_period_cost_usd": period_delta.as_ref().map(|(_, prior)| *prior),
        },
        "pricing_coverage": {
            "pricing_coverage_pct": status.coverage.pricing_coverage_pct,
            "api_token_coverage_pct": status.coverage.api_token_coverage_pct,
            "model_name_coverage_pct": status.coverage.model_name_coverage_pct,
            "estimate_only_pct": status.coverage.estimate_only_pct,
            "unpriced_token_share_pct": unpriced_token_share,
            "unpriced_models": unpriced_json,
        },
        "buckets": buckets_json,
        "by_model": by_model.to_cli_json(),
        "by_agent": by_agent.to_cli_json(),
    }))
}

/// Compute period-over-period cost delta from time-bucketed data.
///
/// Splits the buckets into two halves (first half = prior, second half = current)
/// and returns `(delta_pct, prior_cost)`.  Returns `None` when fewer than 2 buckets.
fn compute_cost_period_delta(ts: &analytics::TimeseriesResult) -> Option<(f64, f64)> {
    let n = ts.buckets.len();
    if n < 2 {
        return None;
    }
    let mid = n / 2;
    let prior_cost: f64 = ts.buckets[..mid]
        .iter()
        .map(|(_, b)| b.estimated_cost_usd)
        .sum();
    let current_cost: f64 = ts.buckets[mid..]
        .iter()
        .map(|(_, b)| b.estimated_cost_usd)
        .sum();
    if prior_cost.abs() < f64::EPSILON {
        return None;
    }
    let delta_pct = ((current_cost - prior_cost) / prior_cost) * 100.0;
    Some((delta_pct, prior_cost))
}

// ---------------------------------------------------------------------------
// analytics models (br-z9fse.3.6)
// ---------------------------------------------------------------------------

/// Run `cass analytics models` — top models by usage and coverage statistics.
fn run_analytics_models(
    common: &AnalyticsCommon,
    group_by: AnalyticsBucketing,
    db_path_override: Option<&PathBuf>,
) -> CliResult<serde_json::Value> {
    let lazy =
        crate::storage::sqlite::LazyDb::from_overrides(&common.data_dir, db_path_override.cloned());
    let conn = lazy.get("analytics-models").map_err(lazy_db_to_cli_error)?;
    let filter = analytics::AnalyticsFilter::from(common);
    let db_err = |e: crate::analytics::AnalyticsError| CliError {
        code: 9,
        kind: "db-error",
        message: e.to_string(),
        hint: Some("Check that the analytics tables exist and are not corrupt.".into()),
        retryable: false,
    };

    // Model breakdown by API tokens.
    let by_tokens = analytics::query::query_breakdown(
        &conn,
        &filter,
        analytics::Dim::Model,
        analytics::Metric::ApiTotal,
        50,
    )
    .map_err(db_err)?;

    // Model breakdown by cost.
    let by_cost = analytics::query::query_breakdown(
        &conn,
        &filter,
        analytics::Dim::Model,
        analytics::Metric::EstimatedCostUsd,
        50,
    )
    .map_err(db_err)?;

    // Time series for aggregate stats.
    let ts = analytics::query::query_tokens_timeseries(&conn, &filter, group_by.into())
        .map_err(db_err)?;

    // Human-readable stderr summary.
    {
        use colored::Colorize;
        eprintln!(
            "  {} models (by API tokens)",
            by_tokens.rows.len().to_string().cyan().bold()
        );
        for (i, row) in by_tokens.rows.iter().take(5).enumerate() {
            eprintln!(
                "    {}. {} — {} API tokens",
                i + 1,
                row.key.yellow(),
                row.bucket.api_tokens_total
            );
        }
        if by_tokens.rows.len() > 5 {
            eprintln!("    ... and {} more", by_tokens.rows.len() - 5);
        }
    }

    Ok(serde_json::json!({
        "by_api_tokens": by_tokens.to_cli_json(),
        "by_cost": by_cost.to_cli_json(),
        "timeseries": ts.to_cli_json(),
    }))
}

async fn import_chatgpt_export(
    export_path: &Path,
    output_dir: Option<&Path>,
    json_output: bool,
) -> CliResult<()> {
    use std::io::Write;

    // Validate export file exists
    if !export_path.exists() {
        return Err(CliError {
            code: 1,
            kind: "io_error",
            message: format!("Export file not found: {}", export_path.display()),
            hint: Some(
                "Provide the path to conversations.json from ChatGPT web export \
                 (Settings \u{2192} Data Controls \u{2192} Export)"
                    .into(),
            ),
            retryable: false,
        });
    }

    // Determine output directory
    let base_dir = if let Some(dir) = output_dir {
        dir.to_path_buf()
    } else {
        // Try macOS ChatGPT app support dir first
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir()
                .map(|h| h.join("Library/Application Support/com.openai.chat"))
                .unwrap_or_else(|| {
                    dirs::data_local_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("cass/chatgpt")
                })
        }
        #[cfg(not(target_os = "macos"))]
        {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("cass/chatgpt")
        }
    };

    let conv_dir = base_dir.join("conversations-web-export");
    std::fs::create_dir_all(&conv_dir).map_err(|e| CliError {
        code: 1,
        kind: "io_error",
        message: format!("Failed to create output directory: {e}"),
        hint: None,
        retryable: false,
    })?;

    // Read and parse export file
    let content = std::fs::read_to_string(export_path).map_err(|e| CliError {
        code: 1,
        kind: "io_error",
        message: format!("Failed to read export file: {e}"),
        hint: None,
        retryable: false,
    })?;

    let conversations: Vec<serde_json::Value> =
        serde_json::from_str(&content).map_err(|e| CliError {
            code: 1,
            kind: "parse_error",
            message: format!("Failed to parse conversations.json: {e}"),
            hint: Some("Expected a JSON array of conversation objects".into()),
            retryable: false,
        })?;

    let total = conversations.len();
    let mut imported = 0u64;
    let mut skipped = 0u64;

    for (i, conv) in conversations.iter().enumerate() {
        // Extract conversation ID
        let conv_id = conv
            .get("id")
            .or_else(|| conv.get("conversation_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("conv-{i}"));

        let filepath = conv_dir.join(format!("{conv_id}.json"));

        // Idempotent: skip if already exists
        if filepath.exists() {
            skipped += 1;
            continue;
        }

        // Write individual conversation file
        let mut file = std::fs::File::create(&filepath).map_err(|e| CliError {
            code: 1,
            kind: "io_error",
            message: format!("Failed to write {}: {e}", filepath.display()),
            hint: None,
            retryable: false,
        })?;
        serde_json::to_writer(&mut file, conv).map_err(|e| CliError {
            code: 1,
            kind: "io_error",
            message: format!("Failed to serialize conversation: {e}"),
            hint: None,
            retryable: false,
        })?;
        file.flush().map_err(|e| CliError {
            code: 1,
            kind: "io_error",
            message: format!("Failed to flush: {e}"),
            hint: None,
            retryable: false,
        })?;

        imported += 1;
    }

    if json_output {
        let result = serde_json::json!({
            "success": true,
            "total": total,
            "imported": imported,
            "skipped": skipped,
            "output_dir": conv_dir.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        println!("Import complete!");
        println!("  Total conversations: {total}");
        println!("  Newly imported:      {imported}");
        println!("  Skipped (existing):  {skipped}");
        println!("  Output directory:    {}", conv_dir.display());
        println!();
        println!("Next step: Run `cass index` to index the conversations.");
    }

    Ok(())
}

/// Compute lightweight state snapshot (index/db freshness) for robot meta and state command reuse
fn state_meta_json(
    data_dir: &Path,
    db_path: &Path,
    stale_threshold: u64,
    allow_db_open: bool,
) -> serde_json::Value {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Use the actual versioned index path (index/v4, not tantivy_index)
    let index_path = crate::search::tantivy::index_dir(data_dir)
        .unwrap_or_else(|_| data_dir.join("index").join("v4"));
    let index_exists = index_path.exists();
    let db_exists = db_path.exists();
    let watch_state_path = data_dir.join("watch_state.json");

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut conversation_count: i64 = 0;
    let mut message_count: i64 = 0;
    let mut last_indexed_at: Option<i64> = None;
    let mut db_opened = false;

    // Use LazyDb to get timing/logging for state snapshot DB access
    let lazy = crate::storage::sqlite::LazyDb::new(db_path.to_path_buf());
    if allow_db_open
        && db_exists
        && let Ok(conn) = lazy.get("state-meta")
    {
        db_opened = true;
        conversation_count = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap_or(0);
        message_count = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap_or(0);
        last_indexed_at = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_indexed_at'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse::<i64>().ok());
    }

    if last_indexed_at.is_none() && index_exists {
        let meta_path = index_path.join("meta.json");
        let probe_path = if meta_path.exists() {
            meta_path
        } else {
            index_path.clone()
        };
        last_indexed_at = fs::metadata(&probe_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
    }

    let pending_sessions = if watch_state_path.exists() {
        std::fs::read_to_string(&watch_state_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|v| v.get("pending_count").and_then(serde_json::Value::as_u64))
            .unwrap_or(0)
    } else {
        0
    };

    let index_age_secs = last_indexed_at.map(|ts| {
        let ts_secs = ts / 1000;
        now_secs.saturating_sub(ts_secs as u64)
    });
    let is_stale = match index_age_secs {
        None => true,
        Some(age) => age > stale_threshold,
    };
    let fresh = index_exists && !is_stale;

    let ts_str = chrono::DateTime::from_timestamp(now_secs as i64, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();

    serde_json::json!({
        "index": {
            "exists": index_exists,
            "fresh": fresh,
            "last_indexed_at": last_indexed_at.map(|ts| {
                chrono::DateTime::from_timestamp_millis(ts)
                    .unwrap_or_else(chrono::Utc::now)
                    .to_rfc3339()
            }),
            "age_seconds": index_age_secs,
            "stale": is_stale,
            "stale_threshold_seconds": stale_threshold
        },
        "database": {
            "exists": db_exists,
            "opened": db_opened,
            "conversations": conversation_count,
            "messages": message_count
        },
        "pending": {
            "sessions": pending_sessions,
            "watch_active": watch_state_path.exists()
        },
        "_meta": {
            "timestamp": ts_str,
            "data_dir": data_dir.display().to_string(),
            "db_path": db_path.display().to_string()
        }
    })
}

fn state_index_freshness(state: &serde_json::Value) -> Option<serde_json::Value> {
    let index = state.get("index")?;
    let pending = state.get("pending");
    Some(serde_json::json!({
        "exists": index.get("exists"),
        "fresh": index.get("fresh"),
        "last_indexed_at": index.get("last_indexed_at"),
        "age_seconds": index.get("age_seconds"),
        "stale": index.get("stale"),
        "stale_threshold_seconds": index.get("stale_threshold_seconds"),
        "pending_sessions": pending.and_then(|p| p.get("sessions"))
    }))
}

fn warn_tui_terminal_profile(stderr_is_tty: bool) {
    if !stderr_is_tty {
        return;
    }

    let env_truthy = |raw: Option<String>| {
        raw.map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
    };

    let no_color_global = dotenvy::var("NO_COLOR").is_ok();
    let no_color_local = dotenvy::var("CASS_NO_COLOR").is_ok();
    let respect_global = env_truthy(dotenvy::var("CASS_RESPECT_NO_COLOR").ok());
    let effective_no_color = no_color_local || (respect_global && no_color_global);
    if effective_no_color {
        eprintln!(
            "warning: CASS_NO_COLOR/NO_COLOR profile is active; cass TUI styling may be reduced."
        );
        eprintln!(
            "hint: use `CASS_NO_COLOR=1` for monochrome, or `CASS_RESPECT_NO_COLOR=1` if you want global NO_COLOR honored."
        );
    } else if no_color_global {
        eprintln!(
            "info: NO_COLOR is set but ignored by default. Set CASS_RESPECT_NO_COLOR=1 to honor it."
        );
    }

    let term = dotenvy::var("TERM").unwrap_or_default();
    if term.trim().eq_ignore_ascii_case("dumb") && dotenvy::var("TUI_HEADLESS").is_err() {
        eprintln!(
            "warning: TERM=dumb detected; cass will apply a compatibility profile unless CASS_ALLOW_DUMB_TERM=1."
        );
        eprintln!(
            "hint: try `env -u NO_COLOR TERM=xterm-256color COLORTERM=truecolor cass` for full rendering."
        );
    }
}

fn configure_color(choice: ColorPref, stdout_is_tty: bool, stderr_is_tty: bool) {
    let enabled = match choice {
        ColorPref::Always => true,
        ColorPref::Never => false,
        ColorPref::Auto => stdout_is_tty || stderr_is_tty,
    };
    colored::control::set_override(enabled);
}

fn resolve_progress(mode: ProgressMode, stdout_is_tty: bool) -> ProgressResolved {
    match mode {
        ProgressMode::Bars => ProgressResolved::Bars,
        ProgressMode::Plain => ProgressResolved::Plain,
        ProgressMode::None => ProgressResolved::None,
        ProgressMode::Auto => {
            if stdout_is_tty {
                ProgressResolved::Bars
            } else {
                ProgressResolved::Plain
            }
        }
    }
}

/// Convert [`LazyDbError`] to the CLI-appropriate [`CliError`].
fn lazy_db_to_cli_error(e: crate::storage::sqlite::LazyDbError) -> CliError {
    use crate::storage::sqlite::LazyDbError;
    match e {
        LazyDbError::NotFound(path) => CliError {
            code: 3,
            kind: "missing-db",
            message: format!(
                "Database not found at {}. Run 'cass index --full' first.",
                path.display()
            ),
            hint: Some("Run 'cass index --full' to create the database.".into()),
            retryable: true,
        },
        LazyDbError::OpenFailed { path, source } => CliError {
            code: 9,
            kind: "db-open",
            message: format!("Failed to open database at {}: {source}", path.display()),
            hint: None,
            retryable: false,
        },
    }
}

fn describe_command(cli: &Cli) -> String {
    match &cli.command {
        Some(Commands::Tui { .. }) => "tui".to_string(),
        Some(Commands::Index { .. }) => "index".to_string(),
        Some(Commands::Search { .. }) => "search".to_string(),
        Some(Commands::Stats { .. }) => "stats".to_string(),
        Some(Commands::Diag { .. }) => "diag".to_string(),
        Some(Commands::Status { .. }) => "status".to_string(),
        Some(Commands::View { .. }) => "view".to_string(),
        Some(Commands::Completions { .. }) => "completions".to_string(),
        Some(Commands::Man) => "man".to_string(),
        Some(Commands::Capabilities { .. }) => "capabilities".to_string(),
        Some(Commands::ApiVersion { .. }) => "api-version".to_string(),
        Some(Commands::State { .. }) => "state".to_string(),
        Some(Commands::Introspect { .. }) => "introspect".to_string(),
        Some(Commands::RobotDocs { topic }) => format!("robot-docs:{topic:?}"),
        Some(Commands::Health { .. }) => "health".to_string(),
        Some(Commands::Doctor { .. }) => "doctor".to_string(),
        Some(Commands::Context { .. }) => "context".to_string(),
        Some(Commands::Export { .. }) => "export".to_string(),
        Some(Commands::ExportHtml { .. }) => "export-html".to_string(),
        Some(Commands::Expand { .. }) => "expand".to_string(),
        Some(Commands::Timeline { .. }) => "timeline".to_string(),
        Some(Commands::Sources(..)) => "sources".to_string(),
        Some(Commands::Models(..)) => "models".to_string(),
        Some(Commands::Pages { .. }) => "pages".to_string(),
        Some(Commands::Import(..)) => "import".to_string(),
        Some(Commands::Analytics(..)) => "analytics".to_string(),
        None => "(default)".to_string(),
    }
}

/// Returns true if the command is using robot/JSON output mode.
/// Used to auto-suppress INFO logs for clean machine-parseable output.
fn is_robot_mode(command: &Commands) -> bool {
    // Env-driven output formats should behave like robot mode (suppresses INFO logs).
    let env_robot_mode = robot_format_from_env().is_some();

    match command {
        Commands::Search {
            json,
            robot_format,
            robot_meta,
            ..
        } => *json || robot_format.is_some() || *robot_meta || env_robot_mode,
        Commands::Index { json, .. } => *json || env_robot_mode,
        Commands::Stats { json, .. } => *json || env_robot_mode,
        Commands::Diag { json, .. } => *json || env_robot_mode,
        Commands::Status { json, .. } => *json || env_robot_mode,
        Commands::Health { json, .. } => *json || env_robot_mode,
        Commands::Doctor { json, .. } => *json || env_robot_mode,
        Commands::ApiVersion { json, .. } => *json || env_robot_mode,
        Commands::State { json, .. } => *json || env_robot_mode,
        Commands::View { json, .. } => *json || env_robot_mode,
        Commands::Capabilities { json, .. } => *json || env_robot_mode,
        Commands::Introspect { json, .. } => *json || env_robot_mode,
        Commands::Context { json, .. } => *json || env_robot_mode,
        Commands::Expand { json, .. } => *json || env_robot_mode,
        Commands::ExportHtml { json, .. } => *json || env_robot_mode,
        Commands::Timeline { json, .. } => *json || env_robot_mode,
        Commands::Sources(cmd) => match cmd {
            // Only `sources list` honors env-based structured output today.
            SourcesCommand::List { json, .. } => *json || env_robot_mode,
            SourcesCommand::Doctor { json, .. }
            | SourcesCommand::Sync { json, .. }
            | SourcesCommand::Discover { json, .. }
            | SourcesCommand::Setup { json, .. } => *json,
            _ => false,
        },
        Commands::Import(cmd) => match cmd {
            ImportCommand::Chatgpt { json, .. } => *json || env_robot_mode,
        },
        Commands::Analytics(cmd) => {
            let json = match cmd {
                AnalyticsCommand::Status { common, .. }
                | AnalyticsCommand::Tokens { common, .. }
                | AnalyticsCommand::Tools { common, .. }
                | AnalyticsCommand::AnalyticsModels { common, .. }
                | AnalyticsCommand::Cost { common, .. }
                | AnalyticsCommand::Rebuild { common, .. }
                | AnalyticsCommand::Validate { common, .. } => common.json,
            };
            json || env_robot_mode
        }
        _ => false,
    }
}

fn apply_wrap(line: &str, wrap: WrapConfig) -> String {
    let width = wrap.effective_width();
    if line.trim().is_empty() || width.is_none() {
        return line.trim_end().to_string();
    }
    let width = width.unwrap_or(usize::MAX);
    if line.len() <= width {
        return line.trim_end().to_string();
    }

    let mut out = String::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        if current.len() + word.len() + 1 > width && !current.is_empty() {
            out.push_str(current.trim_end());
            out.push('\n');
            current.clear();
        }
        current.push_str(word);
        current.push(' ');
    }
    if !current.is_empty() {
        out.push_str(current.trim_end());
    }
    out
}

fn lowercase_with_map(text: &str) -> (String, Vec<usize>, Vec<(usize, usize)>) {
    let mut lower = String::with_capacity(text.len());
    let mut lower_starts = Vec::new();
    let mut orig_ranges = Vec::new();

    for (orig_idx, ch) in text.char_indices() {
        let orig_end = orig_idx + ch.len_utf8();
        for lower_ch in ch.to_lowercase() {
            lower_starts.push(lower.len());
            lower.push(lower_ch);
            orig_ranges.push((orig_idx, orig_end));
        }
    }

    (lower, lower_starts, orig_ranges)
}

fn lower_char_index(lower_starts: &[usize], lower_len: usize, byte_idx: usize) -> Option<usize> {
    if byte_idx == lower_len {
        return Some(lower_starts.len());
    }
    let idx = lower_starts.partition_point(|&b| b < byte_idx);
    if idx < lower_starts.len() && lower_starts[idx] == byte_idx {
        Some(idx)
    } else {
        None
    }
}

fn map_lower_range(
    lower_starts: &[usize],
    lower_len: usize,
    orig_ranges: &[(usize, usize)],
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    let start_idx = lower_char_index(lower_starts, lower_len, start)?;
    let end_idx = lower_char_index(lower_starts, lower_len, end)?;
    if start_idx >= end_idx {
        return None;
    }
    let orig_start = orig_ranges.get(start_idx)?.0;
    let orig_end = orig_ranges.get(end_idx - 1)?.1;
    Some((orig_start, orig_end))
}

/// Highlight matching search terms in text
///
/// Extracts query terms and wraps matches with the specified markers.
/// Uses case-insensitive matching. Handles quoted phrases and individual terms.
///
/// # Arguments
/// * `text` - The text to highlight matches in
/// * `query` - The search query to extract terms from
/// * `start_mark` - Opening marker (e.g., "**" for markdown bold, "<mark>" for HTML)
/// * `end_mark` - Closing marker (e.g., "**" for markdown bold, "</mark>" for HTML)
fn highlight_matches(text: &str, query: &str, start_mark: &str, end_mark: &str) -> String {
    // Extract search terms from query (handles quoted phrases and individual words)
    let terms = extract_search_terms(query);
    if terms.is_empty() {
        return text.to_string();
    }

    // Sort terms by length (longest first) to avoid partial matches
    let mut terms: Vec<_> = terms.into_iter().collect();
    terms.sort_by_key(|s| std::cmp::Reverse(s.len()));

    let mut result = text.to_string();
    for term in &terms {
        if term.is_empty() {
            continue;
        }
        // Case-insensitive replacement
        // Build a lowercase copy with a byte-level map back to original indices to
        // avoid slicing bugs when Unicode case-folding changes string length.
        let (lower_result, lower_starts, orig_ranges) = lowercase_with_map(&result);
        let lower_term = term.to_lowercase();
        let mut new_result = String::new();
        let mut last_end = 0;

        for (idx, matched_str) in lower_result.match_indices(&lower_term) {
            let end = idx + matched_str.len();
            let Some((orig_start, orig_end)) =
                map_lower_range(&lower_starts, lower_result.len(), &orig_ranges, idx, end)
            else {
                continue;
            };

            // Skip if this overlaps with a previous highlight (from a longer term)
            if orig_start < last_end {
                continue;
            }
            // Append text before this match
            new_result.push_str(&result[last_end..orig_start]);
            // Append highlighted match (preserve original case)
            new_result.push_str(start_mark);
            new_result.push_str(&result[orig_start..orig_end]);
            new_result.push_str(end_mark);
            last_end = orig_end;
        }
        // Append remaining text
        new_result.push_str(&result[last_end..]);
        result = new_result;
    }

    result
}

/// Extract meaningful search terms from a query string
///
/// Handles:
/// - Quoted phrases: "exact phrase" -> ["exact phrase"]
/// - Regular words: word -> ["word"]
/// - Field filters: agent:claude -> ignored (filter, not content term)
/// - Operators: AND, OR, NOT -> ignored
fn extract_search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut chars = query.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '"' {
            // Quoted phrase
            let mut phrase = String::new();
            while let Some(&next) = chars.peek() {
                if next == '"' {
                    chars.next();
                    break;
                }
                phrase.push(chars.next().unwrap());
            }
            if !phrase.is_empty() {
                terms.push(phrase);
            }
        } else if c.is_alphanumeric() || c == '_' || c == '-' {
            // Word (might be a field filter like agent:foo)
            let mut word = String::from(c);
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' || next == '-' {
                    word.push(chars.next().unwrap());
                } else if next == ':' {
                    // This is a field filter - skip the whole thing
                    chars.next(); // consume ':'
                    while let Some(&n) = chars.peek() {
                        if n.is_whitespace() {
                            break;
                        }
                        chars.next();
                    }
                    word.clear();
                    break;
                } else {
                    break;
                }
            }
            // Ignore operators
            let upper = word.to_uppercase();
            if !word.is_empty() && upper != "AND" && upper != "OR" && upper != "NOT" {
                terms.push(word);
            }
        }
        // Skip whitespace and other characters
    }

    terms
}

fn render_block<T: AsRef<str>>(lines: &[T], wrap: WrapConfig) -> String {
    lines
        .iter()
        .map(|l| apply_wrap(l.as_ref(), wrap))
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_robot_help(wrap: WrapConfig) -> CliResult<()> {
    let lines = vec![
        "cass --robot-help (contract v1)",
        "===============================",
        "",
        "QUICKSTART (for AI agents):",
        "  cass search \"your query\" --robot     # Search with JSON output",
        "  cass search \"bug fix\" --today        # Search today's sessions only",
        "  cass search \"api\" --week --agent codex  # Last 7 days, codex only",
        "  cass stats --json                    # Get index statistics",
        "  cass view /path/file.jsonl -n 42    # View file at line 42",
        "  cass robot-docs commands            # Machine-readable command list",
        "  cass --robot-docs=commands          # Also accepted (auto-normalized)",
        "",
        "TIME FILTERS:",
        "  --today | --yesterday | --week | --days N",
        "  --since YYYY-MM-DD | --until YYYY-MM-DD",
        "",
        "WORKFLOW:",
        "  1. cass index --full          # First-time setup (index all sessions)",
        "  2. cass search \"query\" --robot  # Search with JSON output",
        "  3. cass view <source_path> -n <line>  # Follow up on search result",
        "",
        "OUTPUT:",
        "  --robot | --json   Machine-readable JSON output (auto-quiet enabled)",
        "  stdout=data only; stderr=warnings/errors only (INFO auto-suppressed)",
        "  Use -v/--verbose with --json to enable INFO logs if needed",
        "",
        "Subcommands: search | stats | view | index | tui | robot-docs <topic>",
        "Topics: commands | env | paths | schemas | guide | exit-codes | examples | contracts | wrap | sources",
        "Exit codes: 0 ok; 2 usage; 3 missing index/db; 9 unknown",
        "More: cass robot-docs examples | cass robot-docs commands",
    ];
    println!("{}", render_block(&lines, wrap));
    Ok(())
}

fn print_robot_docs(topic: RobotTopic, wrap: WrapConfig) -> CliResult<()> {
    let lines: Vec<String> = match topic {
        RobotTopic::Commands => vec![
            "commands:".to_string(),
            "  (global) --quiet / -q  Suppress info logs (auto-enabled in robot mode)".to_string(),
            "  (global) --verbose/-v  Enable debug logs (overrides auto-quiet)".to_string(),
            "  Tip: `--robot-docs=<topic>` is normalized to `robot-docs <topic>`; globals can appear before/after subcommands.".to_string(),
            "  cass search <query> [OPTIONS]".to_string(),
            "    --agent A         Filter by agent (codex, claude_code, gemini, vibe, opencode, amp, cline)".to_string(),
            "    --workspace W     Filter by workspace path".to_string(),
            "    --limit N         Max results (default: 10)".to_string(),
            "    --offset N        Pagination offset (default: 0)".to_string(),
            "    --json | --robot  JSON output for automation".to_string(),
            "    --fields F1,F2    Select specific fields in hits (reduces token usage)".to_string(),
            "                      Presets: minimal (path,line,agent), summary (+title,score), provenance (source_id,origin_kind,origin_host)".to_string(),
            "                      Fields: score,agent,workspace,source_path,snippet,content,title,created_at,line_number,match_type,source_id,origin_kind,origin_host".to_string(),
            "    --max-content-length N  Truncate content/snippet/title to N chars (UTF-8 safe, adds '...')".to_string(),
            "                            Adds *_truncated: true indicator for each truncated field".to_string(),
            "    --today           Filter to today only".to_string(),
            "    --yesterday       Filter to yesterday only".to_string(),
            "    --week            Filter to last 7 days".to_string(),
            "    --days N          Filter to last N days".to_string(),
            "    --since DATE      Filter from date (YYYY-MM-DD)".to_string(),
            "    --until DATE      Filter to date (YYYY-MM-DD)".to_string(),
            "    --aggregate F1,F2 Server-side aggregation by fields (agent,workspace,date,match_type)".to_string(),
            "                      Returns buckets with counts. Reduces tokens by ~99% for overview queries".to_string(),
            "  cass stats [--json] [--data-dir DIR]".to_string(),
            "  cass status [--json] [--stale-threshold N] [--data-dir DIR]".to_string(),
            "  cass diag [--json] [--verbose] [--data-dir DIR]".to_string(),
            "  cass view <path> [-n LINE] [-C CONTEXT] [--json]".to_string(),
            "  cass index [--full] [--watch] [--json] [--data-dir DIR]".to_string(),
            "  cass tui [--once] [--data-dir DIR] [--reset-state] [--asciicast FILE]"
                .to_string(),
            "  cass capabilities [--json]".to_string(),
            "  cass robot-docs <topic>".to_string(),
            "  cass --robot-help".to_string(),
        ],
        RobotTopic::Env => vec![
            "env:".to_string(),
            "  CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1   skip update prompt".to_string(),
            "  TUI_HEADLESS=1                           skip update prompt".to_string(),
            "  CASS_DATA_DIR                            override data dir".to_string(),
            "  CASS_DB_PATH                             override db path".to_string(),
            "  CASS_OUTPUT_FORMAT=json|jsonl|compact|sessions|toon  default structured output".to_string(),
            "  TOON_DEFAULT_FORMAT=toon|json            fallback structured output for all tools".to_string(),
            "  TOON_INDENT=<N>                           pretty-print TOON with indent".to_string(),
            "  TOON_KEY_FOLDING=off|safe                 TOON key folding mode".to_string(),
            "  CASS_NO_COLOR                            force monochrome".to_string(),
            "  CASS_RESPECT_NO_COLOR=1                  honor global NO_COLOR".to_string(),
            "  CASS_TRACE_FILE                          default trace path".to_string(),
        ],
        RobotTopic::Paths => {
            let mut lines: Vec<String> = vec!["paths:".to_string()];
            lines.push(format!("  data dir default: {}", default_data_dir().display()));
            lines.push(format!("  db path default: {}", default_db_path().display()));
            lines.push("  log path: <data-dir>/cass.log (daily rolling)".to_string());
            lines.push("  trace: user-provided path (JSONL).".to_string());
            lines
        }
        RobotTopic::Guide => vec![
            "guide:".to_string(),
            "  Robot-mode handbook: docs/ROBOT_MODE.md (automation quickstart)".to_string(),
            "  Output: --robot/--json; formats via --robot-format json|jsonl|compact|toon".to_string(),
            "  Logging: INFO auto-suppressed in robot mode; add -v to re-enable".to_string(),
            "  Args: accepts --robot-docs=topic and misplaced globals; detailed errors with examples on parse failure".to_string(),
            "  Safety: prefer --color=never in non-TTY; use --trace-file for spans; reset TUI via `cass tui --reset-state`".to_string(),
            "  Quick refs: cass --robot-help | cass robot-docs commands | cass robot-docs examples".to_string(),
        ],
        RobotTopic::Schemas => render_schema_docs(),
        RobotTopic::ExitCodes => vec![
            "exit-codes:".to_string(),
            " 0 ok | 2 usage | 3 missing index/db | 4 network | 5 data-corrupt | 6 incompatible-version | 7 lock/busy | 8 partial | 9 unknown".to_string(),
        ],
        RobotTopic::Examples => vec![
            "examples:".to_string(),
            String::new(),
            "# Basic search with JSON output for agents".to_string(),
            "  cass search \"your query\" --robot".to_string(),
            "# Token-budgeted search with cursor + request-id".to_string(),
            "  cass search \"error\" --robot --max-tokens 200 --request-id run-1 --limit 2 --robot-meta".to_string(),
            "  cass search \"error\" --robot --cursor <_meta.next_cursor> --request-id run-1b --robot-meta".to_string(),
            String::new(),
            "# Search with time filters".to_string(),
            "  cass search \"bug\" --today                 # today only".to_string(),
            "  cass search \"api\" --week                  # last 7 days".to_string(),
            "  cass search \"feature\" --days 30           # last 30 days".to_string(),
            "  cass search \"fix\" --since 2025-01-01      # since date".to_string(),
            "  cass search \"error\" --robot --limit 5 --offset 5  # paginate robot output".to_string(),
            String::new(),
            "# Filter by agent or workspace".to_string(),
            "  cass search \"error\" --agent codex         # codex sessions only".to_string(),
            "  cass search \"test\" --workspace /myproject # specific project".to_string(),
            String::new(),
            "# Follow up on search results".to_string(),
            "  cass view /path/to/session.jsonl -n 42   # view line 42 with context".to_string(),
            "  cass view /path/to/session.jsonl -n 42 -C 10  # 10 lines context".to_string(),
            String::new(),
            "# Get index statistics".to_string(),
            "  cass stats --json                        # JSON stats".to_string(),
            "  cass stats                               # Human-readable stats".to_string(),
            String::new(),
            "# Aggregation (overview queries - 99% token reduction)".to_string(),
            "  cass search \"error\" --json --aggregate agent    # count by agent".to_string(),
            "  cass search \"*\" --json --aggregate agent,workspace  # multi-field agg".to_string(),
            "  cass search \"bug\" --json --aggregate date --week  # time distribution".to_string(),
            String::new(),
            "# Quick health check (ideal for agents)".to_string(),
            "  cass status --json                       # health check JSON".to_string(),
            "  cass status --stale-threshold 3600       # custom stale threshold (1hr)".to_string(),
            String::new(),
            "# Diagnostics".to_string(),
            "  cass diag --json                         # JSON diagnostic info".to_string(),
            "  cass diag --verbose                      # Human-readable with sizes".to_string(),
            String::new(),
            "# Capabilities introspection (for agent self-configuration)".to_string(),
            "  cass capabilities --json                 # JSON with version, features, limits".to_string(),
            "  cass capabilities                        # Human-readable summary".to_string(),
            String::new(),
            "# Full workflow".to_string(),
            "  cass index --full                        # index all sessions".to_string(),
            "  cass search \"cma-es\" --robot             # search".to_string(),
            "  cass view <source_path> -n <line>        # examine result".to_string(),
        ],
        RobotTopic::Contracts => vec![
            "contracts:".to_string(),
            "  stdout data-only; stderr diagnostics/progress.".to_string(),
            "  No implicit TUI when automation flags set or stdout non-TTY.".to_string(),
            "  Color auto off when non-TTY unless forced.".to_string(),
            "  Use --quiet to silence info logs in robot runs.".to_string(),
            "  JSON errors only to stderr.".to_string(),
        ],
        RobotTopic::Wrap => vec![
            "wrap:".to_string(),
            "  Default: no forced wrap (wide output).".to_string(),
            "  --wrap <n>: wrap informational text to n columns.".to_string(),
            "  --nowrap: force no wrapping even if wrap set elsewhere.".to_string(),
        ],
        RobotTopic::Sources => vec![
            "sources:".to_string(),
            String::new(),
            "# cass sources setup - Interactive Remote Sources Wizard".to_string(),
            String::new(),
            "## Overview".to_string(),
            "The setup wizard automates configuring cass to search across multiple machines.".to_string(),
            "It discovers SSH hosts from ~/.ssh/config, checks their status, and handles".to_string(),
            "installation, indexing, and configuration automatically.".to_string(),
            String::new(),
            "## Quick Start".to_string(),
            "  cass sources setup                    # Interactive (recommended)".to_string(),
            "  cass sources setup --hosts css,csd    # Configure specific hosts".to_string(),
            "  cass sources setup --dry-run          # Preview without changes".to_string(),
            "  cass sources setup --resume           # Resume interrupted setup".to_string(),
            String::new(),
            "## Workflow Phases".to_string(),
            "  1. Discovery  - Parses ~/.ssh/config to find configured hosts".to_string(),
            "  2. Probing    - Connects via SSH to check cass status and agent data".to_string(),
            "  3. Selection  - Interactive selection of which hosts to configure".to_string(),
            "  4. Install    - Installs cass on hosts without it (optional)".to_string(),
            "  5. Indexing   - Runs `cass index` on remotes (optional)".to_string(),
            "  6. Config     - Generates sources.toml entries".to_string(),
            "  7. Sync       - Downloads session data to local machine (optional)".to_string(),
            String::new(),
            "## Flags Reference".to_string(),
            "  --hosts <names>      Only configure these hosts (comma-separated SSH aliases)".to_string(),
            "  --dry-run            Preview without making changes".to_string(),
            "  --resume             Resume from ~/.config/cass/setup_state.json".to_string(),
            "  --non-interactive    Skip prompts, use auto-detected defaults".to_string(),
            "  --skip-install       Don't install cass on remotes".to_string(),
            "  --skip-index         Don't run remote indexing".to_string(),
            "  --skip-sync          Don't sync after setup".to_string(),
            "  --json               Output progress as JSON for scripting".to_string(),
            "  --timeout <secs>     SSH connection timeout (default: 10)".to_string(),
            "  --verbose            Show detailed progress".to_string(),
            String::new(),
            "## Non-Interactive Usage (Scripting)".to_string(),
            "  cass sources setup --non-interactive --hosts css,csd".to_string(),
            "  cass sources setup --non-interactive --hosts css --skip-install --skip-index".to_string(),
            "  cass sources setup --json --hosts css  # JSON output for parsing".to_string(),
            String::new(),
            "## State and Resume".to_string(),
            "State saved to ~/.config/cass/setup_state.json on interruption.".to_string(),
            "Resume with: cass sources setup --resume".to_string(),
            String::new(),
            "## Generated Configuration".to_string(),
            "The wizard generates sources.toml entries like:".to_string(),
            "  [[sources]]".to_string(),
            "  name = \"css\"".to_string(),
            "  type = \"ssh\"".to_string(),
            "  host = \"css\"".to_string(),
            "  paths = [\"~/.claude/projects\", \"~/.codex/sessions\"]".to_string(),
            "  sync_schedule = \"manual\"".to_string(),
            "  [[sources.path_mappings]]".to_string(),
            "  from = \"/data/projects\"".to_string(),
            "  to = \"/Users/username/projects\"".to_string(),
            String::new(),
            "## After Setup".to_string(),
            "  cass search \"query\"       # Search across all sources".to_string(),
            "  cass sources sync --all   # Sync latest data".to_string(),
            "  cass sources list         # List configured sources".to_string(),
            String::new(),
            "## Troubleshooting".to_string(),
            "  \"Host unreachable\": Verify SSH config with `ssh <host>` manually".to_string(),
            "  \"Permission denied\": Load SSH key with `ssh-add ~/.ssh/id_rsa`".to_string(),
            "  \"cargo not found\": Use --skip-install and install manually".to_string(),
            "  \"Index taking too long\": Large histories take time; runs in background".to_string(),
            String::new(),
            "## Related Commands".to_string(),
            "  cass sources list         List configured sources".to_string(),
            "  cass sources sync         Sync data from sources".to_string(),
            "  cass sources discover     Just discover hosts (no setup)".to_string(),
            "  cass sources add          Manually add a source".to_string(),
        ],
        RobotTopic::Analytics => render_analytics_docs(),
    };

    println!("{}", render_block(&lines, wrap));
    Ok(())
}

/// Render comprehensive analytics robot-docs.
fn render_analytics_docs() -> Vec<String> {
    vec![
        "analytics:".into(),
        String::new(),
        "# cass analytics — Token, Cost, Tool, and Model Analytics".into(),
        String::new(),
        "## Subcommands".into(),
        "  status    Row counts, freshness, coverage, drift warnings".into(),
        "  tokens    Token usage over time with dimensional breakdowns".into(),
        "  tools     Per-tool invocation counts and derived metrics".into(),
        "  models    Top models by usage and coverage statistics".into(),
        "  cost      USD cost estimates with pricing coverage".into(),
        "  rebuild   Rebuild/backfill rollup tables with progress output".into(),
        "  validate  Check rollup invariants and detect data drift".into(),
        String::new(),
        "## Shared Flags (all subcommands)".into(),
        "  --since <ISO>        Filter from date (YYYY-MM-DD or full timestamp)".into(),
        "  --until <ISO>        Filter to date".into(),
        "  --days <N>           Filter to last N days".into(),
        "  --agent <slug>       Filter by agent (repeatable)".into(),
        "  --workspace <path>   Filter by workspace (repeatable)".into(),
        "  --source <name>      Filter by source ('local', 'remote', hostname)".into(),
        "  --json / --robot     Machine-readable JSON output".into(),
        "  --data-dir <path>    Override data directory".into(),
        String::new(),
        "## Bucketed Subcommands (tokens, tools, models, cost)".into(),
        "  --group-by <bucket>  hour | day (default) | week | month".into(),
        String::new(),
        "## JSON Envelope (all subcommands)".into(),
        "  { \"command\": \"analytics/<sub>\", \"data\": {...}, \"_meta\": {...} }".into(),
        "  _meta: { elapsed_ms: u64, filters_applied: [string], data_dir: string|null }".into(),
        String::new(),
        "## Per-Subcommand JSON Schemas".into(),
        String::new(),
        "### analytics status".into(),
        "  data.tables: [{ table, exists, row_count, min_day_id, max_day_id, last_updated }]".into(),
        "  data.coverage: { total_messages, message_metrics_coverage_pct, api_token_coverage_pct,".into(),
        "                   model_name_coverage_pct, estimate_only_pct, pricing_coverage_pct }".into(),
        "  data.drift: { signals: [{ signal, detail, severity }], track_a_fresh, track_b_fresh }".into(),
        "  data.recommended_action: string".into(),
        String::new(),
        "### analytics tokens".into(),
        "  data.buckets: [{ bucket: string, counts: {...}, content_tokens: {...},".into(),
        "                   api_tokens: {...}, cost: {...}, plan: {...}, derived: {...} }]".into(),
        "  data.totals: <same shape as bucket>".into(),
        "  data.source_table: string  ('usage_daily' | 'usage_hourly')".into(),
        "  data.granularity: string   ('hour' | 'day' | 'week' | 'month')".into(),
        "  Bucket keys: counts.{message_count, user_message_count, assistant_message_count,".into(),
        "    tool_call_count, plan_message_count}; api_tokens.{total, input, output,".into(),
        "    cache_read, cache_creation, thinking}; derived.{api_coverage_pct,".into(),
        "    avg_api_per_message, avg_content_per_message, cost_per_1k_api, cost_per_message}".into(),
        String::new(),
        "### analytics tools".into(),
        "  data.rows: [{ key: string, tool_call_count, message_count, api_tokens_total,".into(),
        "               tool_calls_per_1k_api_tokens, tool_calls_per_1k_content_tokens }]".into(),
        "  data.totals: { tool_call_count, message_count, api_tokens_total,".into(),
        "                 overall_per_1k_api_tokens }".into(),
        "  data.row_count: int".into(),
        "  --limit N (default 20): caps returned rows".into(),
        String::new(),
        "### analytics models".into(),
        "  data.by_api_tokens: { dim, metric, rows: [{ key, value, message_count, ... }] }".into(),
        "  data.by_cost: { dim, metric, rows: [{ key, value, estimated_cost_usd, ... }] }".into(),
        "  data.timeseries: <same as analytics tokens>".into(),
        "  Source: token_daily_stats (Track B) — models only available for connectors".into(),
        "    that report model names (claude_code, codex, pi_agent, factory, opencode, cursor).".into(),
        String::new(),
        "### analytics cost".into(),
        "  data.totals: { estimated_cost_usd, api_tokens_total, message_count,".into(),
        "                 cost_per_1k_api_tokens, cost_per_message,".into(),
        "                 period_delta_pct: f64|null, prior_period_cost_usd: f64|null }".into(),
        "  data.pricing_coverage: { pricing_coverage_pct, api_token_coverage_pct,".into(),
        "    model_name_coverage_pct, estimate_only_pct,".into(),
        "    unpriced_token_share_pct: f64|null, unpriced_models: [{ model_name, total_tokens, row_count }] }".into(),
        "  data.buckets: [{ bucket, estimated_cost_usd, api_tokens_total, message_count }]".into(),
        "  data.by_model: { dim, metric, rows: [...] }".into(),
        "  IMPORTANT: estimated_cost_usd = 0.0 when pricing_coverage_pct is 0%.".into(),
        "  Never treat cost=0 as 'free'; check pricing_coverage first.".into(),
        String::new(),
        "### analytics rebuild".into(),
        "  data.track: string ('a')".into(),
        "  data.tracks_rebuilt: [string]".into(),
        "  data.track_a: { message_metrics_rows, usage_hourly_rows, usage_daily_rows,".into(),
        "                  usage_models_daily_rows, elapsed_ms, rows_per_sec }".into(),
        "  data.overall_elapsed_ms: u64".into(),
        "  --force: rebuild even when rollups appear fresh".into(),
        String::new(),
        "### analytics validate".into(),
        "  data.summary: { errors, warnings, drift_entries, buckets_checked, buckets_total }".into(),
        "  data.checks: [{ id, ok, severity, details, suggested_action? }]".into(),
        "    Check IDs: track_a.tables_exist, track_a.{content_tokens,message_count,".into(),
        "      api_tokens,api_coverage}_match, track_b.tables_exist,".into(),
        "      track_b.{tokens,agents}_match, cross_track.drift, non_negative.counters".into(),
        "  data.drift: [{ day_id, agent_slug, source_id, track_a_total,".into(),
        "                 track_b_total, delta, delta_pct, likely_cause }]".into(),
        "  data.perf: { timeseries: { elapsed_ms, budget_ms, within_budget },".into(),
        "              breakdown: { elapsed_ms, budget_ms, within_budget } }".into(),
        "  --fix: attempt automatic repair (not yet implemented)".into(),
        String::new(),
        "## Coverage & Uncertainty Semantics".into(),
        "  - api_token_coverage_pct: % of messages with API token data (from Claude, Codex).".into(),
        "  - pricing_coverage_pct: % of token_usage rows with cost > 0 (requires model_pricing match).".into(),
        "  - estimate_only_pct: % of messages with content-estimated tokens only (chars/4 heuristic).".into(),
        "  - When coverage is low, derived metrics are unreliable estimates, not ground truth.".into(),
        "  - Content token estimates are always available (heuristic); API tokens are sparse.".into(),
        String::new(),
        "## Exit Codes".into(),
        "  0  Success".into(),
        "  2  Usage error (invalid flags, missing required args)".into(),
        "  3  Missing database (run 'cass index --full' first)".into(),
        "  9  Database error (corrupt, missing tables, query failure)".into(),
        String::new(),
        "## Retry Guidance".into(),
        "  exit 9 + retryable=true: transient DB lock/busy — retry after 1s".into(),
        "  exit 9 + retryable=false: schema or data issue — run 'cass analytics rebuild' first".into(),
        "  exit 3: no database — run 'cass index --full' to create it".into(),
        "  validate errors: run 'cass analytics rebuild --force' then re-validate".into(),
        String::new(),
        "## Common Workflows".into(),
        "  # Quick health check".into(),
        "  cass analytics status --json | jq '.data.coverage'".into(),
        String::new(),
        "  # Recent usage by agent for last 7 days".into(),
        "  cass analytics tokens --days 7 --json | jq '.data.buckets'".into(),
        String::new(),
        "  # Cost analysis by model".into(),
        "  cass analytics cost --json | jq '.data.by_model.rows[:5]'".into(),
        String::new(),
        "  # Tool usage top-10".into(),
        "  cass analytics tools --limit 10 --json | jq '.data.rows'".into(),
        String::new(),
        "  # Validation + remediation loop".into(),
        "  cass analytics validate --json | jq '.data.summary'".into(),
        "  # If errors: rebuild then re-validate".into(),
        "  cass analytics rebuild --force --json && cass analytics validate --json".into(),
    ]
}

/// Render schema docs from live response schemas
fn render_schema_docs() -> Vec<String> {
    use serde_json::{Map, Value};

    fn type_of(v: &Value) -> String {
        v.get("type")
            .and_then(Value::as_str)
            .map_or_else(|| "?".to_string(), str::to_string)
    }

    fn render_props(
        lines: &mut Vec<String>,
        props: &Map<String, Value>,
        indent: usize,
        depth: usize,
    ) {
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        for k in keys {
            let v = &props[k];
            let ty = type_of(v);
            let pad = "  ".repeat(indent);
            lines.push(format!("{pad}- {k}: {ty}"));
            if depth < 2
                && let Some(obj) = v.get("properties").and_then(Value::as_object)
            {
                render_props(lines, obj, indent + 1, depth + 1);
            }
        }
    }

    let mut lines = vec!["schemas: (auto-generated from contract)".to_string()];
    let mut schemas: Vec<(String, Value)> = build_response_schemas().into_iter().collect();
    schemas.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, schema) in schemas {
        lines.push(format!("  {name}:"));
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            render_props(&mut lines, props, 2, 0);
        } else {
            lines.push("    (no properties)".to_string());
        }
    }

    lines
}

/// Extract request_id from CLI command if present (currently only Search has it)
fn extract_request_id(cli: &Cli) -> Option<String> {
    match &cli.command {
        Some(Commands::Search { request_id, .. }) => request_id.clone(),
        _ => None,
    }
}

fn write_trace_line(
    path: &PathBuf,
    label: &str,
    cli: &Cli,
    start_ts: &chrono::DateTime<Utc>,
    duration_ms: u128,
    exit_code: i32,
    error: Option<&CliError>,
) -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let request_id = extract_request_id(cli);
    let trace_id = dotenvy::var("CASS_TRACE_ID").ok();
    let payload = serde_json::json!({
        "start_ts": start_ts.to_rfc3339(),
        "end_ts": (*start_ts
            + chrono::Duration::from_std(Duration::from_millis(duration_ms as u64)).unwrap_or_default())
        .to_rfc3339(),
        "duration_ms": duration_ms,
        "cmd": label,
        "args": args,
        "exit_code": exit_code,
        "error": error.map(|e| serde_json::json!({
            "code": e.code,
            "kind": e.kind,
            "message": e.message,
            "hint": e.hint,
            "retryable": e.retryable,
        })),
        "request_id": request_id,
        "trace_id": trace_id,
        "contract_version": CONTRACT_VERSION,
        "crate_version": env!("CARGO_PKG_VERSION"),
    });

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{payload}")?;
    Ok(())
}

/// Time filter helper for search commands
#[derive(Debug, Clone, Default)]
pub struct TimeFilter {
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// Semantic search options from CLI flags (bd-3bbv)
///
/// These options control model selection, reranking, and daemon usage
/// for semantic search operations.
#[derive(Debug, Clone, Default)]
pub struct SemanticSearchOptions {
    /// Embedding model to use (overrides config/default)
    pub model: Option<String>,
    /// Enable reranking of results
    pub rerank: bool,
    /// Reranker model to use (if rerank is enabled)
    pub reranker: Option<String>,
    /// Use daemon for warm model inference
    pub use_daemon: bool,
    /// Use approximate nearest neighbor search when available
    pub approximate: bool,
}

impl TimeFilter {
    pub fn new(
        days: Option<u32>,
        today: bool,
        yesterday: bool,
        week: bool,
        since_str: Option<&str>,
        until_str: Option<&str>,
    ) -> Self {
        use chrono::{Datelike, Duration, Local, TimeZone};

        let now = Local::now();
        let today_start = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
            .single()
            .unwrap_or(now);

        let (since, until) = if today {
            (Some(today_start.timestamp_millis()), None)
        } else if yesterday {
            let yesterday_start = today_start - Duration::days(1);
            (
                Some(yesterday_start.timestamp_millis()),
                Some(today_start.timestamp_millis()),
            )
        } else if week {
            let week_ago = now - Duration::days(7);
            (Some(week_ago.timestamp_millis()), None)
        } else if let Some(d) = days {
            let days_ago = now - Duration::days(i64::from(d));
            (Some(days_ago.timestamp_millis()), None)
        } else {
            (None, None)
        };

        // Explicit --since/--until override convenience flags when they parse successfully
        let since = since_str.and_then(parse_datetime_str).or(since);
        let until = until_str.and_then(parse_datetime_str).or(until);

        TimeFilter { since, until }
    }
}

fn parse_datetime_str(s: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};

    fn local_from_naive(dt: NaiveDateTime) -> i64 {
        match Local.from_local_datetime(&dt) {
            chrono::LocalResult::Single(local) => local.timestamp_millis(),
            chrono::LocalResult::Ambiguous(local, _) => local.timestamp_millis(),
            chrono::LocalResult::None => Local.from_utc_datetime(&dt).timestamp_millis(),
        }
    }

    fn local_midnight_ts(date: NaiveDate) -> Option<i64> {
        let dt = date.and_hms_opt(0, 0, 0)?;
        Some(local_from_naive(dt))
    }

    // Try full datetime first: YYYY-MM-DDTHH:MM:SS
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(local_from_naive(dt));
    }

    // Try date only: YYYY-MM-DD
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local_midnight_ts(date);
    }

    None
}

/// Compute aggregations from search hits
fn compute_aggregations(
    hits: &[crate::search::query::SearchHit],
    fields: &[AggregateField],
) -> Aggregations {
    use std::collections::HashMap;

    const MAX_BUCKETS: usize = 10;
    let mut aggregations = Aggregations::default();

    for field in fields {
        let mut counts: HashMap<String, u64> = HashMap::new();

        // Count occurrences based on field type
        for hit in hits {
            let key = match field {
                AggregateField::Agent => hit.agent.clone(),
                AggregateField::Workspace => hit.workspace.clone(),
                AggregateField::Date => {
                    // Group by date (YYYY-MM-DD)
                    hit.created_at
                        .and_then(|ts| {
                            chrono::DateTime::from_timestamp_millis(ts)
                                .map(|d| d.format("%Y-%m-%d").to_string())
                        })
                        .unwrap_or_else(|| "unknown".to_string())
                }
                AggregateField::MatchType => format!("{:?}", hit.match_type).to_lowercase(),
            };
            *counts.entry(key).or_insert(0) += 1;
        }

        // Sort by count descending, take top N
        let mut sorted: Vec<_> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let total_count: u64 = sorted.iter().map(|(_, c)| *c).sum();
        let top_buckets: Vec<AggregationBucket> = sorted
            .iter()
            .take(MAX_BUCKETS)
            .map(|(key, count)| AggregationBucket {
                key: key.clone(),
                count: *count,
            })
            .collect();
        let top_sum: u64 = top_buckets.iter().map(|b| b.count).sum();
        let other_count = total_count.saturating_sub(top_sum);

        let agg = FieldAggregation {
            buckets: top_buckets,
            other_count,
        };

        match field {
            AggregateField::Agent => aggregations.agent = Some(agg),
            AggregateField::Workspace => aggregations.workspace = Some(agg),
            AggregateField::Date => aggregations.date = Some(agg),
            AggregateField::MatchType => aggregations.match_type = Some(agg),
        }
    }

    aggregations
}

/// Parse aggregate field strings into enum values, warning on unknown fields
fn parse_aggregate_fields(fields: &[String]) -> Vec<AggregateField> {
    fields
        .iter()
        .filter_map(|f| {
            let parsed = AggregateField::from_str(f);
            if parsed.is_none() {
                warn!(field = %f, "Unknown aggregate field, ignoring. Valid: agent, workspace, date, match_type");
            }
            parsed
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_cli_search(
    query: &str,
    agents: &[String],
    workspaces: &[String],
    limit: &usize,
    offset: &usize,
    json: &bool,
    robot_format: Option<RobotFormat>,
    robot_meta: bool,
    fields: Option<Vec<String>>,
    max_content_length: Option<usize>,
    max_tokens: Option<usize>,
    request_id: Option<String>,
    cursor: Option<String>,
    display_format: Option<DisplayFormat>,
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    wrap: WrapConfig,
    _progress: ProgressResolved,
    robot_auto: bool,
    time_filter: TimeFilter,
    aggregate: Option<Vec<String>>,
    explain: bool,
    dry_run: bool,
    timeout_ms: Option<u64>,
    highlight: bool,
    source: Option<String>,
    sessions_from: Option<String>,
    mode: Option<crate::search::query::SearchMode>,
    semantic_opts: SemanticSearchOptions,
) -> CliResult<()> {
    use crate::search::ann_index::hnsw_index_path;
    use crate::search::model_manager::{load_hash_semantic_context, load_semantic_context};
    use crate::search::query::{
        QueryExplanation, SearchClient, SearchClientOptions, SearchFilters, SearchMode,
    };
    use crate::search::tantivy::index_dir;
    use crate::sources::provenance::SourceFilter;
    use std::collections::HashSet;
    use std::sync::Arc;

    // Start timing for robot_meta elapsed_ms
    let start_time = Instant::now();

    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let index_path = index_dir(&data_dir).map_err(|e| CliError {
        code: 9,
        kind: "path",
        message: format!("failed to open index dir: {e}"),
        hint: None,
        retryable: false,
    })?;
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));

    let client = SearchClient::open_with_options(
        &index_path,
        Some(&db_path),
        SearchClientOptions {
            enable_reload: false,
            enable_warm: false,
        },
    )
    .map_err(|e| CliError {
        code: 9,
        kind: "open-index",
        message: format!("failed to open index: {e}"),
        hint: Some("try cass index --full".to_string()),
        retryable: true,
    })?
    .ok_or_else(|| CliError {
        code: 3,
        kind: "missing-index",
        message: format!(
            "Index not found at {}. Run 'cass index --full' first.",
            index_path.display()
        ),
        hint: None,
        retryable: true,
    })?;

    // Determine effective search mode (default to Lexical)
    let effective_mode = mode.unwrap_or(SearchMode::Lexical);
    let approximate = if semantic_opts.approximate && matches!(effective_mode, SearchMode::Lexical)
    {
        eprintln!("Warning: --approximate has no effect in lexical mode.");
        false
    } else {
        semantic_opts.approximate
    };

    if matches!(effective_mode, SearchMode::Semantic | SearchMode::Hybrid) {
        use crate::search::embedder_registry::{EmbedderRegistry, HASH_EMBEDDER};

        // Use embedder registry for model selection (bd-2mbe)
        let registry = EmbedderRegistry::new(&data_dir);
        let requested_model = semantic_opts.model.as_deref();

        // Validate requested model if specified
        if let Some(model_name) = requested_model
            && let Err(e) = registry.validate(model_name)
        {
            return Err(CliError {
                code: 15,
                kind: "embedder-unavailable",
                message: format!("Embedder validation failed: {e}"),
                hint: Some("Run 'cass models list' to see available embedders".to_string()),
                retryable: false,
            });
        }

        // Determine which embedder to use
        let embedder_info = match requested_model {
            Some(name) => registry.get(name),
            None => Some(registry.best_available()),
        };
        let prefer_hash = embedder_info.is_some_and(|e| e.name == HASH_EMBEDDER);

        let setup = if prefer_hash {
            load_hash_semantic_context(&data_dir, &db_path)
        } else {
            load_semantic_context(&data_dir, &db_path)
        };

        if let Some(context) = setup.context {
            let embedder = context.embedder;
            let index = context.index;
            let filter_maps = context.filter_maps;
            let roles = context.roles;

            let embedder: Arc<dyn crate::search::embedder::Embedder> = if semantic_opts.use_daemon {
                use crate::search::daemon_client::{
                    DaemonFallbackEmbedder, DaemonRetryConfig, NoopDaemonClient,
                };

                let daemon = Arc::new(NoopDaemonClient::new("daemon-unconfigured"));
                let config = DaemonRetryConfig::from_env();
                Arc::new(DaemonFallbackEmbedder::new(daemon, embedder, config))
            } else {
                embedder
            };

            let ann_path = Some(hnsw_index_path(&data_dir, embedder.id()));
            if let Err(err) =
                client.set_semantic_context(embedder, index, filter_maps, roles, ann_path)
            {
                let hint = if prefer_hash {
                    "Run 'cass index --semantic --embedder hash' to rebuild the hash vector index, or use --mode lexical"
                        .to_string()
                } else {
                    "Run 'cass models install' and then 'cass index --semantic', or use --mode lexical"
                        .to_string()
                };
                return Err(CliError {
                    code: 15,
                    kind: "semantic-unavailable",
                    message: format!("Semantic search not available: {err}"),
                    hint: Some(hint),
                    retryable: false,
                });
            }
        } else {
            let _ = client.clear_semantic_context();
            let summary = setup.availability.summary();
            let hint = if prefer_hash {
                "Run 'cass index --semantic --embedder hash' to build the hash vector index, or use --mode lexical"
                    .to_string()
            } else {
                "Run 'cass models install' and then 'cass index --semantic', or use --mode lexical"
                    .to_string()
            };
            return Err(CliError {
                code: 15,
                kind: "semantic-unavailable",
                message: format!("Semantic search not available: {summary}"),
                hint: Some(hint),
                retryable: false,
            });
        }
    }

    let mut filters = SearchFilters::default();
    if !agents.is_empty() {
        filters.agents = HashSet::from_iter(agents.iter().cloned());
    }
    if !workspaces.is_empty() {
        filters.workspaces = HashSet::from_iter(workspaces.iter().cloned());
    }
    filters.created_from = time_filter.since;
    filters.created_to = time_filter.until;

    // Apply source filter (P3.1)
    if let Some(ref source_str) = source {
        filters.source_filter = SourceFilter::parse(source_str);
    }

    // Apply session paths filter (for chained searches)
    if let Some(ref sessions_from_arg) = sessions_from {
        let session_paths = read_session_paths(sessions_from_arg).map_err(|e| CliError {
            code: 2,
            kind: "sessions-from",
            message: format!("failed to read session paths: {e}"),
            hint: Some("Provide a file path or '-' for stdin".to_string()),
            retryable: false,
        })?;
        filters.session_paths = session_paths;
    }

    // Apply cursor overrides (base64-encoded JSON { "offset": usize, "limit": usize })
    let mut limit_val = *limit;
    let mut offset_val = *offset;
    if let Some(ref cursor_str) = cursor {
        let decoded = BASE64_STANDARD.decode(cursor_str).map_err(|e| CliError {
            code: 2,
            kind: "cursor-decode",
            message: format!("invalid cursor: {e}"),
            hint: Some("Pass cursor returned in previous _meta.next_cursor".to_string()),
            retryable: false,
        })?;
        let cursor_json: serde_json::Value =
            serde_json::from_slice(&decoded).map_err(|e| CliError {
                code: 2,
                kind: "cursor-parse",
                message: format!("invalid cursor payload: {e}"),
                hint: Some("Cursor should be base64 of {\"offset\":N,\"limit\":M}".to_string()),
                retryable: false,
            })?;
        if let Some(o) = cursor_json
            .get("offset")
            .and_then(serde_json::Value::as_u64)
        {
            offset_val = o as usize;
        }
        if let Some(l) = cursor_json.get("limit").and_then(serde_json::Value::as_u64) {
            limit_val = l as usize;
        }
    }

    // Determine the effective output format
    // Priority: robot_format CLI > json flag > CASS_OUTPUT_FORMAT > TOON_DEFAULT_FORMAT > robot_auto > None
    let effective_robot = robot_format
        .or(if *json { Some(RobotFormat::Json) } else { None })
        .or_else(robot_format_from_env)
        .or(if robot_auto {
            Some(RobotFormat::Json)
        } else {
            None
        });
    let field_mask = resolve_field_mask(&fields, effective_robot, display_format);

    // Parse aggregate fields if provided
    let agg_fields = aggregate
        .as_ref()
        .map(|f| parse_aggregate_fields(f))
        .unwrap_or_default();
    let has_aggregation = !agg_fields.is_empty();

    // Handle dry-run mode: validate and analyze query without executing
    if dry_run {
        let explanation = QueryExplanation::analyze(query, &filters);
        let elapsed_ms = start_time.elapsed().as_millis();

        let output = serde_json::json!({
            "dry_run": true,
            "valid": explanation.warnings.iter().all(|w| !w.contains("error") && !w.contains("invalid")),
            "query": query,
            "explanation": explanation,
            "estimated_cost": format!("{:?}", explanation.estimated_cost),
            "warnings": explanation.warnings,
            "request_id": request_id,
            "_meta": {
                "elapsed_ms": elapsed_ms,
                "dry_run": true,
            }
        });

        println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())
        );
        return Ok(());
    }

    // Use search_with_fallback to get full metadata (wildcard_fallback, cache_stats)
    let sparse_threshold = 3; // Threshold for triggering wildcard fallback

    // When aggregating, we need more results for accurate counts
    // Fetch up to 1000 for aggregation starting at offset 0, then apply offset/limit
    let (search_limit, search_offset) = if has_aggregation {
        (1000.max(limit_val + offset_val), 0)
    } else {
        (limit_val, offset_val)
    };

    // Check if we're already past timeout before starting search
    let timeout_duration = timeout_ms.map(Duration::from_millis);
    if let Some(timeout) = timeout_duration
        && start_time.elapsed() >= timeout
    {
        return Err(CliError {
            code: 10,
            kind: "timeout",
            message: format!(
                "Operation timed out after {}ms (before search started)",
                timeout.as_millis()
            ),
            hint: Some("Increase --timeout value or simplify query".to_string()),
            retryable: true,
        });
    }

    // Log semantic options if any are set (bd-3bbv: flags are wired, infra pending)
    if semantic_opts.model.is_some()
        || semantic_opts.rerank
        || semantic_opts.reranker.is_some()
        || semantic_opts.use_daemon
        || semantic_opts.approximate
    {
        tracing::debug!(
            model = ?semantic_opts.model,
            rerank = semantic_opts.rerank,
            reranker = ?semantic_opts.reranker,
            use_daemon = semantic_opts.use_daemon,
            approximate = semantic_opts.approximate,
            "Semantic search options configured"
        );
    }

    // Track search timing breakdown (T7.4)
    let search_start = Instant::now();
    let result = match effective_mode {
        SearchMode::Lexical => client
            .search_with_fallback(
                query,
                filters.clone(),
                search_limit,
                search_offset,
                sparse_threshold,
                field_mask,
            )
            .map_err(|e| CliError {
                code: 9,
                kind: "search",
                message: format!("search failed: {e}"),
                hint: None,
                retryable: true,
            })?,
        SearchMode::Semantic => {
            let (hits, ann_stats) = client
                .search_semantic(
                    query,
                    filters.clone(),
                    search_limit,
                    search_offset,
                    field_mask,
                    approximate,
                )
                .map_err(|e| {
                    let err_str = e.to_string();
                    if err_str.contains("HNSW index") {
                        CliError {
                            code: 15,
                            kind: "semantic-unavailable",
                            message: "Approximate search unavailable (HNSW index missing)".to_string(),
                            hint: Some(
                                "Run 'cass index --semantic --build-hnsw' to build the ANN index, or omit --approximate"
                                    .to_string(),
                            ),
                            retryable: false,
                        }
                    } else if err_str.contains("unavailable") || err_str.contains("no embedder") {
                        CliError {
                            code: 15,
                            kind: "semantic-unavailable",
                            message: "Semantic search not available".to_string(),
                            hint: Some(
                                "Run 'cass tui' and press Alt+S to set up semantic search, or use --mode lexical"
                                    .to_string(),
                            ),
                            retryable: false,
                        }
                    } else {
                        CliError {
                            code: 9,
                            kind: "search",
                            message: format!("semantic search failed: {e}"),
                            hint: Some("Try --mode lexical as fallback".to_string()),
                            retryable: true,
                        }
                    }
                })?;
            crate::search::query::SearchResult {
                hits,
                wildcard_fallback: false,
                cache_stats: crate::search::query::CacheStats::default(),
                suggestions: Vec::new(),
                ann_stats,
            }
        }
        SearchMode::Hybrid => client
            .search_hybrid(
                query,
                query,
                filters.clone(),
                search_limit,
                search_offset,
                sparse_threshold,
                field_mask,
                approximate,
            )
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("unavailable") || err_str.contains("no embedder") {
                    CliError {
                        code: 15,
                        kind: "semantic-unavailable",
                        message: "Hybrid search not available (requires semantic search)".to_string(),
                        hint: Some(
                            "Run 'cass tui' and press Alt+S to set up semantic search, or use --mode lexical"
                                .to_string(),
                        ),
                        retryable: false,
                    }
                } else {
                    CliError {
                        code: 9,
                        kind: "search",
                        message: format!("hybrid search failed: {e}"),
                        hint: Some("Try --mode lexical as fallback".to_string()),
                        retryable: true,
                    }
                }
            })?,
    };
    let search_ms = search_start.elapsed().as_millis() as u64;

    // Apply reranking if enabled (bd-2t2d)
    let rerank_start = Instant::now();
    let result = if semantic_opts.rerank && !result.hits.is_empty() {
        use crate::search::daemon_client::{
            DaemonFallbackReranker, DaemonRetryConfig, NoopDaemonClient,
        };
        use crate::search::fastembed_reranker::FastEmbedReranker;
        use crate::search::reranker::Reranker;

        let model_dir = FastEmbedReranker::default_model_dir(&data_dir);
        let local_reranker: Option<Arc<dyn Reranker>> =
            match FastEmbedReranker::load_from_dir(&model_dir) {
                Ok(reranker) => Some(Arc::new(reranker)),
                Err(e) => {
                    if !semantic_opts.use_daemon {
                        tracing::debug!(error = %e, "Reranker not available, skipping rerank");
                    }
                    None
                }
            };

        let reranker: Option<Arc<dyn Reranker>> = if semantic_opts.use_daemon {
            let daemon = Arc::new(NoopDaemonClient::new("daemon-unconfigured"));
            let config = DaemonRetryConfig::from_env();
            Some(Arc::new(DaemonFallbackReranker::new(
                daemon,
                local_reranker,
                config,
            )))
        } else {
            local_reranker
        };

        if let Some(reranker) = reranker {
            // Extract content from hits for reranking (use snippet if content is empty)
            let docs: Vec<String> = result
                .hits
                .iter()
                .map(|hit| {
                    if hit.content.is_empty() {
                        hit.snippet.clone()
                    } else {
                        hit.content.clone()
                    }
                })
                .collect();

            // Skip reranking if any document is empty (reranker rejects empty docs)
            let has_empty_doc = docs.iter().any(|d| d.is_empty());
            if has_empty_doc {
                tracing::debug!("Skipping rerank: one or more hits have empty content and snippet");
                result
            } else {
                let doc_refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();

                match reranker.rerank(query, &doc_refs) {
                    Ok(scores) => {
                        // Update scores and re-sort hits
                        let mut scored_hits: Vec<_> = result
                            .hits
                            .into_iter()
                            .zip(scores)
                            .map(|(mut hit, score)| {
                                hit.score = score;
                                hit
                            })
                            .collect();
                        scored_hits.sort_by(|a, b| {
                            b.score
                                .partial_cmp(&a.score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });

                        tracing::debug!(
                            reranker_id = reranker.id(),
                            hits_reranked = scored_hits.len(),
                            "Reranking complete"
                        );

                        crate::search::query::SearchResult {
                            hits: scored_hits,
                            wildcard_fallback: result.wildcard_fallback,
                            cache_stats: result.cache_stats,
                            suggestions: result.suggestions,
                            ann_stats: result.ann_stats,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Reranking failed, returning original results"
                        );
                        result
                    }
                }
            }
        } else {
            result
        }
    } else {
        result
    };
    // Track reranking time (0 if not applied) (T7.4)
    let rerank_ms = if semantic_opts.rerank {
        rerank_start.elapsed().as_millis() as u64
    } else {
        0
    };

    // Check if search exceeded timeout - return partial results with timeout indicator
    let timed_out = timeout_duration.is_some_and(|t| start_time.elapsed() > t);

    // Build query explanation if requested
    let explanation = if explain {
        Some(
            QueryExplanation::analyze(query, &filters)
                .with_wildcard_fallback(result.wildcard_fallback),
        )
    } else {
        None
    };

    // Compute aggregations and create display result based on mode
    let (aggregations, display_result, total_matches) = if has_aggregation {
        // Compute aggregations from all fetched results
        let aggs = compute_aggregations(&result.hits, &agg_fields);
        let total = result.hits.len();

        // Apply offset and limit to get display hits
        let display_hits: Vec<_> = result
            .hits
            .iter()
            .skip(offset_val)
            .take(limit_val)
            .cloned()
            .collect();

        let display = crate::search::query::SearchResult {
            hits: display_hits,
            wildcard_fallback: result.wildcard_fallback,
            cache_stats: result.cache_stats,
            suggestions: result.suggestions.clone(),
            ann_stats: result.ann_stats.clone(),
        };
        (aggs, display, total)
    } else {
        // No aggregation - use result as-is
        let total = result.hits.len();
        (Aggregations::default(), result, total)
    };

    let elapsed_ms = start_time.elapsed().as_millis() as u64;

    // Derive per-field budgets, preferring snippet > content > title
    let (snippet_budget, content_budget, title_budget, fallback_budget) = {
        let base = max_content_length;
        if let Some(tokens) = max_tokens {
            let char_budget = tokens.saturating_mul(4);
            let per_hit = char_budget / std::cmp::max(1, display_result.hits.len());
            let snippet = std::cmp::max(16, (per_hit as f64 * 0.5) as usize);
            let content = std::cmp::max(12, (per_hit as f64 * 0.35) as usize);
            let title = std::cmp::max(8, (per_hit as f64 * 0.15) as usize);
            (
                Some(snippet),
                Some(content),
                Some(title),
                base.map(|b| std::cmp::min(b, per_hit)),
            )
        } else {
            (base, base, base, base)
        }
    };

    let truncation_budgets = FieldBudgets {
        snippet: snippet_budget,
        content: content_budget,
        title: title_budget,
        fallback: fallback_budget,
    };

    // Build next cursor if more results remain
    let next_cursor = if total_matches > offset_val + display_result.hits.len() {
        let payload = serde_json::json!({
            "offset": offset_val + display_result.hits.len(),
            "limit": limit_val,
        })
        .to_string();
        Some(BASE64_STANDARD.encode(payload))
    } else {
        None
    };

    // Gather state meta for robot output (index/db freshness)
    let state_meta = if robot_meta {
        Some(state_meta_json(
            &data_dir,
            &db_path,
            DEFAULT_STALE_THRESHOLD_SECS,
            true,
        ))
    } else {
        None
    };
    let index_freshness = state_meta.as_ref().and_then(state_index_freshness);
    let warning = index_freshness
        .as_ref()
        .and_then(|f: &serde_json::Value| f.get("stale"))
        .and_then(|v: &serde_json::Value| v.as_bool())
        .filter(|stale| *stale)
        .map(|_| {
            let age = index_freshness
                .as_ref()
                .and_then(|f: &serde_json::Value| f.get("age_seconds"))
                .and_then(|v: &serde_json::Value| v.as_u64()).map_or_else(|| "an unknown age".to_string(), |s| format!("{s} seconds"));
            let pending = index_freshness
                .as_ref()
                .and_then(|f: &serde_json::Value| f.get("pending_sessions"))
                .and_then(|v: &serde_json::Value| v.as_u64())
                .unwrap_or(0);
            format!(
                "Index may be stale (age: {age}; pending sessions: {pending}). Run `cass index --full` or enable watch mode for fresh results."
            )
        });

    let index_freshness_for_closure = index_freshness.clone();
    let state_meta_with_warning = state_meta.map(|mut meta| {
        if let Some(fresh) = index_freshness_for_closure
            && let serde_json::Value::Object(ref mut m) = meta
        {
            m.insert("index_freshness".to_string(), fresh);
        }
        if let Some(warn) = &warning
            && let serde_json::Value::Object(ref mut m) = meta
        {
            m.insert(
                "_warning".to_string(),
                serde_json::Value::String(warn.clone()),
            );
        }
        meta
    });

    if let Some(format) = effective_robot {
        // Robot output mode (JSON)
        output_robot_results(
            query,
            limit_val,
            offset_val,
            &display_result,
            format,
            robot_meta,
            elapsed_ms,
            &fields,
            truncation_budgets,
            max_tokens,
            request_id.clone(),
            cursor.clone(),
            next_cursor,
            state_meta_with_warning,
            index_freshness,
            warning,
            &aggregations,
            total_matches,
            explanation.as_ref(),
            timed_out,
            timeout_ms,
            effective_mode,
            search_ms,
            rerank_ms,
        )?;
    } else if display_result.hits.is_empty() {
        eprintln!("No results found.");
    } else if let Some(display) = display_format {
        // Human-readable display formats
        output_display_results(&display_result.hits, display, wrap, query, highlight)?;
    } else {
        // Default plain text output
        for hit in &display_result.hits {
            println!("----------------------------------------------------------------");
            println!(
                "Score: {:.2} | Agent: {} | WS: {}",
                hit.score, hit.agent, hit.workspace
            );
            println!("Path: {}", hit.source_path);
            let snippet = hit.snippet.replace('\n', " ");
            let snippet = if highlight {
                highlight_matches(&snippet, query, "**", "**")
            } else {
                snippet
            };
            println!("Snippet: {}", apply_wrap(&snippet, wrap));
        }
        println!("----------------------------------------------------------------");
    }

    Ok(())
}

/// Output search results in human-readable display format
fn output_display_results(
    hits: &[crate::search::query::SearchHit],
    format: DisplayFormat,
    wrap: WrapConfig,
    query: &str,
    highlight: bool,
) -> CliResult<()> {
    match format {
        DisplayFormat::Table => {
            // Aligned columns with headers
            println!("{:<6} {:<12} {:<25} SNIPPET", "SCORE", "AGENT", "WORKSPACE");
            println!("{}", "-".repeat(80));
            for hit in hits {
                let workspace = truncate_start(&hit.workspace, 24);
                let snippet = hit.snippet.replace('\n', " ");
                let snippet = if highlight {
                    highlight_matches(&snippet, query, "**", "**")
                } else {
                    snippet
                };
                let snippet_display = truncate_end(&snippet, 50);
                println!(
                    "{:<6.2} {:<12} {:<25} {}",
                    hit.score, hit.agent, workspace, snippet_display
                );
            }
            println!("\n{} results", hits.len());
        }
        DisplayFormat::Lines => {
            // One-liner per result
            for hit in hits {
                let snippet = hit.snippet.replace('\n', " ");
                let snippet = if highlight {
                    highlight_matches(&snippet, query, "**", "**")
                } else {
                    snippet
                };
                let snippet_short = truncate_end(&snippet, 60);
                println!(
                    "[{:.1}] {} | {} | {}",
                    hit.score, hit.agent, hit.source_path, snippet_short
                );
            }
        }
        DisplayFormat::Markdown => {
            // Markdown with headers and code blocks
            println!("# Search Results\n");
            println!("Found **{}** results.\n", hits.len());
            for (i, hit) in hits.iter().enumerate() {
                println!("## {}. {} (score: {:.2})\n", i + 1, hit.agent, hit.score);
                println!("- **Workspace**: `{}`", hit.workspace);
                println!("- **Path**: `{}`", hit.source_path);
                if let Some(ts) = hit.created_at {
                    let dt = chrono::DateTime::from_timestamp_millis(ts).map_or_else(
                        || "unknown".to_string(),
                        |d| d.format("%Y-%m-%d %H:%M").to_string(),
                    );
                    println!("- **Created**: {dt}");
                }
                let snippet = if highlight {
                    // Use backticks for highlighting in markdown code blocks (shows as-is)
                    // But for non-code context, we'd use **bold**
                    highlight_matches(&hit.snippet, query, ">>>", "<<<")
                } else {
                    hit.snippet.clone()
                };
                let snippet = apply_wrap(&snippet, wrap);
                println!("\n```\n{snippet}\n```\n");
            }
        }
    }
    Ok(())
}

/// Expand field presets and return the resolved field list
fn expand_field_presets(fields: &Option<Vec<String>>) -> Option<Vec<String>> {
    fields.as_ref().map(|f| {
        f.iter()
            .flat_map(|field| match field.as_str() {
                "minimal" => vec![
                    "source_path".to_string(),
                    "line_number".to_string(),
                    "agent".to_string(),
                ],
                "summary" => vec![
                    "source_path".to_string(),
                    "line_number".to_string(),
                    "agent".to_string(),
                    "title".to_string(),
                    "score".to_string(),
                ],
                // Provenance preset (P3.4) - add source origin info to results
                "provenance" => vec![
                    "source_id".to_string(),
                    "origin_kind".to_string(),
                    "origin_host".to_string(),
                ],
                "*" | "all" => vec![], // Empty means include all - handled specially
                other => vec![other.to_string()],
            })
            .collect()
    })
}

fn resolve_field_mask(
    fields: &Option<Vec<String>>,
    format: Option<RobotFormat>,
    display_format: Option<DisplayFormat>,
) -> crate::search::query::FieldMask {
    use crate::search::query::FieldMask;

    if matches!(format, Some(RobotFormat::Sessions)) {
        return FieldMask::new(false, false, false, false);
    }

    if format.is_none() && display_format.is_none() {
        return FieldMask::new(true, true, false, true);
    }

    if format.is_none() {
        return FieldMask::new(true, true, false, true);
    }

    let resolved_fields = expand_field_presets(fields);
    let wants_all = fields.is_none()
        || resolved_fields
            .as_ref()
            .is_some_and(|field_list| field_list.is_empty());
    let wants_snippet = wants_all
        || resolved_fields
            .as_ref()
            .is_some_and(|field_list| field_list.iter().any(|f| f == "snippet"));
    let wants_content = wants_all
        || resolved_fields
            .as_ref()
            .is_some_and(|field_list| field_list.iter().any(|f| f == "content"));
    let wants_title = wants_all
        || resolved_fields
            .as_ref()
            .is_some_and(|field_list| field_list.iter().any(|f| f == "title"));

    let needs_content = wants_content || wants_snippet;
    let allows_cache = needs_content;
    FieldMask::new(needs_content, wants_snippet, wants_title, allows_cache)
}

/// Filter a search hit to only include the requested fields
fn filter_hit_fields(
    hit: &crate::search::query::SearchHit,
    fields: &Option<Vec<String>>,
) -> serde_json::Value {
    let all_fields = serde_json::to_value(hit).unwrap_or_default();

    match fields {
        None => all_fields,                                      // No filtering
        Some(field_list) if field_list.is_empty() => all_fields, // "all" or "*" preset
        Some(field_list) => {
            let mut filtered = serde_json::Map::new();
            let known_fields = [
                "score",
                "agent",
                "workspace",
                "source_path",
                "snippet",
                "content",
                "title",
                "created_at",
                "line_number",
                "match_type",
                // Provenance fields (P3.4)
                "source_id",
                "origin_kind",
                "origin_host",
            ];

            for field in field_list {
                if let Some(value) = all_fields.get(field) {
                    filtered.insert(field.clone(), value.clone());
                } else if !known_fields.contains(&field.as_str()) {
                    // Warn about unknown fields (only once per unknown field)
                    warn!(unknown_field = %field, "Unknown field in --fields, ignoring");
                }
            }
            serde_json::Value::Object(filtered)
        }
    }
}

/// Truncate a string to `max_len` characters, UTF-8 safe, with ellipsis
fn truncate_content(s: &str, max_len: usize) -> (String, bool) {
    let char_count = s.chars().count();
    if char_count <= max_len {
        (s.to_string(), false)
    } else {
        // Leave room for "..." (3 chars)
        let truncate_at = max_len.saturating_sub(3);
        let truncated: String = s.chars().take(truncate_at).collect();
        (format!("{truncated}..."), true)
    }
}

/// Apply content truncation to a filtered hit JSON object
#[derive(Clone, Copy)]
struct FieldBudgets {
    snippet: Option<usize>,
    content: Option<usize>,
    title: Option<usize>,
    fallback: Option<usize>,
}

fn apply_content_truncation(hit: serde_json::Value, budgets: FieldBudgets) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = hit else {
        return hit;
    };

    let fields = [
        ("snippet", budgets.snippet.or(budgets.fallback)),
        ("content", budgets.content.or(budgets.fallback)),
        ("title", budgets.title.or(budgets.fallback)),
    ];

    for (field, budget) in fields {
        if let (Some(limit), Some(serde_json::Value::String(s))) = (budget, obj.get(field)) {
            let (truncated, was_truncated) = truncate_content(s, limit);
            if was_truncated {
                obj.insert(field.to_string(), serde_json::Value::String(truncated));
                obj.insert(format!("{field}_truncated"), serde_json::Value::Bool(true));
            }
        }
    }

    serde_json::Value::Object(obj)
}

/// Clamp hits to an approximate token budget (4 chars ≈ 1 token). Returns (hits, `est_tokens`, clamped?)
fn clamp_hits_to_budget(
    hits: Vec<serde_json::Value>,
    max_tokens: Option<usize>,
) -> (Vec<serde_json::Value>, Option<usize>, bool) {
    let input_len = hits.len();
    let Some(tokens) = max_tokens else {
        let est = serde_json::to_string(&hits)
            .map(|s| s.chars().count() / 4)
            .ok();
        return (hits, est, false);
    };

    let budget_chars = tokens.saturating_mul(4);
    let mut acc_chars = 0usize;
    let mut kept: Vec<serde_json::Value> = Vec::new();
    for hit in hits {
        let len = serde_json::to_string(&hit)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        if !kept.is_empty() && acc_chars + len > budget_chars {
            break;
        }
        acc_chars += len;
        kept.push(hit);
        if acc_chars >= budget_chars {
            break;
        }
    }
    let est = serde_json::to_string(&kept)
        .map(|s| s.chars().count() / 4)
        .ok();
    let clamped = kept.len() < input_len || est.is_some_and(|e| e > tokens);
    (kept, est, clamped)
}

fn robot_format_from_env() -> Option<RobotFormat> {
    dotenvy::var("CASS_OUTPUT_FORMAT")
        .ok()
        .and_then(|val| match val.trim().to_ascii_lowercase().as_str() {
            "json" => Some(RobotFormat::Json),
            "jsonl" => Some(RobotFormat::Jsonl),
            "compact" => Some(RobotFormat::Compact),
            "sessions" => Some(RobotFormat::Sessions),
            "toon" => Some(RobotFormat::Toon),
            _ => None,
        })
        .or_else(|| {
            dotenvy::var("TOON_DEFAULT_FORMAT").ok().and_then(|val| {
                match val.trim().to_ascii_lowercase().as_str() {
                    "toon" => Some(RobotFormat::Toon),
                    "json" => Some(RobotFormat::Json),
                    _ => None,
                }
            })
        })
}

fn toon_encode_options_from_env() -> toon_rust::EncodeOptions {
    let indent = match dotenvy::var("TOON_INDENT") {
        Ok(v) if !v.trim().is_empty() => match v.parse::<usize>() {
            Ok(n) => Some(n),
            Err(e) => {
                warn!("invalid TOON_INDENT={v}: {e} (ignoring)");
                None
            }
        },
        _ => None,
    };

    let key_folding = match dotenvy::var("TOON_KEY_FOLDING") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "" | "off" | "0" | "false" => Some(toon_rust::options::KeyFoldingMode::Off),
            "safe" => Some(toon_rust::options::KeyFoldingMode::Safe),
            other => {
                warn!("invalid TOON_KEY_FOLDING={other} (expected off|safe); ignoring");
                None
            }
        },
        Err(_) => None,
    };

    toon_rust::EncodeOptions {
        indent,
        delimiter: None,
        key_folding,
        flatten_depth: None,
        replacer: None,
    }
}

fn output_structured_value(payload: serde_json::Value, format: RobotFormat) -> CliResult<()> {
    match format {
        RobotFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        RobotFormat::Jsonl | RobotFormat::Compact | RobotFormat::Sessions => {
            println!("{}", serde_json::to_string(&payload).unwrap_or_default());
        }
        RobotFormat::Toon => {
            let toon_str = toon_rust::encode(payload, Some(toon_encode_options_from_env()));
            print!("{toon_str}");
        }
    }
    Ok(())
}

/// Output search results in robot-friendly format
#[allow(clippy::too_many_arguments, unused_variables)]
fn output_robot_results(
    query: &str,
    limit: usize,
    offset: usize,
    result: &crate::search::query::SearchResult,
    format: RobotFormat,
    include_meta: bool,
    elapsed_ms: u64,
    fields: &Option<Vec<String>>,
    truncation_budgets: FieldBudgets,
    max_tokens: Option<usize>,
    request_id: Option<String>,
    input_cursor: Option<String>,
    next_cursor: Option<String>,
    state_meta: Option<serde_json::Value>,
    index_freshness: Option<serde_json::Value>,
    warning: Option<String>,
    aggregations: &Aggregations,
    total_matches: usize,
    explanation: Option<&crate::search::query::QueryExplanation>,
    timed_out: bool,
    timeout_ms: Option<u64>,
    search_mode: crate::search::query::SearchMode,
    search_ms: u64,
    rerank_ms: u64,
) -> CliResult<()> {
    if matches!(format, RobotFormat::Sessions) {
        // Output unique session paths only, one per line.
        // This format is designed for chained searches via --sessions-from.
        use std::collections::BTreeSet;
        let paths: BTreeSet<&str> = result
            .hits
            .iter()
            .map(|hit| hit.source_path.as_str())
            .collect();
        for path in paths {
            println!("{path}");
        }
        return Ok(());
    }

    // Expand presets (minimal, summary, provenance, all, *)
    let resolved_fields = expand_field_presets(fields);

    // Filter hits to requested fields, then apply content truncation
    let filtered_hits: Vec<serde_json::Value> = if resolved_fields.as_ref().is_some_and(|fields| {
        fields.len() == 3
            && fields[0] == "source_path"
            && fields[1] == "line_number"
            && fields[2] == "agent"
    }) {
        result
            .hits
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "source_path": hit.source_path.as_str(),
                    "line_number": hit.line_number,
                    "agent": hit.agent.as_str(),
                })
            })
            .collect()
    } else {
        result
            .hits
            .iter()
            .map(|hit| filter_hit_fields(hit, &resolved_fields))
            .map(|hit| apply_content_truncation(hit, truncation_budgets))
            .collect()
    };

    // Clamp hits to token budget if provided (approx 4 chars per token)
    let (filtered_hits, tokens_estimated, hits_clamped) =
        clamp_hits_to_budget(filtered_hits, max_tokens);

    // Serialize aggregations if present
    let agg_json = if aggregations.is_empty() {
        None
    } else {
        Some(serde_json::to_value(aggregations).unwrap_or_default())
    };

    match format {
        RobotFormat::Json => {
            let mut payload = serde_json::json!({
                "query": query,
                "limit": limit,
                "offset": offset,
                "count": filtered_hits.len(),
                "total_matches": total_matches,
                "hits": filtered_hits,
                "max_tokens": max_tokens,
                "request_id": request_id,
                "cursor": input_cursor,
                "hits_clamped": hits_clamped,
            });

            // Add suggestions if present
            if !result.suggestions.is_empty()
                && let serde_json::Value::Object(ref mut map) = payload
            {
                map.insert(
                    "suggestions".to_string(),
                    serde_json::to_value(&result.suggestions).unwrap_or_default(),
                );
            }

            // Add aggregations if present
            if let (Some(agg), serde_json::Value::Object(map)) = (&agg_json, &mut payload) {
                map.insert("aggregations".to_string(), agg.clone());
            }

            // Add query explanation if requested
            if let (Some(exp), serde_json::Value::Object(map)) = (explanation, &mut payload) {
                map.insert(
                    "explanation".to_string(),
                    serde_json::to_value(exp).unwrap_or_default(),
                );
            }

            // Add extended metadata if requested
            if include_meta && let serde_json::Value::Object(ref mut map) = payload {
                let mut meta = serde_json::json!({
                    "elapsed_ms": elapsed_ms,
                    "search_mode": search_mode,
                    "wildcard_fallback": result.wildcard_fallback,
                    "cache_stats": {
                        "hits": result.cache_stats.cache_hits,
                        "misses": result.cache_stats.cache_miss,
                        "shortfall": result.cache_stats.cache_shortfall,
                    },
                    // Search pipeline timing breakdown (T7.4)
                    "timing": {
                        "search_ms": search_ms,
                        "rerank_ms": rerank_ms,
                        "other_ms": elapsed_ms.saturating_sub(search_ms).saturating_sub(rerank_ms),
                    },
                    "tokens_estimated": tokens_estimated,
                    "max_tokens": max_tokens,
                    "request_id": request_id,
                    "next_cursor": next_cursor,
                    "hits_clamped": hits_clamped,
                });
                if let Some(state) = state_meta
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("state".to_string(), state);
                }
                if let Some(freshness) = index_freshness
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("index_freshness".to_string(), freshness);
                }
                // Add timeout info to _meta if timeout was configured
                if let Some(timeout) = timeout_ms
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("timeout_ms".to_string(), serde_json::json!(timeout));
                    m.insert("timed_out".to_string(), serde_json::json!(timed_out));
                    if timed_out {
                        m.insert("partial_results".to_string(), serde_json::json!(true));
                    }
                }
                // Add ANN stats to _meta if approximate search was used
                if let Some(ref ann_stats) = result.ann_stats
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert(
                        "ann_stats".to_string(),
                        serde_json::to_value(ann_stats).unwrap_or_default(),
                    );
                }
                map.insert("_meta".to_string(), meta);

                if let Some(warn) = &warning {
                    map.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
                    );
                }
                // Add top-level timeout indicator if timed out
                if timed_out {
                    map.insert(
                        "_timeout".to_string(),
                        serde_json::json!({
                            "code": 10,
                            "kind": "timeout",
                            "message": format!("Operation exceeded timeout of {}ms", timeout_ms.unwrap_or(0)),
                            "retryable": true,
                            "partial_results": true
                        }),
                    );
                }
            }

            let out = serde_json::to_string_pretty(&payload).map_err(|e| CliError {
                code: 9,
                kind: "encode-json",
                message: format!("failed to encode json: {e}"),
                hint: None,
                retryable: false,
            })?;
            println!("{out}");
        }
        RobotFormat::Jsonl => {
            // JSONL: one object per line, optional _meta header
            if include_meta
                || agg_json.is_some()
                || !result.suggestions.is_empty()
                || explanation.is_some()
            {
                let mut meta = serde_json::json!({
                    "_meta": {
                        "query": query,
                        "limit": limit,
                        "offset": offset,
                        "count": filtered_hits.len(),
                        "total_matches": total_matches,
                        "elapsed_ms": elapsed_ms,
                        "search_mode": search_mode,
                        "wildcard_fallback": result.wildcard_fallback,
                        "cache_stats": {
                            "hits": result.cache_stats.cache_hits,
                            "misses": result.cache_stats.cache_miss,
                            "shortfall": result.cache_stats.cache_shortfall,
                        },
                        "tokens_estimated": tokens_estimated,
                        "max_tokens": max_tokens,
                        "request_id": request_id,
                        "next_cursor": next_cursor,
                        "hits_clamped": hits_clamped,
                    }
                });
                if let Some(state) = state_meta
                    && let serde_json::Value::Object(ref mut outer) = meta
                    && let Some(serde_json::Value::Object(m)) = outer.get_mut("_meta")
                {
                    m.insert("state".to_string(), state);
                }
                if let Some(freshness) = index_freshness
                    && let serde_json::Value::Object(ref mut outer) = meta
                    && let Some(serde_json::Value::Object(m)) = outer.get_mut("_meta")
                {
                    m.insert("index_freshness".to_string(), freshness);
                }
                // Add suggestions to meta line
                if !result.suggestions.is_empty()
                    && let serde_json::Value::Object(ref mut map) = meta
                {
                    map.insert(
                        "suggestions".to_string(),
                        serde_json::to_value(&result.suggestions).unwrap_or_default(),
                    );
                }
                // Add aggregations to meta line
                if let (Some(agg), serde_json::Value::Object(map)) = (&agg_json, &mut meta) {
                    map.insert("aggregations".to_string(), agg.clone());
                }
                // Add explanation to meta line
                if let (Some(exp), serde_json::Value::Object(map)) = (explanation, &mut meta) {
                    map.insert(
                        "explanation".to_string(),
                        serde_json::to_value(exp).unwrap_or_default(),
                    );
                }
                if let Some(warn) = &warning
                    && let Some(m) = meta.get_mut("_meta").and_then(|v| v.as_object_mut())
                {
                    m.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
                    );
                }
                // Add timeout info to JSONL _meta
                if let Some(m) = meta.get_mut("_meta").and_then(|v| v.as_object_mut())
                    && let Some(timeout) = timeout_ms
                {
                    m.insert("timeout_ms".to_string(), serde_json::json!(timeout));
                    m.insert("timed_out".to_string(), serde_json::json!(timed_out));
                    if timed_out {
                        m.insert("partial_results".to_string(), serde_json::json!(true));
                    }
                }
                // Add top-level timeout indicator if timed out
                if timed_out && let serde_json::Value::Object(ref mut map) = meta {
                    map.insert(
                        "_timeout".to_string(),
                        serde_json::json!({
                            "code": 10,
                            "kind": "timeout",
                            "message": format!("Operation exceeded timeout of {}ms", timeout_ms.unwrap_or(0)),
                            "retryable": true,
                            "partial_results": true
                        }),
                    );
                }
                println!("{}", serde_json::to_string(&meta).unwrap_or_default());
            }
            // One hit per line (with field filtering applied)
            for hit in &filtered_hits {
                println!("{}", serde_json::to_string(hit).unwrap_or_default());
            }
        }
        RobotFormat::Compact => {
            // Single-line compact JSON
            let mut payload = serde_json::json!({
                "query": query,
                "limit": limit,
                "offset": offset,
                "count": filtered_hits.len(),
                "total_matches": total_matches,
                "hits": filtered_hits,
                "max_tokens": max_tokens,
                "request_id": request_id,
                "cursor": input_cursor,
                "hits_clamped": hits_clamped,
            });

            // Add suggestions if present
            if !result.suggestions.is_empty()
                && let serde_json::Value::Object(ref mut map) = payload
            {
                map.insert(
                    "suggestions".to_string(),
                    serde_json::to_value(&result.suggestions).unwrap_or_default(),
                );
            }

            // Add aggregations if present
            if let (Some(agg), serde_json::Value::Object(map)) = (&agg_json, &mut payload) {
                map.insert("aggregations".to_string(), agg.clone());
            }

            // Add query explanation if requested
            if let (Some(exp), serde_json::Value::Object(map)) = (explanation, &mut payload) {
                map.insert(
                    "explanation".to_string(),
                    serde_json::to_value(exp).unwrap_or_default(),
                );
            }

            if include_meta && let serde_json::Value::Object(ref mut map) = payload {
                let mut meta = serde_json::json!({
                    "elapsed_ms": elapsed_ms,
                    "search_mode": search_mode,
                    "wildcard_fallback": result.wildcard_fallback,
                    "tokens_estimated": tokens_estimated,
                    "max_tokens": max_tokens,
                    "request_id": request_id,
                    "next_cursor": next_cursor,
                    "hits_clamped": hits_clamped,
                });
                if let Some(state) = state_meta
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("state".to_string(), state);
                }
                if let Some(freshness) = index_freshness
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("index_freshness".to_string(), freshness);
                }
                // Add timeout info to _meta if timeout was configured
                if let Some(timeout) = timeout_ms
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("timeout_ms".to_string(), serde_json::json!(timeout));
                    m.insert("timed_out".to_string(), serde_json::json!(timed_out));
                    if timed_out {
                        m.insert("partial_results".to_string(), serde_json::json!(true));
                    }
                }
                // Add ANN stats to _meta if approximate search was used
                if let Some(ref ann_stats) = result.ann_stats
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert(
                        "ann_stats".to_string(),
                        serde_json::to_value(ann_stats).unwrap_or_default(),
                    );
                }
                map.insert("_meta".to_string(), meta);
                if let Some(warn) = &warning {
                    map.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
                    );
                }
                // Add top-level timeout indicator if timed out
                if timed_out {
                    map.insert(
                        "_timeout".to_string(),
                        serde_json::json!({
                            "code": 10,
                            "kind": "timeout",
                            "message": format!("Operation exceeded timeout of {}ms", timeout_ms.unwrap_or(0)),
                            "retryable": true,
                            "partial_results": true
                        }),
                    );
                }
            }

            let out = serde_json::to_string(&payload).map_err(|e| CliError {
                code: 9,
                kind: "encode-json",
                message: format!("failed to encode json: {e}"),
                hint: None,
                retryable: false,
            })?;
            println!("{out}");
        }
        RobotFormat::Toon => {
            // TOON: Token-Optimized Object Notation
            // Encodes via toon_rust crate for token-efficient output
            let mut payload = serde_json::json!({
                "query": query,
                "limit": limit,
                "offset": offset,
                "count": filtered_hits.len(),
                "total_matches": total_matches,
                "hits": filtered_hits,
                "max_tokens": max_tokens,
                "request_id": request_id,
                "cursor": input_cursor,
                "hits_clamped": hits_clamped,
            });

            // Add suggestions if present
            if !result.suggestions.is_empty()
                && let serde_json::Value::Object(ref mut map) = payload
            {
                map.insert(
                    "suggestions".to_string(),
                    serde_json::to_value(&result.suggestions).unwrap_or_default(),
                );
            }

            // Add aggregations if present
            if let (Some(agg), serde_json::Value::Object(map)) = (&agg_json, &mut payload) {
                map.insert("aggregations".to_string(), agg.clone());
            }

            // Add query explanation if requested
            if let (Some(exp), serde_json::Value::Object(map)) = (explanation, &mut payload) {
                map.insert(
                    "explanation".to_string(),
                    serde_json::to_value(exp).unwrap_or_default(),
                );
            }

            if include_meta && let serde_json::Value::Object(ref mut map) = payload {
                let mut meta = serde_json::json!({
                    "elapsed_ms": elapsed_ms,
                    "search_mode": search_mode,
                    "wildcard_fallback": result.wildcard_fallback,
                    "tokens_estimated": tokens_estimated,
                    "max_tokens": max_tokens,
                    "request_id": request_id,
                    "next_cursor": next_cursor,
                    "hits_clamped": hits_clamped,
                });
                if let Some(state) = state_meta
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("state".to_string(), state);
                }
                if let Some(freshness) = index_freshness
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("index_freshness".to_string(), freshness);
                }
                if let Some(timeout) = timeout_ms
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert("timeout_ms".to_string(), serde_json::json!(timeout));
                    m.insert("timed_out".to_string(), serde_json::json!(timed_out));
                    if timed_out {
                        m.insert("partial_results".to_string(), serde_json::json!(true));
                    }
                }
                // Add ANN stats to _meta if approximate search was used
                if let Some(ref ann_stats) = result.ann_stats
                    && let serde_json::Value::Object(ref mut m) = meta
                {
                    m.insert(
                        "ann_stats".to_string(),
                        serde_json::to_value(ann_stats).unwrap_or_default(),
                    );
                }
                map.insert("_meta".to_string(), meta);
                if let Some(warn) = &warning {
                    map.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
                    );
                }
                if timed_out {
                    map.insert(
                        "_timeout".to_string(),
                        serde_json::json!({
                            "code": 10,
                            "kind": "timeout",
                            "message": format!("Operation exceeded timeout of {}ms", timeout_ms.unwrap_or(0)),
                            "retryable": true,
                            "partial_results": true
                        }),
                    );
                }
            }

            let json_str = serde_json::to_string(&payload).map_err(|e| CliError {
                code: 9,
                kind: "encode-json",
                message: format!("failed to encode json: {e}"),
                hint: None,
                retryable: false,
            })?;

            let toon_str = toon_rust::encode(payload, Some(toon_encode_options_from_env()));

            // Preserve the existing "compact JSON" behavior by first ensuring the payload is
            // valid JSON (serde_json::to_string above). We don't need the string itself here.
            drop(json_str);
            print!("{toon_str}");
        }
        RobotFormat::Sessions => {
            unreachable!("RobotFormat::Sessions is handled above to avoid building hit payloads");
        }
    }

    Ok(())
}

fn run_stats(
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    source: Option<&str>,
    by_source: bool,
) -> CliResult<()> {
    use crate::sources::provenance::SourceFilter;

    let lazy = crate::storage::sqlite::LazyDb::from_overrides(data_dir_override, db_override);
    let conn = lazy.get("stats").map_err(lazy_db_to_cli_error)?;

    // Parse source filter (P3.7)
    let source_filter = source.map(SourceFilter::parse);

    // Build WHERE clause for source filtering
    let (source_where, source_param): (String, Option<String>) = match &source_filter {
        None | Some(SourceFilter::All) => (String::new(), None),
        Some(SourceFilter::Local) => (" WHERE c.source_id = 'local'".to_string(), None),
        Some(SourceFilter::Remote) => (" WHERE c.source_id != 'local'".to_string(), None),
        Some(SourceFilter::SourceId(id)) => {
            (" WHERE c.source_id = ?".to_string(), Some(id.clone()))
        }
    };

    // Get counts and statistics with source filter
    let conversation_count: i64 = if let Some(ref param) = source_param {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM conversations c{source_where}"),
            [param],
            |r| r.get(0),
        )
    } else {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM conversations c{source_where}"),
            [],
            |r| r.get(0),
        )
    }
    .unwrap_or(0);

    let message_count: i64 = if let Some(ref param) = source_param {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM messages m JOIN conversations c ON m.conversation_id = c.id{source_where}"
            ),
            [param],
            |r| r.get(0),
        )
    } else {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM messages m JOIN conversations c ON m.conversation_id = c.id{source_where}"
            ),
            [],
            |r| r.get(0),
        )
    }
    .unwrap_or(0);

    // Get per-agent breakdown with source filter
    let agent_sql = format!(
        "SELECT a.slug, COUNT(*) FROM conversations c JOIN agents a ON c.agent_id = a.id{source_where} GROUP BY a.slug ORDER BY COUNT(*) DESC"
    );
    let agent_rows: Vec<(String, i64)> = if let Some(ref param) = source_param {
        let mut stmt = conn
            .prepare(&agent_sql)
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        stmt.query_map([param], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| CliError::unknown(format!("query: {e}")))?
        .filter_map(std::result::Result::ok)
        .collect()
    } else {
        let mut stmt = conn
            .prepare(&agent_sql)
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| CliError::unknown(format!("query: {e}")))?
            .filter_map(std::result::Result::ok)
            .collect()
    };

    // Get workspace breakdown with source filter (top 10)
    let ws_sql = format!(
        "SELECT w.path, COUNT(*) FROM conversations c JOIN workspaces w ON c.workspace_id = w.id{source_where} GROUP BY w.path ORDER BY COUNT(*) DESC LIMIT 10"
    );
    let ws_rows: Vec<(String, i64)> = if let Some(ref param) = source_param {
        let mut stmt = conn
            .prepare(&ws_sql)
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        stmt.query_map([param], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| CliError::unknown(format!("query: {e}")))?
        .filter_map(std::result::Result::ok)
        .collect()
    } else {
        let mut stmt = conn
            .prepare(&ws_sql)
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| CliError::unknown(format!("query: {e}")))?
            .filter_map(std::result::Result::ok)
            .collect()
    };

    // Get date range with source filter.
    // Note: source_where already includes a leading " WHERE ...", so when it is present we must
    // append additional conditions with " AND ..." (not another WHERE).
    let date_sql = if source_where.is_empty() {
        "SELECT MIN(started_at), MAX(started_at) FROM conversations c WHERE started_at IS NOT NULL"
            .to_string()
    } else {
        format!(
            "SELECT MIN(started_at), MAX(started_at) FROM conversations c{source_where} AND started_at IS NOT NULL"
        )
    };
    let (oldest, newest): (Option<i64>, Option<i64>) = if let Some(ref param) = source_param {
        conn.query_row(&date_sql, [param], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((None, None))
    } else {
        conn.query_row(&date_sql, [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((None, None))
    };

    // Get per-source breakdown if requested (P3.7)
    let source_rows: Vec<(String, i64, i64)> = if by_source {
        let source_sql = format!(
            "SELECT c.source_id, COUNT(DISTINCT c.id) as convs, COUNT(m.id) as msgs
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             {source_where}
             GROUP BY c.source_id
             ORDER BY convs DESC"
        );
        let mut stmt = conn
            .prepare(&source_sql)
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        if let Some(ref param) = source_param {
            stmt.query_map([param], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| CliError::unknown(format!("query: {e}")))?
            .filter_map(std::result::Result::ok)
            .collect()
        } else {
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| CliError::unknown(format!("query: {e}")))?
            .filter_map(std::result::Result::ok)
            .collect()
        }
    } else {
        Vec::new()
    };

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let mut payload = serde_json::json!({
            "conversations": conversation_count,
            "messages": message_count,
            "by_agent": agent_rows.iter().map(|(a, c)| serde_json::json!({"agent": a, "count": c})).collect::<Vec<_>>(),
            "top_workspaces": ws_rows.iter().map(|(w, c)| serde_json::json!({"workspace": w, "count": c})).collect::<Vec<_>>(),
            "date_range": {
                "oldest": oldest.and_then(|ts| chrono::DateTime::from_timestamp_millis(ts).map(|d| d.to_rfc3339())),
                "newest": newest.and_then(|ts| chrono::DateTime::from_timestamp_millis(ts).map(|d| d.to_rfc3339())),
            },
            "db_path": lazy.path().display().to_string(),
        });

        // Add source filter info if specified (P3.7)
        if let Some(ref filter) = source_filter {
            payload["source_filter"] = serde_json::json!(filter.to_string());
        }

        // Add by_source breakdown if requested (P3.7)
        if by_source && !source_rows.is_empty() {
            payload["by_source"] = serde_json::json!(
                source_rows
                    .iter()
                    .map(|(s, convs, msgs)| {
                        serde_json::json!({
                            "source_id": s,
                            "conversations": convs,
                            "messages": msgs
                        })
                    })
                    .collect::<Vec<_>>()
            );
        }

        return output_structured_value(payload, fmt);
    }

    // Header with source filter indicator
    let title = if let Some(ref filter) = source_filter {
        format!("CASS Index Statistics (source: {})", filter)
    } else {
        "CASS Index Statistics".to_string()
    };
    println!("{title}");
    println!("{}", "=".repeat(title.len()));
    println!("Database: {}", lazy.path().display());
    println!();

    // Show by_source breakdown if requested (P3.7)
    if by_source && !source_rows.is_empty() {
        println!("By Source:");
        println!("  {:20} {:>10} {:>12}", "Source", "Convs", "Messages");
        println!("  {}", "-".repeat(44));
        for (src, convs, msgs) in &source_rows {
            println!("  {:20} {:>10} {:>12}", src, convs, msgs);
        }
        println!();
    }

    println!("Totals:");
    println!("  Conversations: {conversation_count}");
    println!("  Messages: {message_count}");
    println!();
    println!("By Agent:");
    for (agent, count) in &agent_rows {
        println!("  {agent}: {count}");
    }
    println!();
    if !ws_rows.is_empty() {
        println!("Top Workspaces:");
        for (ws, count) in &ws_rows {
            println!("  {ws}: {count}");
        }
        println!();
    }
    if let (Some(old), Some(new)) = (oldest, newest)
        && let (Some(old_dt), Some(new_dt)) = (
            chrono::DateTime::from_timestamp_millis(old),
            chrono::DateTime::from_timestamp_millis(new),
        )
    {
        println!(
            "Date Range: {} to {}",
            old_dt.format("%Y-%m-%d"),
            new_dt.format("%Y-%m-%d")
        );
    }

    Ok(())
}

fn run_diag(
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    verbose: bool,
) -> CliResult<()> {
    use rusqlite::Connection;
    use std::fs;

    let version = env!("CARGO_PKG_VERSION");
    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    // Use the actual versioned index path (index/v4, not tantivy_index)
    let index_path = crate::search::tantivy::index_dir(&data_dir)
        .unwrap_or_else(|_| data_dir.join("index").join("v4"));

    // Check database existence and get stats
    let (db_exists, db_size, conversation_count, message_count) = if db_path.exists() {
        let size = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        let (convs, msgs) = if let Ok(conn) = Connection::open(&db_path) {
            let convs: i64 = conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
                .unwrap_or(0);
            let msgs: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap_or(0);
            (convs, msgs)
        } else {
            (0, 0)
        };
        (true, size, convs, msgs)
    } else {
        (false, 0, 0, 0)
    };

    // Check index existence
    let (index_exists, index_size) = if index_path.exists() {
        let size = fs_dir_size(&index_path);
        (true, size)
    } else {
        (false, 0)
    };

    // Agent search paths - compute path once, then check existence
    let home = dirs::home_dir().unwrap_or_default();
    let config_dir = dirs::config_dir().unwrap_or_default();

    let agent_paths: Vec<(String, PathBuf, bool)> = diagnostics_connector_paths(&home, &config_dir)
        .into_iter()
        .map(|(name, path)| {
            let exists = path.exists();
            (name, path, exists)
        })
        .collect();

    let platform = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let payload = serde_json::json!({
            "version": version,
            "platform": { "os": platform, "arch": arch },
            "paths": {
                "data_dir": data_dir.display().to_string(),
                "db_path": db_path.display().to_string(),
                "index_path": index_path.display().to_string(),
            },
            "database": {
                "exists": db_exists,
                "size_bytes": db_size,
                "conversations": conversation_count,
                "messages": message_count,
            },
            "index": {
                "exists": index_exists,
                "size_bytes": index_size,
            },
            "connectors": agent_paths.iter().map(|(name, path, exists)| {
                serde_json::json!({
                    "name": name,
                    "path": path.display().to_string(),
                    "found": exists,
                })
            }).collect::<Vec<_>>(),
        });
        return output_structured_value(payload, fmt);
    }

    println!("CASS Diagnostic Report");
    println!("======================");
    println!();
    println!("Version: {version}");
    println!("Platform: {platform} ({arch})");
    println!();
    println!("Paths:");
    println!("  Data directory: {}", data_dir.display());
    println!("  Database: {}", db_path.display());
    println!("  Tantivy index: {}", index_path.display());
    println!();
    println!("Database Status:");
    if db_exists {
        println!("  Status: OK");
        if verbose {
            println!("  Size: {}", format_bytes(db_size));
        }
        println!("  Conversations: {conversation_count}");
        println!("  Messages: {message_count}");
    } else {
        println!("  Status: NOT FOUND");
        println!("  Hint: Run 'cass index --full' to create the database");
    }
    println!();
    println!("Index Status:");
    if index_exists {
        println!("  Status: OK");
        if verbose {
            println!("  Size: {}", format_bytes(index_size));
        }
    } else {
        println!("  Status: NOT FOUND");
        println!("  Hint: Run 'cass index --full' to create the index");
    }
    println!();
    println!("Connector Search Paths:");
    for (name, path, exists) in &agent_paths {
        let status = if *exists { "✓" } else { "✗" };
        println!("  {} {}: {}", status, name, path.display());
    }

    Ok(())
}

fn fs_dir_size(path: &std::path::Path) -> u64 {
    if !path.is_dir() {
        return std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        fs_dir_size(&p)
                    } else {
                        std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

fn public_connector_slug(slug: &str) -> &str {
    match slug {
        // Public API contract uses claude_code even though indexer registry key is claude.
        "claude" => "claude_code",
        other => other,
    }
}

fn capabilities_connector_names() -> Vec<String> {
    // Preserve existing connector ordering for stable API contracts.
    let preferred = [
        "codex",
        "claude_code",
        "gemini",
        "clawdbot",
        "vibe",
        "opencode",
        "amp",
        "cline",
        "aider",
        "cursor",
        "chatgpt",
        "pi_agent",
        "factory",
        "openclaw",
    ];

    let mut connectors: Vec<String> = preferred.iter().map(|name| (*name).to_string()).collect();
    let mut seen: HashSet<String> = connectors.iter().cloned().collect();

    // Append any connector newly registered in the indexer to prevent list drift.
    for (slug, _) in crate::indexer::get_connector_factories() {
        let public = public_connector_slug(slug).to_string();
        if seen.insert(public.clone()) {
            connectors.push(public);
        }
    }

    connectors
}

fn diagnostics_connector_paths(
    home: &std::path::Path,
    config_dir: &std::path::Path,
) -> Vec<(String, PathBuf)> {
    let codex_home = dotenvy::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".codex"));
    let gemini_home = dotenvy::var("GEMINI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".gemini"));
    let opencode_path = dotenvy::var("OPENCODE_STORAGE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_local_dir()
                .unwrap_or_else(|| home.join(".local/share"))
                .join("opencode/storage")
        });
    let aider_path = dotenvy::var("CASS_AIDER_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".aider.chat.history.md"));
    let pi_home = dotenvy::var("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".pi/agent"));
    let pi_path = if pi_home.join("sessions").exists() {
        pi_home.join("sessions")
    } else {
        pi_home
    };

    let cursor_path = crate::connectors::cursor::CursorConnector::app_support_dir()
        .unwrap_or_else(|| home.join("Library/Application Support/Cursor/User"));
    let chatgpt_path = crate::connectors::chatgpt::ChatGptConnector::app_support_dir()
        .unwrap_or_else(|| home.join("Library/Application Support/com.openai.chat"));

    vec![
        ("codex".to_string(), codex_home.join("sessions")),
        ("claude_code".to_string(), home.join(".claude/projects")),
        (
            "cline".to_string(),
            config_dir.join("Code/User/globalStorage/saoudrizwan.claude-dev"),
        ),
        ("gemini".to_string(), gemini_home.join("tmp")),
        ("clawdbot".to_string(), home.join(".clawdbot/sessions")),
        ("vibe".to_string(), home.join(".vibe/logs/session")),
        ("opencode".to_string(), opencode_path),
        (
            "amp".to_string(),
            config_dir.join("Code/User/globalStorage/sourcegraph.amp"),
        ),
        ("aider".to_string(), aider_path),
        ("cursor".to_string(), cursor_path),
        ("chatgpt".to_string(), chatgpt_path),
        ("pi_agent".to_string(), pi_path),
        ("factory".to_string(), home.join(".factory/sessions")),
        (
            "openclaw".to_string(),
            home.join(".openclaw/agents/openclaw/sessions"),
        ),
    ]
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// Truncate a string from the start, keeping the last `max_chars` characters.
/// UTF-8 safe. Adds "..." prefix if truncated.
fn truncate_start(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else if max_chars <= 3 {
        // Not enough room for any content plus "..."
        "...".to_string()
    } else {
        let skip = char_count.saturating_sub(max_chars.saturating_sub(3));
        format!("...{}", s.chars().skip(skip).collect::<String>())
    }
}

/// Truncate a string from the end, keeping the first `max_chars` characters.
/// UTF-8 safe. Adds "..." suffix if truncated.
fn truncate_end(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else if max_chars <= 3 {
        // Not enough room for any content plus "..."
        "...".to_string()
    } else {
        let take = max_chars.saturating_sub(3);
        format!("{}...", s.chars().take(take).collect::<String>())
    }
}

/// Quick health check for agents: index freshness, db stats, recommended action.
/// Designed to be fast (<100ms) for pre-search checks.
fn run_status(
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    stale_threshold: u64,
    _robot_meta: bool,
) -> CliResult<()> {
    use rusqlite::Connection;
    use std::time::{SystemTime, UNIX_EPOCH};

    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    // Use the actual versioned index path (index/v4, not tantivy_index)
    let index_path = crate::search::tantivy::index_dir(&data_dir)
        .unwrap_or_else(|_| data_dir.join("index").join("v4"));
    let watch_state_path = data_dir.join("watch_state.json");

    // Check if database exists
    let db_exists = db_path.exists();
    let index_exists = index_path.exists();

    // Get current timestamp
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Default values if db doesn't exist
    let mut conversation_count: i64 = 0;
    let mut message_count: i64 = 0;
    let mut last_indexed_at: Option<i64> = None;

    if db_exists && let Ok(conn) = Connection::open(&db_path) {
        // Get counts
        conversation_count = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap_or(0);
        message_count = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap_or(0);

        // Get last indexed timestamp from meta table
        last_indexed_at = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_indexed_at'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse::<i64>().ok());
    }

    // Calculate index age and staleness
    let index_age_secs = last_indexed_at.map(|ts| {
        let ts_secs = ts / 1000; // Convert millis to secs
        now_secs.saturating_sub(ts_secs as u64)
    });
    let is_stale = match index_age_secs {
        None => true,
        Some(age) => age > stale_threshold,
    };

    // Check for pending sessions from watch_state.json
    let pending_sessions = if watch_state_path.exists() {
        std::fs::read_to_string(&watch_state_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|v| v.get("pending_count").and_then(serde_json::Value::as_u64))
            .unwrap_or(0)
    } else {
        0
    };

    // Determine overall health
    let healthy = db_exists && index_exists && !is_stale;

    // Build recommended action
    let recommended_action = if !db_exists {
        Some("Run 'cass index --full' to create the database".to_string())
    } else if !index_exists {
        Some("Run 'cass index --full' to rebuild the search index".to_string())
    } else if is_stale || pending_sessions > 0 {
        let pending_msg = if pending_sessions > 0 {
            format!(" ({pending_sessions} sessions pending)")
        } else {
            String::new()
        };
        Some(format!(
            "Run 'cass index' to refresh the index{pending_msg}"
        ))
    } else {
        None
    };

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let ts_str = chrono::DateTime::from_timestamp(now_secs as i64, 0)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();
        let payload = serde_json::json!({
            "healthy": healthy,
            "index": {
                "exists": index_exists,
                "fresh": !is_stale,
                "last_indexed_at": last_indexed_at.map(|ts| {
                    chrono::DateTime::from_timestamp_millis(ts)
                        .map(|d| d.to_rfc3339())
                }),
                "age_seconds": index_age_secs,
                "stale": is_stale,
                "stale_threshold_seconds": stale_threshold,
            },
            "database": {
                "exists": db_exists,
                "conversations": conversation_count,
                "messages": message_count,
                "path": db_path.display().to_string(),
            },
            "pending": {
                "sessions": pending_sessions,
                "watch_active": watch_state_path.exists(),
            },
            "recommended_action": recommended_action,
            "_meta": {
                "timestamp": ts_str,
                "data_dir": data_dir.display().to_string(),
                "db_path": db_path.display().to_string(),
            },
        });
        return output_structured_value(payload, fmt);
    }

    // Human-readable output
    let status_icon = if healthy { "✓" } else { "!" };
    let status_word = if healthy {
        "Healthy"
    } else {
        "Attention needed"
    };

    println!("{status_icon} CASS Status: {status_word}");
    println!();

    // Index info
    println!("Index:");
    if index_exists {
        if let Some(age) = index_age_secs {
            let age_str = if age < 60 {
                format!("{age} seconds ago")
            } else if age < 3600 {
                format!("{} minutes ago", age / 60)
            } else if age < 86400 {
                format!("{} hours ago", age / 3600)
            } else {
                format!("{} days ago", age / 86400)
            };
            let stale_indicator = if is_stale { " (stale)" } else { "" };
            println!("  Last indexed: {age_str}{stale_indicator}");
        } else {
            println!("  Last indexed: unknown");
        }
    } else {
        println!("  Not found - run 'cass index --full'");
    }

    // Database info
    println!();
    println!("Database:");
    if db_exists {
        println!("  Conversations: {conversation_count}");
        println!("  Messages: {message_count}");
    } else {
        println!("  Not found");
    }

    // Pending
    if pending_sessions > 0 {
        println!();
        println!("Pending: {pending_sessions} sessions awaiting indexing");
    }

    // Recommended action
    if let Some(action) = &recommended_action {
        println!();
        println!("Recommended: {action}");
    }

    Ok(())
}

/// Minimal health check (<50ms). Exit 0=healthy, 1=unhealthy.
/// Designed for agent pre-flight checks before complex operations.
fn run_health(
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    stale_threshold: u64,
    _robot_meta: bool,
) -> CliResult<()> {
    use std::time::Instant;

    let start = Instant::now();
    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    let state = state_meta_json(&data_dir, &db_path, stale_threshold, false);

    let index_exists = state
        .get("index")
        .and_then(|i| i.get("exists"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let index_fresh = state
        .get("index")
        .and_then(|i| i.get("fresh"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let db_exists = state
        .get("database")
        .and_then(|d| d.get("exists"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let pending_sessions = state
        .get("pending")
        .and_then(|p| p.get("sessions"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Core operational health: can the tool be used at all?
    // Freshness and pending sessions are informational (reported in state) but don't prevent searching
    let healthy = db_exists && index_exists;
    let latency_ms = start.elapsed().as_millis() as u64;

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let payload = serde_json::json!({
            "healthy": healthy,
            "latency_ms": latency_ms,
            "state": state
        });
        output_structured_value(payload, fmt)?;
    } else if healthy {
        println!("✓ Healthy ({latency_ms}ms)");
        // Show informational warnings even when healthy
        if !index_fresh {
            println!("  Note: index stale (older than {}s)", stale_threshold);
        }
        if pending_sessions > 0 {
            println!("  Note: {pending_sessions} sessions pending reindex");
        }
    } else {
        println!("✗ Unhealthy ({latency_ms}ms)");
        if !db_exists {
            println!("  - database not found");
        }
        if !index_exists {
            println!("  - index not found");
        }
        println!("Run 'cass index --full' or 'cass index --watch' to create index.");
    }

    if healthy {
        Ok(())
    } else {
        Err(CliError {
            code: 1,
            kind: "health",
            message: "Health check failed".to_string(),
            hint: Some("Run 'cass index --full' to rebuild the index/database.".to_string()),
            retryable: true,
        })
    }
}

fn ensure_cass_origin(
    metadata: &mut serde_json::Value,
    source_id: &str,
    kind: crate::sources::provenance::SourceKind,
    host: Option<&str>,
) {
    if !metadata.is_object() {
        *metadata = serde_json::json!({});
    }

    let Some(obj) = metadata.as_object_mut() else {
        return;
    };

    let cass = obj
        .entry("cass".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(cass_obj) = cass.as_object_mut() else {
        return;
    };

    let origin = cass_obj
        .entry("origin".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(origin_obj) = origin.as_object_mut() {
        origin_obj
            .entry("source_id".to_string())
            .or_insert_with(|| serde_json::Value::String(source_id.to_string()));
        origin_obj
            .entry("kind".to_string())
            .or_insert_with(|| serde_json::Value::String(kind.as_str().to_string()));
        if let Some(host) = host {
            origin_obj
                .entry("host".to_string())
                .or_insert_with(|| serde_json::Value::String(host.to_string()));
        }
    }
}

fn rebuild_tantivy_from_db(
    db_path: &Path,
    data_dir: &Path,
    total_conversations: usize,
    progress: Option<std::sync::Arc<indexer::IndexingProgress>>,
) -> CliResult<usize> {
    use crate::connectors::{NormalizedConversation, NormalizedMessage};
    use crate::model::types::MessageRole;
    use crate::search::tantivy::TantivyIndex;
    use crate::sources::provenance::{LOCAL_SOURCE_ID, SourceKind};
    use crate::storage::sqlite::SqliteStorage;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    let storage = SqliteStorage::open_readonly(db_path).map_err(|e| CliError {
        code: 5,
        kind: "doctor",
        message: format!("failed to open database for rebuild: {e}"),
        hint: None,
        retryable: true,
    })?;

    let sources = storage.list_sources().unwrap_or_default();
    let mut source_map: HashMap<String, (SourceKind, Option<String>)> = HashMap::new();
    for source in sources {
        source_map.insert(source.id, (source.kind, source.host_label));
    }

    let index_path = crate::search::tantivy::index_dir(data_dir).map_err(|e| CliError {
        code: 5,
        kind: "doctor",
        message: format!("failed to resolve index path: {e}"),
        hint: None,
        retryable: true,
    })?;

    let _ = std::fs::remove_dir_all(&index_path);
    std::fs::create_dir_all(&index_path).map_err(|e| CliError {
        code: 5,
        kind: "doctor",
        message: format!("failed to create index directory: {e}"),
        hint: None,
        retryable: true,
    })?;

    let mut t_index = TantivyIndex::open_or_create(&index_path).map_err(|e| CliError {
        code: 5,
        kind: "doctor",
        message: format!("failed to create tantivy index: {e}"),
        hint: None,
        retryable: true,
    })?;

    if let Some(p) = &progress {
        p.phase.store(2, Ordering::Relaxed);
        p.is_rebuilding.store(true, Ordering::Relaxed);
        p.total.store(total_conversations, Ordering::Relaxed);
        p.current.store(0, Ordering::Relaxed);
        p.discovered_agents.store(0, Ordering::Relaxed);
    }

    let page_size: i64 = 200;
    let mut offset: i64 = 0;
    let mut indexed_docs: usize = 0;

    loop {
        let batch = storage
            .list_conversations(page_size, offset)
            .map_err(|e| CliError::unknown(format!("failed to list conversations: {e}")))?;
        if batch.is_empty() {
            break;
        }

        for conv in batch {
            let Some(conv_id) = conv.id else {
                continue;
            };

            let messages = storage
                .fetch_messages(conv_id)
                .map_err(|e| CliError::unknown(format!("failed to fetch messages: {e}")))?;

            let mut metadata = conv.metadata_json.clone();
            let (kind, host_label) =
                source_map.get(&conv.source_id).cloned().unwrap_or_else(|| {
                    let fallback_kind = if conv.source_id == LOCAL_SOURCE_ID {
                        SourceKind::Local
                    } else {
                        SourceKind::Ssh
                    };
                    (fallback_kind, None)
                });

            let host = conv.origin_host.as_deref().or(host_label.as_deref());
            ensure_cass_origin(&mut metadata, &conv.source_id, kind, host);

            let normalized_messages: Vec<NormalizedMessage> = messages
                .into_iter()
                .map(|msg| {
                    let role = match msg.role {
                        MessageRole::User => "user".to_string(),
                        MessageRole::Agent => "assistant".to_string(),
                        MessageRole::Tool => "tool".to_string(),
                        MessageRole::System => "system".to_string(),
                        MessageRole::Other(other) => other,
                    };

                    NormalizedMessage {
                        idx: msg.idx,
                        role,
                        author: msg.author,
                        created_at: msg.created_at,
                        content: msg.content,
                        extra: msg.extra_json,
                        snippets: Vec::new(),
                    }
                })
                .collect();

            let normalized = NormalizedConversation {
                agent_slug: conv.agent_slug,
                external_id: conv.external_id,
                title: conv.title,
                workspace: conv.workspace,
                source_path: conv.source_path,
                started_at: conv.started_at,
                ended_at: conv.ended_at,
                metadata,
                messages: normalized_messages,
            };

            indexed_docs += normalized.messages.len();
            t_index
                .add_messages(&normalized, &normalized.messages)
                .map_err(|e| CliError::unknown(format!("failed to index messages: {e}")))?;

            if let Some(p) = &progress {
                p.current.fetch_add(1, Ordering::Relaxed);
            }
        }

        offset += page_size;
    }

    t_index
        .commit()
        .map_err(|e| CliError::unknown(format!("failed to commit index: {e}")))?;

    if let Some(p) = &progress {
        p.phase.store(0, Ordering::Relaxed);
        p.is_rebuilding.store(false, Ordering::Relaxed);
    }

    Ok(indexed_docs)
}

fn wait_with_progress<T>(
    handle: std::thread::JoinHandle<CliResult<T>>,
    progress: std::sync::Arc<indexer::IndexingProgress>,
    show_progress: bool,
    show_plain: bool,
    initial_message: &str,
) -> CliResult<T> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    if show_progress {
        use indicatif::{ProgressBar, ProgressStyle};

        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        pb.set_message(initial_message.to_string());
        pb.enable_steady_tick(Duration::from_millis(80));

        let mut last_phase = usize::MAX;
        let mut last_current = usize::MAX;
        let mut last_agents = usize::MAX;
        let mut last_update = Instant::now();

        loop {
            if handle.is_finished() {
                break;
            }

            let phase = progress.phase.load(Ordering::Relaxed);
            let total = progress.total.load(Ordering::Relaxed);
            let current = progress.current.load(Ordering::Relaxed);
            let agents = progress.discovered_agents.load(Ordering::Relaxed);
            let is_rebuilding = progress.is_rebuilding.load(Ordering::Relaxed);

            let agent_names: Vec<String> = progress
                .discovered_agent_names
                .lock()
                .map(|names| names.clone())
                .unwrap_or_default();

            let phase_str = match phase {
                1 => "Scanning",
                2 => "Indexing",
                _ => "Preparing",
            };

            let rebuild_indicator = if is_rebuilding { " (rebuilding)" } else { "" };

            let msg = if phase == 1 {
                let scan_progress = if total > 0 {
                    format!("{current}/{total} connectors")
                } else {
                    "scanning connectors".to_string()
                };
                if agents > 0 {
                    let names_preview = if agent_names.len() <= 3 {
                        agent_names.join(", ")
                    } else {
                        format!(
                            "{}, ... +{} more",
                            agent_names[..3].join(", "),
                            agent_names.len() - 3
                        )
                    };
                    format!(
                        "{}{}: {} · {} agent(s): {}",
                        phase_str, rebuild_indicator, scan_progress, agents, names_preview
                    )
                } else {
                    format!(
                        "{}{}: {} · detecting agents...",
                        phase_str, rebuild_indicator, scan_progress
                    )
                }
            } else if phase == 2 {
                if total > 0 {
                    let pct = (current as f64 / total as f64 * 100.0).min(100.0);
                    format!(
                        "{}{}: {}/{} conversations ({:.0}%)",
                        phase_str, rebuild_indicator, current, total, pct
                    )
                } else {
                    format!("{}{}: Processing...", phase_str, rebuild_indicator)
                }
            } else {
                format!("{}{}...", phase_str, rebuild_indicator)
            };

            let now = Instant::now();
            let should_update = phase != last_phase
                || current != last_current
                || agents != last_agents
                || now.duration_since(last_update).as_millis() > 500;

            if should_update {
                pb.set_message(msg);
                last_phase = phase;
                last_current = current;
                last_agents = agents;
                last_update = now;
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        let total = progress.total.load(Ordering::Relaxed);
        let current = progress.current.load(Ordering::Relaxed);
        let agents = progress.discovered_agents.load(Ordering::Relaxed);
        pb.finish_with_message(format!(
            "Done: {} conversations from {} agent(s)",
            current.max(total),
            agents
        ));
    } else if show_plain {
        eprintln!("Starting index...");
        let mut last_phase = usize::MAX;
        let mut last_agents = 0;
        let mut last_current = 0;
        let mut last_scan_current = 0;

        loop {
            if handle.is_finished() {
                break;
            }

            let phase = progress.phase.load(Ordering::Relaxed);
            let total = progress.total.load(Ordering::Relaxed);
            let current = progress.current.load(Ordering::Relaxed);
            let agents = progress.discovered_agents.load(Ordering::Relaxed);

            if phase != last_phase {
                match phase {
                    1 => eprintln!("Scanning for agents..."),
                    2 => eprintln!("Indexing conversations..."),
                    _ => {}
                }
                last_phase = phase;
            }

            if phase == 1 && current != last_scan_current {
                if total > 0 {
                    eprintln!("  Scanned {}/{} connectors", current, total);
                } else {
                    eprintln!("  Scanned {} connectors", current);
                }
                last_scan_current = current;
            }

            if agents > last_agents {
                eprintln!("  Found {} agent(s)", agents);
                last_agents = agents;
            }

            if phase == 2 && current > last_current && current.is_multiple_of(100) {
                if total > 0 {
                    eprintln!("  Indexed {}/{} conversations", current, total);
                } else {
                    eprintln!("  Indexed {} conversations", current);
                }
                last_current = current;
            }

            std::thread::sleep(Duration::from_millis(200));
        }
    } else {
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    handle.join().map_err(|_| CliError {
        code: 9,
        kind: "doctor",
        message: "doctor worker thread panicked".to_string(),
        hint: None,
        retryable: true,
    })?
}

/// Comprehensive diagnostic and repair tool for cass installation.
/// CRITICAL: This function NEVER deletes user data. It only rebuilds derived data (index, db)
/// from source session files. This is essential because users may have only one copy of their
/// agent session data, and Codex/Claude Code auto-expire older logs.
#[allow(clippy::collapsible_if, clippy::collapsible_else_if)]
fn run_doctor(
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    fix: bool,
    verbose: bool,
    force_rebuild: bool,
) -> CliResult<()> {
    use colored::*;
    use std::time::Instant;

    let start = Instant::now();
    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    let index_path = crate::search::tantivy::index_dir(&data_dir).map_err(|e| CliError {
        code: 5,
        kind: "doctor",
        message: format!("failed to resolve index directory: {e}"),
        hint: None,
        retryable: true,
    })?;
    let lock_path = data_dir.join(".index.lock");

    // Track all checks and their results
    #[derive(serde::Serialize)]
    struct Check {
        name: String,
        status: String, // "pass", "warn", "fail"
        message: String,
        fix_available: bool,
        fix_applied: bool,
    }

    let mut checks: Vec<Check> = Vec::new();
    let mut needs_rebuild = force_rebuild;
    let mut db_ok = false;
    let mut db_conversations: Option<usize> = None;
    let mut db_messages: Option<usize> = None;
    let mut auto_fix_actions: Vec<String> = Vec::new();
    let mut auto_fix_applied = false;

    // Helper macro to add a check (avoids closure borrow issues)
    macro_rules! add_check {
        ($name:expr, $status:expr, $message:expr, $fix_available:expr) => {
            checks.push(Check {
                name: $name.to_string(),
                status: $status.to_string(),
                message: $message.to_string(),
                fix_available: $fix_available,
                fix_applied: false,
            });
        };
    }

    // 1. Check data directory exists and is writable
    if data_dir.exists() {
        if std::fs::metadata(&data_dir)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
        {
            add_check!(
                "data_directory",
                "pass",
                format!("Data directory exists: {}", data_dir.display()),
                false
            );
        } else {
            add_check!(
                "data_directory",
                "fail",
                format!("Data directory not writable: {}", data_dir.display()),
                false
            );
        }
    } else {
        if std::fs::create_dir_all(&data_dir).is_ok() {
            checks.push(Check {
                name: "data_directory".to_string(),
                status: "pass".to_string(),
                message: format!("Data directory created: {}", data_dir.display()),
                fix_available: true,
                fix_applied: true,
            });
            auto_fix_actions.push("Created missing data directory".to_string());
            auto_fix_applied = true;
        } else {
            add_check!(
                "data_directory",
                "fail",
                format!("Data directory missing: {}", data_dir.display()),
                true
            );
        }
    }

    // 2. Check for stale lock files
    if lock_path.exists() {
        // Check if lock is stale (older than 1 hour)
        let is_stale = std::fs::metadata(&lock_path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(true))
            .unwrap_or(true);

        if is_stale {
            if std::fs::remove_file(&lock_path).is_ok() {
                checks.push(Check {
                    name: "lock_file".to_string(),
                    status: "pass".to_string(),
                    message: "Stale lock file removed".to_string(),
                    fix_available: true,
                    fix_applied: true,
                });
                auto_fix_actions.push("Removed stale lock file".to_string());
                auto_fix_applied = true;
            } else {
                add_check!(
                    "lock_file",
                    "warn",
                    "Stale lock file found (older than 1 hour)",
                    true
                );
            }
        } else {
            add_check!(
                "lock_file",
                "warn",
                "Active lock file found - another process may be indexing",
                false
            );
        }
    } else {
        add_check!("lock_file", "pass", "No stale lock files", false);
    }

    // 3. Check database exists and is readable
    if db_path.exists() {
        match rusqlite::Connection::open(&db_path) {
            Ok(conn) => {
                let conv_count = conn
                    .query_row("SELECT COUNT(*) FROM conversations", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .ok();
                let msg_count = conn
                    .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
                    .ok();

                if let (Some(conv_count), Some(msg_count)) = (conv_count, msg_count) {
                    db_ok = true;
                    db_conversations = Some(conv_count.max(0) as usize);
                    db_messages = Some(msg_count.max(0) as usize);
                    add_check!(
                        "database",
                        "pass",
                        format!(
                            "Database OK ({} conversations, {} messages)",
                            conv_count, msg_count
                        ),
                        false
                    );

                    // Check for FTS table (fts_messages) - this can be missing if table was
                    // dropped after migrations completed
                    let fts_exists = conn
                        .query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fts_messages'",
                            [],
                            |r| r.get::<_, i64>(0),
                        )
                        .unwrap_or(0)
                        > 0;

                    if fts_exists {
                        add_check!(
                            "fts_table",
                            "pass",
                            "FTS search table (fts_messages) exists",
                            false
                        );
                    } else {
                        // FTS table missing - attempt to recreate it if --fix is set
                        if fix {
                            let create_result = conn.execute_batch(
                                r#"
                                CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
                                    content,
                                    title,
                                    agent,
                                    workspace,
                                    source_path,
                                    created_at UNINDEXED,
                                    message_id UNINDEXED,
                                    tokenize='porter'
                                );
                                INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
                                SELECT m.content, m.title, a.name, w.name, m.source_path, m.created_at, m.id
                                FROM messages m
                                LEFT JOIN agents a ON m.agent_id = a.id
                                LEFT JOIN workspaces w ON m.workspace_id = w.id
                                WHERE m.content IS NOT NULL;
                                "#,
                            );
                            match create_result {
                                Ok(_) => {
                                    checks.push(Check {
                                        name: "fts_table".to_string(),
                                        status: "pass".to_string(),
                                        message: "FTS search table (fts_messages) recreated"
                                            .to_string(),
                                        fix_available: true,
                                        fix_applied: true,
                                    });
                                    auto_fix_actions
                                        .push("Recreated missing FTS search table".to_string());
                                    auto_fix_applied = true;
                                }
                                Err(e) => {
                                    add_check!(
                                        "fts_table",
                                        "fail",
                                        format!("FTS table missing and recreation failed: {}", e),
                                        true
                                    );
                                    needs_rebuild = true;
                                }
                            }
                        } else {
                            add_check!(
                                "fts_table",
                                "fail",
                                "FTS search table (fts_messages) missing - run with --fix to recreate",
                                true
                            );
                            needs_rebuild = true;
                        }
                    }
                } else {
                    add_check!("database", "fail", "Database query failed", true);
                    needs_rebuild = true;
                }
            }
            Err(e) => {
                add_check!(
                    "database",
                    "fail",
                    format!("Cannot open database: {}", e),
                    true
                );
                needs_rebuild = true;
            }
        }
    } else {
        add_check!("database", "fail", "Database not found", true);
        needs_rebuild = true;
    }

    // 4. Check Tantivy index exists and is readable
    if index_path.join("meta.json").exists() {
        match tantivy::Index::open_in_dir(&index_path) {
            Ok(index) => {
                match index.reader() {
                    Ok(reader) => {
                        let searcher = reader.searcher();
                        let num_docs = searcher.num_docs();
                        add_check!(
                            "index",
                            "pass",
                            format!("Search index OK ({} documents)", num_docs),
                            false
                        );

                        // Check if index is empty but database has data
                        if num_docs == 0 && db_ok {
                            if let Ok(conn) = rusqlite::Connection::open(&db_path) {
                                if let Ok(msg_count) =
                                    conn.query_row("SELECT COUNT(*) FROM messages", [], |r| {
                                        r.get::<_, i64>(0)
                                    })
                                {
                                    if msg_count > 0 {
                                        add_check!(
                                            "index_sync",
                                            "warn",
                                            format!(
                                                "Index is empty but database has {} messages",
                                                msg_count
                                            ),
                                            true
                                        );
                                        needs_rebuild = true;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        add_check!("index", "fail", format!("Cannot read index: {}", e), true);
                        needs_rebuild = true;
                    }
                }
            }
            Err(e) => {
                add_check!("index", "fail", format!("Cannot open index: {}", e), true);
                needs_rebuild = true;
            }
        }
    } else {
        add_check!("index", "fail", "Search index not found", true);
        needs_rebuild = true;
    }

    // 5. Check config file
    let config_path = data_dir.join("config.toml");
    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match toml::from_str::<toml::Value>(&content) {
                Ok(_) => {
                    add_check!("config", "pass", "Config file valid", false);
                }
                Err(e) => {
                    add_check!(
                        "config",
                        "warn",
                        format!("Config parse error: {}", e),
                        false
                    );
                }
            },
            Err(e) => {
                add_check!(
                    "config",
                    "warn",
                    format!("Cannot read config: {}", e),
                    false
                );
            }
        }
    } else {
        add_check!("config", "pass", "No config file (using defaults)", false);
    }

    // 6. Check sources.toml
    let sources_path = dirs::config_dir()
        .unwrap_or_else(|| data_dir.clone())
        .join("cass")
        .join("sources.toml");
    if sources_path.exists() {
        match std::fs::read_to_string(&sources_path) {
            Ok(content) => match toml::from_str::<toml::Value>(&content) {
                Ok(_) => {
                    add_check!("sources_config", "pass", "Sources config valid", false);
                }
                Err(e) => {
                    add_check!(
                        "sources_config",
                        "warn",
                        format!("Sources config parse error: {}", e),
                        false
                    );
                }
            },
            Err(e) => {
                add_check!(
                    "sources_config",
                    "warn",
                    format!("Cannot read sources config: {}", e),
                    false
                );
            }
        }
    } else {
        add_check!(
            "sources_config",
            "pass",
            "No remote sources configured",
            false
        );
    }

    // 7. Check common session directories exist
    let mut session_dirs_found = 0usize;
    let home = dirs::home_dir().unwrap_or_default();
    let session_paths = [
        home.join(".claude"),        // Claude Code
        home.join(".codex"),         // Codex
        home.join(".cursor"),        // Cursor
        home.join(".aider"),         // Aider
        home.join(".chatgpt"),       // ChatGPT
        home.join(".config/gemini"), // Gemini
    ];
    for path in &session_paths {
        if path.exists() {
            session_dirs_found += 1;
        }
    }
    if session_dirs_found > 0 {
        add_check!(
            "sessions",
            "pass",
            format!("Found {} agent session directories", session_dirs_found),
            false
        );
    } else {
        add_check!(
            "sessions",
            "warn",
            "No agent session directories found",
            false
        );
    }

    // Apply fix: rebuild index if needed
    if needs_rebuild {
        let stderr_is_tty = std::io::stderr().is_terminal();
        let show_progress = !json && stderr_is_tty;
        let show_plain = !json && !stderr_is_tty;

        if !json {
            println!();
            if fix {
                println!(
                    "{} Rebuilding index (this may take a moment)...",
                    "→".cyan()
                );
            } else {
                println!(
                    "{} Auto-repair: rebuilding index (this may take a moment)...",
                    "→".cyan()
                );
            }
        }

        let progress = std::sync::Arc::new(indexer::IndexingProgress::default());
        let rebuild_from_db = db_ok && db_messages.unwrap_or(0) > 0;

        if rebuild_from_db {
            let total_convs = db_conversations.unwrap_or(0);
            let rebuild_handle = std::thread::spawn({
                let progress = progress.clone();
                let db_path = db_path.clone();
                let data_dir = data_dir.clone();
                move || rebuild_tantivy_from_db(&db_path, &data_dir, total_convs, Some(progress))
            });

            let rebuild_result = wait_with_progress(
                rebuild_handle,
                progress,
                show_progress,
                show_plain,
                "Rebuilding search index from database...",
            );

            match rebuild_result {
                Ok(message_count) => {
                    needs_rebuild = false;
                    auto_fix_actions.push("Rebuilt search index from database".to_string());
                    auto_fix_applied = true;
                    for check in &mut checks {
                        if check.name == "index" || check.name == "index_sync" {
                            check.status = "pass".to_string();
                            check.fix_applied = true;
                            check.message = "Search index rebuilt from database".to_string();
                        }
                    }
                    checks.push(Check {
                        name: "rebuild".to_string(),
                        status: "pass".to_string(),
                        message: format!(
                            "Index rebuilt from database ({} messages)",
                            message_count
                        ),
                        fix_available: true,
                        fix_applied: true,
                    });
                }
                Err(e) => {
                    checks.push(Check {
                        name: "rebuild".to_string(),
                        status: "fail".to_string(),
                        message: format!("Index rebuild failed: {}", e),
                        fix_available: true,
                        fix_applied: false,
                    });
                }
            }
        } else {
            // Preserve existing DB when possible; rebuild only derived data.
            let mut can_rebuild = true;
            let mut db_backup_done = false;
            if db_path.exists() && !db_ok {
                let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                let backup_path = db_path.with_extension(format!("corrupt.{ts}"));
                match std::fs::rename(&db_path, &backup_path) {
                    Ok(_) => {
                        db_backup_done = true;
                        checks.push(Check {
                            name: "database_backup".to_string(),
                            status: "pass".to_string(),
                            message: format!(
                                "Backed up corrupted database to {}",
                                backup_path.display()
                            ),
                            fix_available: true,
                            fix_applied: true,
                        });
                        auto_fix_actions.push(format!(
                            "Backed up corrupted database to {}",
                            backup_path.display()
                        ));
                        auto_fix_applied = true;
                    }
                    Err(e) => {
                        checks.push(Check {
                            name: "database_backup".to_string(),
                            status: "fail".to_string(),
                            message: format!("Failed to backup corrupted database: {}", e),
                            fix_available: true,
                            fix_applied: false,
                        });
                        can_rebuild = false;
                    }
                }
            }

            if !can_rebuild {
                checks.push(Check {
                    name: "rebuild".to_string(),
                    status: "fail".to_string(),
                    message: "Index rebuild skipped because database backup failed".to_string(),
                    fix_available: true,
                    fix_applied: false,
                });
                needs_rebuild = true;
            } else {
                let index_opts = indexer::IndexOptions {
                    full: false,
                    force_rebuild,
                    watch: false,
                    watch_once_paths: None,
                    db_path: db_path.clone(),
                    data_dir: data_dir.clone(),
                    semantic: false,
                    build_hnsw: false,
                    embedder: "fastembed".to_string(),
                    progress: Some(progress.clone()),
                };

                let rebuild_handle = std::thread::spawn(move || {
                    indexer::run_index(index_opts, None)
                        .map(|_| 0usize)
                        .map_err(|e| CliError {
                            code: 5,
                            kind: "doctor",
                            message: format!("index rebuild failed: {e}"),
                            hint: None,
                            retryable: true,
                        })
                });

                let rebuild_result = wait_with_progress(
                    rebuild_handle,
                    progress,
                    show_progress,
                    show_plain,
                    "Rebuilding index from source sessions...",
                );

                match rebuild_result {
                    Ok(_) => {
                        needs_rebuild = false;
                        let rebuild_note = if db_backup_done {
                            "Rebuilt index from source sessions (new database created)".to_string()
                        } else {
                            "Rebuilt index from source sessions (database preserved)".to_string()
                        };
                        auto_fix_actions.push(rebuild_note.clone());
                        auto_fix_applied = true;
                        for check in &mut checks {
                            if check.name == "index" || check.name == "index_sync" {
                                check.status = "pass".to_string();
                                check.fix_applied = true;
                                check.message = rebuild_note.clone();
                            }
                        }
                        checks.push(Check {
                            name: "rebuild".to_string(),
                            status: "pass".to_string(),
                            message: "Index rebuilt successfully".to_string(),
                            fix_available: true,
                            fix_applied: true,
                        });
                    }
                    Err(e) => {
                        checks.push(Check {
                            name: "rebuild".to_string(),
                            status: "fail".to_string(),
                            message: format!("Index rebuild failed: {}", e),
                            fix_available: true,
                            fix_applied: false,
                        });
                    }
                }
            }
        }
    }

    // Count issues
    let fail_count = checks.iter().filter(|c| c.status == "fail").count();
    let warn_count = checks.iter().filter(|c| c.status == "warn").count();
    let issues_found = fail_count + warn_count;
    let issues_fixed = checks.iter().filter(|c| c.fix_applied).count();

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let all_pass = checks.iter().all(|c| c.status == "pass");

    // Output
    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let payload = serde_json::json!({
            "healthy": fail_count == 0,
            "issues_found": issues_found,
            "issues_fixed": issues_fixed,
            "failures": fail_count,
            "warnings": warn_count,
            "needs_rebuild": needs_rebuild,
            "auto_fix_applied": auto_fix_applied,
            "auto_fix_actions": auto_fix_actions,
            "checks": checks,
            "_meta": {
                "elapsed_ms": elapsed_ms,
                "data_dir": data_dir.display().to_string(),
                "db_path": db_path.display().to_string(),
                "fix_mode": fix,
            }
        });
        output_structured_value(payload, fmt)?;
    } else {
        // Human-readable output
        println!("{}", "CASS Doctor".bold());
        println!();

        for check in &checks {
            let icon = match check.status.as_str() {
                "pass" => "✓".green(),
                "warn" => "⚠".yellow(),
                "fail" => "✗".red(),
                _ => "?".normal(),
            };

            // Show passed checks only in verbose mode
            if check.status == "pass" && !verbose {
                continue;
            }

            let fix_indicator = if check.fix_applied {
                " [fixed]".green().to_string()
            } else if check.fix_available && !fix {
                " [fixable]".yellow().to_string()
            } else {
                String::new()
            };

            println!(
                "{} {}: {}{}",
                icon,
                check.name.bold(),
                check.message,
                fix_indicator
            );
        }

        println!();
        if all_pass {
            println!("{} All checks passed ({elapsed_ms}ms)", "✓".green());
        } else {
            let summary_icon = if fail_count > 0 {
                "✗".red()
            } else {
                "⚠".yellow()
            };
            println!(
                "{} {} failure(s), {} warning(s), {} fixed ({elapsed_ms}ms)",
                summary_icon, fail_count, warn_count, issues_fixed
            );

            if auto_fix_applied && !auto_fix_actions.is_empty() {
                println!();
                println!("{}", "Auto-repair actions:".bold());
                for action in &auto_fix_actions {
                    println!("  - {action}");
                }
            }

            if needs_rebuild {
                println!();
                println!("{}", "Recommended action:".bold());
                println!("  cass index --full     # Rebuild from source sessions");
                println!();
                println!("{}", "Note: Your source session files are SAFE. Only derived data (index/db) will be rebuilt.".dimmed());
            }
        }
    }

    if fail_count == 0 {
        Ok(())
    } else {
        Err(CliError {
            code: 5, // Data corruption code
            kind: "doctor",
            message: format!("{} failure(s) remain", fail_count),
            hint: Some(
                "Automatic safe repairs were attempted. Run 'cass index --full' to rebuild from source sessions or check cass.log for details."
                    .to_string(),
            ),
            retryable: true,
        })
    }
}

/// Find related sessions for a given source path.
/// Returns sessions that share the same workspace, same day, or same agent.
fn run_context(
    path: &Path,
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    limit: usize,
) -> CliResult<()> {
    let lazy = crate::storage::sqlite::LazyDb::from_overrides(data_dir_override, db_override);
    let conn = lazy.get("context").map_err(lazy_db_to_cli_error)?;

    // Find the source conversation by path (normalized to string)
    let path_str = path.to_string_lossy().to_string();
    #[allow(clippy::type_complexity)]
    let source_conv: Option<(i64, i64, Option<i64>, Option<i64>, String, String)> = conn
        .query_row(
            "SELECT c.id, c.agent_id, c.workspace_id, c.started_at, c.title, a.slug
             FROM conversations c
             JOIN agents a ON c.agent_id = a.id
             WHERE c.source_path = ?1",
            [&path_str],
            |r: &rusqlite::Row| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    r.get(5)?,
                ))
            },
        )
        .ok();

    let Some((conv_id, agent_id, workspace_id, started_at, title, agent_slug)) = source_conv else {
        return Err(CliError {
            code: 4,
            kind: "not_found",
            message: format!("No session found at path: {path_str}"),
            hint: Some(
                "Use 'cass search' to find sessions, then use the source_path from results."
                    .to_string(),
            ),
            retryable: false,
        });
    };

    // Get workspace path for display
    let workspace_path: Option<String> = workspace_id.and_then(|ws_id: i64| {
        conn.query_row(
            "SELECT path FROM workspaces WHERE id = ?1",
            [ws_id],
            |r: &rusqlite::Row| r.get::<_, String>(0),
        )
        .ok()
    });

    // Find related sessions: same workspace (excluding self)
    let same_workspace: Vec<(String, String, String, Option<i64>)> =
        if let Some(ws_id) = workspace_id {
            let mut stmt = conn
                .prepare(
                    "SELECT c.source_path, c.title, a.slug, c.started_at
                 FROM conversations c
                 JOIN agents a ON c.agent_id = a.id
                 WHERE c.workspace_id = ?1 AND c.id != ?2
                 ORDER BY c.started_at DESC
                 LIMIT ?3",
                )
                .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
            stmt.query_map([ws_id, conv_id, limit as i64], |r: &rusqlite::Row| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get(2)?,
                    r.get(3)?,
                ))
            })
            .map_err(|e| CliError::unknown(format!("query: {e}")))?
            .filter_map(std::result::Result::ok)
            .collect()
        } else {
            Vec::new()
        };

    // Find related sessions: same day (within 24 hours of started_at)
    let same_day: Vec<(String, String, String, Option<i64>)> = if let Some(ts) = started_at {
        let day_start = ts - (ts % 86_400_000); // Start of day in milliseconds
        let day_end = day_start + 86_400_000;
        let mut stmt = conn
            .prepare(
                "SELECT c.source_path, c.title, a.slug, c.started_at
                 FROM conversations c
                 JOIN agents a ON c.agent_id = a.id
                 WHERE c.started_at >= ?1 AND c.started_at < ?2 AND c.id != ?3
                 ORDER BY c.started_at DESC
                 LIMIT ?4",
            )
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        stmt.query_map(
            [day_start, day_end, conv_id, limit as i64],
            |r: &rusqlite::Row| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get(2)?,
                    r.get(3)?,
                ))
            },
        )
        .map_err(|e| CliError::unknown(format!("query: {e}")))?
        .filter_map(std::result::Result::ok)
        .collect()
    } else {
        Vec::new()
    };

    // Find related sessions: same agent (excluding self)
    let same_agent: Vec<(String, String, Option<i64>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT c.source_path, c.title, c.started_at
                 FROM conversations c
                 WHERE c.agent_id = ?1 AND c.id != ?2
                 ORDER BY c.started_at DESC
                 LIMIT ?3",
            )
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
        stmt.query_map([agent_id, conv_id, limit as i64], |r: &rusqlite::Row| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get(2)?,
            ))
        })
        .map_err(|e| CliError::unknown(format!("query: {e}")))?
        .filter_map(std::result::Result::ok)
        .collect()
    };

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let format_ts = |ts: Option<i64>| -> Option<String> {
            ts.and_then(|t| chrono::DateTime::from_timestamp_millis(t).map(|d| d.to_rfc3339()))
        };

        let payload = serde_json::json!({
            "source": {
                "path": path_str,
                "title": title,
                "agent": agent_slug,
                "workspace": workspace_path,
                "started_at": format_ts(started_at),
            },
            "related": {
                "same_workspace": same_workspace.iter().map(|(p, t, a, ts)| {
                    serde_json::json!({
                        "path": p,
                        "title": t,
                        "agent": a,
                        "started_at": format_ts(*ts),
                    })
                }).collect::<Vec<_>>(),
                "same_day": same_day.iter().map(|(p, t, a, ts)| {
                    serde_json::json!({
                        "path": p,
                        "title": t,
                        "agent": a,
                        "started_at": format_ts(*ts),
                    })
                }).collect::<Vec<_>>(),
                "same_agent": same_agent.iter().map(|(p, t, ts)| {
                    serde_json::json!({
                        "path": p,
                        "title": t,
                        "started_at": format_ts(*ts),
                    })
                }).collect::<Vec<_>>(),
            },
            "counts": {
                "same_workspace": same_workspace.len(),
                "same_day": same_day.len(),
                "same_agent": same_agent.len(),
            }
        });
        return output_structured_value(payload, fmt);
    }

    use colored::Colorize;

    println!("{}", "Session Context".bold().cyan());
    println!("{}", "===============".cyan());
    println!();
    println!("{}: {}", "Source".bold(), path_str);
    println!("  Title: {}", title.as_str().yellow());
    println!("  Agent: {}", agent_slug.as_str().green());
    if let Some(ws) = &workspace_path {
        println!("  Workspace: {}", ws.as_str().blue());
    }
    if let Some(ts) = started_at
        && let Some(dt) = chrono::DateTime::from_timestamp_millis(ts)
    {
        println!("  Started: {}", dt.format("%Y-%m-%d %H:%M:%S"));
    }
    println!();

    if !same_workspace.is_empty() {
        println!(
            "{} ({}):",
            "Same Workspace".bold().blue(),
            same_workspace.len()
        );
        for (path, title_str, agent, timestamp) in &same_workspace {
            let ts_str = timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            println!(
                "  • {} [{}] {}",
                title_str.as_str().yellow(),
                agent.as_str().green(),
                ts_str.dimmed()
            );
            println!("    {}", path.as_str().dimmed());
        }
        println!();
    }

    if !same_day.is_empty() {
        println!("{} ({}):", "Same Day".bold().magenta(), same_day.len());
        for (path, title_str, agent, timestamp) in &same_day {
            let ts_str = timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|d| d.format("%H:%M").to_string())
                .unwrap_or_default();
            println!(
                "  • {} [{}] {}",
                title_str.as_str().yellow(),
                agent.as_str().green(),
                ts_str.dimmed()
            );
            println!("    {}", path.as_str().dimmed());
        }
        println!();
    }

    if !same_agent.is_empty() {
        println!("{} ({}):", "Same Agent".bold().green(), same_agent.len());
        for (path, title_str, timestamp) in &same_agent {
            let ts_str = timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            println!("  • {} {}", title_str.as_str().yellow(), ts_str.dimmed());
            println!("    {}", path.as_str().dimmed());
        }
        println!();
    }

    if same_workspace.is_empty() && same_day.is_empty() && same_agent.is_empty() {
        println!("{}", "No related sessions found.".dimmed());
    }

    Ok(())
}

/// Capabilities response for agent introspection.
/// Provides static information about CLI features, versions, and limits.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilitiesResponse {
    /// Semantic version of the crate
    pub crate_version: String,
    /// API contract version (bumped on breaking changes)
    pub api_version: u32,
    /// Human-readable contract identifier
    pub contract_version: String,
    /// List of supported feature flags
    pub features: Vec<String>,
    /// List of supported agent connectors
    pub connectors: Vec<String>,
    /// System limits
    pub limits: CapabilitiesLimits,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilitiesLimits {
    /// Maximum --limit value
    pub max_limit: usize,
    /// Maximum --max-content-length value (0 = unlimited)
    pub max_content_length: usize,
    /// Maximum fields in --fields selection
    pub max_fields: usize,
    /// Maximum aggregation bucket count per field
    pub max_agg_buckets: usize,
}

// ============================================================================
// Introspect command schema structures
// ============================================================================

/// Full API introspection response
#[derive(Debug, Clone, Serialize)]
pub struct IntrospectResponse {
    /// API version (matches capabilities)
    pub api_version: u32,
    /// Contract version (human-visible)
    pub contract_version: String,
    /// Global flags (apply to all commands)
    pub global_flags: Vec<ArgumentSchema>,
    /// All available commands with arguments
    pub commands: Vec<CommandSchema>,
    /// Response schemas for JSON outputs
    pub response_schemas: std::collections::HashMap<String, serde_json::Value>,
}

/// Schema for a single CLI command
#[derive(Debug, Clone, Serialize)]
pub struct CommandSchema {
    /// Command name (e.g., "search", "status")
    pub name: String,
    /// Short description
    pub description: String,
    /// Arguments and options
    pub arguments: Vec<ArgumentSchema>,
    /// Whether this command supports --json output
    pub has_json_output: bool,
}

/// Schema for a command argument/option
#[derive(Debug, Clone, Serialize)]
pub struct ArgumentSchema {
    /// Argument name (e.g., "query", "limit", "json")
    pub name: String,
    /// Short flag (e.g., 'n' for -n)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<char>,
    /// Description
    pub description: String,
    /// Type: "flag", "option", "positional"
    pub arg_type: String,
    /// Value type: "string", "integer", "path", "boolean", "enum"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    /// Whether required
    pub required: bool,
    /// Default value if any
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Enum values if `value_type` is "enum"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    /// Whether option can be repeated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeatable: Option<bool>,
}

/// Global flags that apply to all commands
fn build_global_flag_schemas() -> Vec<ArgumentSchema> {
    vec![
        ArgumentSchema {
            name: "db".to_string(),
            short: None,
            description: "Path to the SQLite database (defaults to platform data dir)".to_string(),
            arg_type: "option".to_string(),
            value_type: Some("path".to_string()),
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
        ArgumentSchema {
            name: "robot-help".to_string(),
            short: None,
            description: "Deterministic machine-first help (no TUI)".to_string(),
            arg_type: "flag".to_string(),
            value_type: None,
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
        ArgumentSchema {
            name: "trace-file".to_string(),
            short: None,
            description: "Trace command execution spans to JSONL file".to_string(),
            arg_type: "option".to_string(),
            value_type: Some("path".to_string()),
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
        ArgumentSchema {
            name: "quiet".to_string(),
            short: Some('q'),
            description: "Reduce log noise (warnings and errors only)".to_string(),
            arg_type: "flag".to_string(),
            value_type: None,
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
        ArgumentSchema {
            name: "verbose".to_string(),
            short: Some('v'),
            description: "Increase verbosity (debug information)".to_string(),
            arg_type: "flag".to_string(),
            value_type: None,
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
        ArgumentSchema {
            name: "color".to_string(),
            short: None,
            description: "Color behavior for CLI output".to_string(),
            arg_type: "option".to_string(),
            value_type: Some("enum".to_string()),
            required: false,
            default: Some("auto".to_string()),
            enum_values: Some(vec![
                "auto".to_string(),
                "never".to_string(),
                "always".to_string(),
            ]),
            repeatable: None,
        },
        ArgumentSchema {
            name: "progress".to_string(),
            short: None,
            description: "Progress output style".to_string(),
            arg_type: "option".to_string(),
            value_type: Some("enum".to_string()),
            required: false,
            default: Some("auto".to_string()),
            enum_values: Some(vec![
                "auto".to_string(),
                "bars".to_string(),
                "plain".to_string(),
                "none".to_string(),
            ]),
            repeatable: None,
        },
        ArgumentSchema {
            name: "wrap".to_string(),
            short: None,
            description: "Wrap informational output to N columns".to_string(),
            arg_type: "option".to_string(),
            value_type: Some("integer".to_string()),
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
        ArgumentSchema {
            name: "nowrap".to_string(),
            short: None,
            description: "Disable wrapping entirely".to_string(),
            arg_type: "flag".to_string(),
            value_type: None,
            required: false,
            default: None,
            enum_values: None,
            repeatable: None,
        },
    ]
}

/// Discover available features, versions, and limits for agent introspection.
fn run_capabilities(json: bool) -> CliResult<()> {
    let response = CapabilitiesResponse {
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: 1,
        contract_version: CONTRACT_VERSION.to_string(),
        features: vec![
            "json_output".to_string(),
            "jsonl_output".to_string(),
            "robot_meta".to_string(),
            "time_filters".to_string(),
            "field_selection".to_string(),
            "content_truncation".to_string(),
            "aggregations".to_string(),
            "wildcard_fallback".to_string(),
            "timeout".to_string(),
            "cursor_pagination".to_string(),
            "request_id".to_string(),
            "dry_run".to_string(),
            "query_explain".to_string(),
            "view_command".to_string(),
            "status_command".to_string(),
            "state_command".to_string(),
            "api_version_command".to_string(),
            "introspect_command".to_string(),
            "export_command".to_string(),
            "expand_command".to_string(),
            "timeline_command".to_string(),
            "highlight_matches".to_string(),
        ],
        connectors: capabilities_connector_names(),
        limits: CapabilitiesLimits {
            max_limit: 10000,
            max_content_length: 0, // 0 = unlimited
            max_fields: 50,
            max_agg_buckets: 10,
        },
    };

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        // sessions is search-only; for other commands treat it as compact JSON.
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let payload = serde_json::to_value(&response).unwrap_or_default();
        return output_structured_value(payload, fmt);
    }

    // Human-readable output
    println!("CASS Capabilities");
    println!("=================");
    println!();
    println!(
        "Version: {} (api v{}, contract v{})",
        response.crate_version, response.api_version, response.contract_version
    );
    println!();
    println!("Features:");
    for feature in &response.features {
        println!("  - {feature}");
    }
    println!();
    println!("Connectors:");
    for connector in &response.connectors {
        println!("  - {connector}");
    }
    println!();
    println!("Limits:");
    println!("  max_limit: {}", response.limits.max_limit);
    println!(
        "  max_content_length: {} (0 = unlimited)",
        response.limits.max_content_length
    );
    println!("  max_fields: {}", response.limits.max_fields);
    println!("  max_agg_buckets: {}", response.limits.max_agg_buckets);

    Ok(())
}

/// Full API schema introspection - commands, arguments, and response schemas.
fn run_introspect(json: bool) -> CliResult<()> {
    let global_flags = build_global_flag_schemas();
    let commands = build_command_schemas();
    let response_schemas = build_response_schemas();

    let response = IntrospectResponse {
        api_version: 1,
        contract_version: CONTRACT_VERSION.to_string(),
        global_flags,
        commands,
        response_schemas,
    };

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let payload = serde_json::to_value(&response).unwrap_or_default();
        return output_structured_value(payload, fmt);
    }

    // Human-readable output
    println!("CASS API Introspection");
    println!("======================");
    println!();
    println!("API Version: {}", response.api_version);
    println!("Contract Version: {}", response.contract_version);
    println!();
    println!("Global Flags:");
    println!("-------------");
    for flag in &response.global_flags {
        let required = if flag.required { " (required)" } else { "" };
        let default = flag
            .default
            .as_ref()
            .map(|d| format!(" [default: {d}]"))
            .unwrap_or_default();
        let enum_values = flag
            .enum_values
            .as_ref()
            .map(|vals| format!(" [values: {}]", vals.join(",")))
            .unwrap_or_default();
        let short = flag.short.map(|s| format!("-{s}, ")).unwrap_or_default();
        let prefix = if flag.arg_type == "positional" {
            String::new()
        } else {
            format!("{short}--")
        };
        println!(
            "  {}{}: {}{}{}{}",
            prefix, flag.name, flag.description, required, default, enum_values
        );
    }
    println!();
    println!("Commands:");
    println!("---------");
    for cmd in &response.commands {
        println!();
        println!("  {} - {}", cmd.name, cmd.description);
        if cmd.has_json_output {
            println!("    [supports --json output]");
        }
        if !cmd.arguments.is_empty() {
            println!("    Arguments:");
            for arg in &cmd.arguments {
                let required = if arg.required { " (required)" } else { "" };
                let default = arg
                    .default
                    .as_ref()
                    .map(|d| format!(" [default: {d}]"))
                    .unwrap_or_default();
                let short = arg.short.map(|s| format!("-{s}, ")).unwrap_or_default();
                let prefix = if arg.arg_type == "positional" {
                    String::new()
                } else {
                    format!("{short}--")
                };
                println!(
                    "      {}{}: {}{}{}",
                    prefix, arg.name, arg.description, required, default
                );
            }
        }
    }
    println!();
    println!(
        "Response Schemas: {} defined",
        response.response_schemas.len()
    );
    for name in response.response_schemas.keys() {
        println!("  - {name}");
    }

    Ok(())
}

/// Run export based on JSON config file.
fn run_config_based_export(
    config: &crate::pages::config_input::PagesConfig,
    wizard_state: &crate::pages::wizard::WizardState,
    db_path: &std::path::Path,
    dry_run: bool,
    json_output: bool,
    _verbose: bool,
) -> anyhow::Result<()> {
    use chrono::DateTime;
    use rand::RngCore;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    if dry_run {
        if json_output {
            let result = serde_json::json!({
                "status": "dry_run",
                "output_dir": wizard_state.output_dir,
                "config_valid": true,
            });
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Dry run: would export to {:?}", wizard_state.output_dir);
        }
        return Ok(());
    }

    // Output directory is the bundle root (contains site/ and private/)
    let output_dir = &wizard_state.output_dir;
    std::fs::create_dir_all(output_dir)?;

    // Create temp directory for intermediate export and encryption output
    let temp_dir = tempfile::tempdir()?;
    let export_db_path = temp_dir.path().join("export.db");
    let encrypted_dir = temp_dir.path().join("encrypted");
    std::fs::create_dir_all(&encrypted_dir)?;

    // Parse time filters to DateTime<Utc>
    let since_dt = config.since_ts().and_then(DateTime::from_timestamp_millis);
    let until_dt = config.until_ts().and_then(DateTime::from_timestamp_millis);

    // Build export filter
    let filter = crate::pages::export::ExportFilter {
        agents: if wizard_state.agents.is_empty() {
            None
        } else {
            Some(wizard_state.agents.clone())
        },
        workspaces: wizard_state.workspaces.clone(),
        since: since_dt,
        until: until_dt,
        path_mode: config.path_mode(),
    };

    // Run export
    let export_engine = crate::pages::export::ExportEngine::new(db_path, &export_db_path, filter);

    let running = Arc::new(AtomicBool::new(true));
    let stats = export_engine.execute(|_current, _total| {}, Some(running))?;

    let mut recovery_secret: Option<Vec<u8>> = None;
    let encryption_enabled = !wizard_state.no_encryption;

    if encryption_enabled {
        let password = wizard_state
            .password
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Encryption enabled but no password provided"))?;
        let chunk_size = config.encryption.chunk_size.unwrap_or(8 * 1024 * 1024) as usize;
        let mut enc_engine = crate::pages::encrypt::EncryptionEngine::new(chunk_size);
        enc_engine.add_password_slot(password)?;

        // Add recovery slot if requested
        if wizard_state.generate_recovery {
            let mut recovery_bytes = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut recovery_bytes);
            enc_engine.add_recovery_slot(&recovery_bytes)?;
            recovery_secret = Some(recovery_bytes.to_vec());
        }

        // Encrypt the database into the temp encrypted dir
        let enc_config = enc_engine.encrypt_file(&export_db_path, &encrypted_dir, |_, _| {})?;

        // Write config.json
        let config_path = encrypted_dir.join("config.json");
        std::fs::write(&config_path, serde_json::to_string_pretty(&enc_config)?)?;
    } else {
        if !wizard_state.unencrypted_confirmed {
            anyhow::bail!(
                "Unencrypted export not confirmed. Set encryption.i_understand_risks: true."
            );
        }

        let payload_dir = encrypted_dir.join("payload");
        std::fs::create_dir_all(&payload_dir)?;
        let dest_db = payload_dir.join("data.db");
        std::fs::copy(&export_db_path, &dest_db)?;

        let db_size = std::fs::metadata(&dest_db).map(|m| m.len()).unwrap_or(0);

        let unencrypted_config = crate::pages::archive_config::UnencryptedConfig {
            encrypted: false,
            version: "1.0.0".to_string(),
            payload: crate::pages::archive_config::UnencryptedPayload {
                path: "payload/data.db".to_string(),
                format: "sqlite".to_string(),
                size_bytes: Some(db_size),
            },
            warning: Some("UNENCRYPTED - All content is publicly readable".to_string()),
        };

        let config_path = encrypted_dir.join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&unencrypted_config)?,
        )?;
    }

    // Build bundle
    let bundle_config = crate::pages::bundle::BundleConfig {
        title: wizard_state.title.clone(),
        description: wizard_state.description.clone(),
        hide_metadata: wizard_state.hide_metadata,
        recovery_secret,
        generate_qr: wizard_state.generate_qr,
        generated_docs: vec![],
    };

    let bundle_builder = crate::pages::bundle::BundleBuilder::with_config(bundle_config);
    let bundle_result = bundle_builder.build(&encrypted_dir, output_dir, |_phase, _msg| {})?;

    // Optional deployment
    let deploy_result = match wizard_state.target {
        crate::pages::wizard::DeployTarget::Local => None,
        crate::pages::wizard::DeployTarget::GitHubPages => {
            let repo = wizard_state
                .repo_name
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("GitHub deployment requires deployment.repo"))?;
            let deployer = crate::pages::deploy_github::GitHubDeployer::new(repo.clone());
            Some(serde_json::to_value(
                deployer.deploy(&bundle_result.site_dir, |_phase, _msg| {})?,
            )?)
        }
        crate::pages::wizard::DeployTarget::CloudflarePages => {
            let project_name = wizard_state
                .repo_name
                .clone()
                .unwrap_or_else(|| "cass-archive".to_string());
            let branch = wizard_state
                .cloudflare_branch
                .clone()
                .unwrap_or_else(|| "main".to_string());
            let account_id = wizard_state
                .cloudflare_account_id
                .clone()
                .or_else(|| dotenvy::var("CLOUDFLARE_ACCOUNT_ID").ok());
            let api_token = wizard_state
                .cloudflare_api_token
                .clone()
                .or_else(|| dotenvy::var("CLOUDFLARE_API_TOKEN").ok());
            let deployer = crate::pages::deploy_cloudflare::CloudflareDeployer::new(
                crate::pages::deploy_cloudflare::CloudflareConfig {
                    project_name: project_name.clone(),
                    custom_domain: None,
                    create_if_missing: true,
                    branch,
                    account_id,
                    api_token,
                },
            );
            Some(serde_json::to_value(
                deployer.deploy(&bundle_result.site_dir, |_phase, _msg| {})?,
            )?)
        }
    };

    // Output results
    if json_output {
        let result = serde_json::json!({
            "status": "success",
            "output_dir": output_dir,
            "bundle_dir": output_dir,
            "site_dir": bundle_result.site_dir,
            "private_dir": bundle_result.private_dir,
            "stats": {
                "conversations": stats.conversations_processed,
                "messages": stats.messages_processed,
            },
            "encryption": {
                "enabled": encryption_enabled,
                "generate_recovery": wizard_state.generate_recovery && encryption_enabled,
                "generate_qr": wizard_state.generate_qr && encryption_enabled,
            },
            "bundle": {
                "total_files": bundle_result.total_files,
                "fingerprint": bundle_result.fingerprint,
            },
            "deployment": deploy_result,
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Export complete:");
        println!("  Output: {}", output_dir.display());
        println!("  Site: {}", bundle_result.site_dir.display());
        println!("  Private: {}", bundle_result.private_dir.display());
        println!("  Conversations: {}", stats.conversations_processed);
        println!("  Messages: {}", stats.messages_processed);
        if encryption_enabled {
            println!("  Encryption: enabled");
        } else {
            println!("  Encryption: DISABLED (content is public)");
        }
        println!("  Fingerprint: {}", &bundle_result.fingerprint[..8]);

        if let Some(deploy) = deploy_result {
            println!("  Deployment: {}", deploy);
        }
    }

    Ok(())
}

/// Show API and contract versions (robot-friendly)
fn run_api_version(json: bool) -> CliResult<()> {
    let payload = serde_json::json!({
        "crate_version": env!("CARGO_PKG_VERSION"),
        "api_version": 1,
        "contract_version": CONTRACT_VERSION,
    });

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        return output_structured_value(payload, fmt);
    }

    println!("CASS API Version");
    println!("================");
    println!("crate: {}", env!("CARGO_PKG_VERSION"));
    println!("api:   v{}", 1);
    println!("contract: v{CONTRACT_VERSION}");

    Ok(())
}

/// Build command schemas for all CLI commands
fn build_command_schemas() -> Vec<CommandSchema> {
    let root = Cli::command();
    root.get_subcommands()
        .map(command_schema_from_clap)
        .collect()
}

fn command_schema_from_clap(cmd: &Command) -> CommandSchema {
    CommandSchema {
        name: cmd.get_name().to_string(),
        description: cmd
            .get_about()
            .or_else(|| cmd.get_long_about())
            .map(std::string::ToString::to_string)
            .unwrap_or_default(),
        arguments: cmd
            .get_arguments()
            .filter(|arg| !should_skip_arg(arg))
            .map(argument_schema_from_clap)
            .collect(),
        has_json_output: cmd
            .get_arguments()
            .any(|arg| arg.get_id().as_str() == "json"),
    }
}

fn argument_schema_from_clap(arg: &Arg) -> ArgumentSchema {
    let num_args = arg.get_num_args().unwrap_or_default();
    let takes_values = arg.get_action().takes_values() && num_args.takes_values();

    let arg_type = if !takes_values {
        "flag".to_string()
    } else if arg.is_positional() {
        "positional".to_string()
    } else {
        "option".to_string()
    };

    let value_type = if takes_values {
        infer_value_type(arg)
    } else {
        None
    };

    let default = {
        let defaults = arg.get_default_values();
        if defaults.is_empty() {
            None
        } else {
            Some(
                defaults
                    .iter()
                    .map(|v| v.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(","),
            )
        }
    };

    ArgumentSchema {
        name: arg.get_long().map_or_else(
            || arg.get_id().as_str().to_string(),
            std::string::ToString::to_string,
        ),
        short: arg.get_short(),
        description: arg
            .get_help()
            .or_else(|| arg.get_long_help())
            .map(std::string::ToString::to_string)
            .unwrap_or_default(),
        arg_type,
        value_type,
        required: arg.is_required_set(),
        default,
        enum_values: extract_enum_values(arg),
        repeatable: infer_repeatable(arg, num_args),
    }
}

const INTEGER_ARG_NAMES: &[&str] = &[
    "limit",
    "offset",
    "max-content-length",
    "max-tokens",
    "days",
    "line",
    "context",
    "stale-threshold",
];

fn infer_value_type(arg: &Arg) -> Option<String> {
    let name = arg.get_long().map_or_else(
        || arg.get_id().as_str().to_string(),
        std::string::ToString::to_string,
    );

    if !arg.get_possible_values().is_empty() {
        return Some("enum".to_string());
    }

    if matches!(
        arg.get_value_hint(),
        ValueHint::AnyPath | ValueHint::DirPath | ValueHint::FilePath | ValueHint::ExecutablePath
    ) {
        return Some("path".to_string());
    }

    if INTEGER_ARG_NAMES.contains(&name.as_str()) {
        return Some("integer".to_string());
    }

    Some("string".to_string())
}

fn extract_enum_values(arg: &Arg) -> Option<Vec<String>> {
    let values = arg.get_possible_values();
    if values.is_empty() {
        None
    } else {
        Some(values.iter().map(|v| v.get_name().to_string()).collect())
    }
}

fn infer_repeatable(arg: &Arg, num_args: clap::builder::ValueRange) -> Option<bool> {
    let multi_values = num_args.max_values() > 1;
    let append_action = matches!(arg.get_action(), ArgAction::Append | ArgAction::Count);

    if multi_values || append_action {
        Some(true)
    } else {
        None
    }
}

fn should_skip_arg(arg: &Arg) -> bool {
    arg.is_hide_set() || matches!(arg.get_id().as_str(), "help" | "version")
}

/// Build response schemas for commands that support JSON output
fn build_response_schemas() -> std::collections::HashMap<String, serde_json::Value> {
    use serde_json::json;
    let mut schemas = std::collections::HashMap::new();

    schemas.insert(
        "search".to_string(),
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer" },
                "offset": { "type": "integer" },
                "count": { "type": "integer" },
                "total_matches": { "type": "integer" },
                "max_tokens": { "type": ["integer", "null"] },
                "request_id": { "type": ["string", "null"] },
                "cursor": { "type": ["string", "null"] },
                "hits_clamped": { "type": "boolean" },
                "hits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "source_path": { "type": "string" },
                            "line_number": { "type": ["integer", "null"] },
                            "agent": { "type": "string" },
                            "workspace": { "type": ["string", "null"] },
                            "title": { "type": ["string", "null"] },
                            "content": { "type": ["string", "null"] },
                            "snippet": { "type": ["string", "null"] },
                            "score": { "type": ["number", "null"] },
                            "created_at": { "type": ["integer", "string", "null"] },
                            "match_type": { "type": ["string", "null"] },
                            "source_id": { "type": "string", "description": "Source identifier (e.g., 'local', 'work-laptop')" },
                            "origin_kind": { "type": "string", "description": "Origin kind ('local' or 'ssh')" },
                            "origin_host": { "type": ["string", "null"], "description": "Host label for remote sources" }
                        }
                    }
                },
                "aggregations": {
                    "type": ["object", "null"],
                    "additionalProperties": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": { "type": "string" },
                                "count": { "type": "integer" }
                            }
                        }
                    }
                },
                "_warning": { "type": ["string", "null"] },
                "_meta": {
                    "type": "object",
                    "properties": {
                        "elapsed_ms": { "type": "integer" },
                        "wildcard_fallback": { "type": "boolean" },
                        "cache_stats": {
                            "type": "object",
                            "properties": {
                                "hits": { "type": "integer" },
                                "misses": { "type": "integer" },
                                "shortfall": { "type": "integer" }
                            }
                        },
                        "tokens_estimated": { "type": ["integer", "null"] },
                        "max_tokens": { "type": ["integer", "null"] },
                        "request_id": { "type": ["string", "null"] },
                        "next_cursor": { "type": ["string", "null"] },
                        "hits_clamped": { "type": "boolean" },
                        "state": {
                            "type": "object",
                            "properties": {
                                "index": {
                                    "type": "object",
                                    "properties": {
                                        "exists": { "type": "boolean" },
                                        "fresh": { "type": "boolean" },
                                        "last_indexed_at": { "type": ["string", "null"] },
                                        "age_seconds": { "type": ["integer", "null"] },
                                        "stale": { "type": "boolean" },
                                        "stale_threshold_seconds": { "type": "integer" }
                                    }
                                },
                                "database": {
                                    "type": "object",
                                    "properties": {
                                        "exists": { "type": "boolean" },
                                        "conversations": { "type": "integer" },
                                        "messages": { "type": "integer" }
                                    }
                                }
                            }
                        },
                        "index_freshness": {
                            "type": "object",
                            "properties": {
                                "last_indexed_at": { "type": ["string", "null"] },
                                "age_seconds": { "type": ["integer", "null"] },
                                "stale": { "type": "boolean" },
                                "pending_sessions": { "type": "integer" },
                                "fresh": { "type": "boolean" }
                            }
                        }
                    }
                }
            }
        }),
    );

    schemas.insert(
        "status".to_string(),
        json!({
            "type": "object",
            "properties": {
                "healthy": { "type": "boolean" },
                "recommended_action": { "type": ["string", "null"] },
                "index": {
                    "type": "object",
                    "properties": {
                        "exists": { "type": "boolean" },
                        "fresh": { "type": "boolean" },
                        "last_indexed_at": { "type": ["string", "null"] },
                        "age_seconds": { "type": ["integer", "null"] },
                        "stale": { "type": "boolean" },
                        "stale_threshold_seconds": { "type": "integer" }
                    }
                },
                "database": {
                    "type": "object",
                    "properties": {
                        "exists": { "type": "boolean" },
                        "conversations": { "type": "integer" },
                        "messages": { "type": "integer" },
                        "path": { "type": "string" }
                    }
                },
                "pending": {
                    "type": "object",
                    "properties": {
                        "sessions": { "type": "integer" },
                        "watch_active": { "type": ["boolean", "null"] }
                    }
                },
                "_meta": {
                    "type": "object",
                    "properties": {
                        "timestamp": { "type": "string" },
                        "data_dir": { "type": "string" },
                        "db_path": { "type": "string" }
                    }
                }
            }
        }),
    );
    schemas.insert(
        "state".to_string(),
        json!({
            "type": "object",
            "properties": {
                "healthy": { "type": "boolean" },
                "recommended_action": { "type": ["string", "null"] },
                "index": {
                    "type": "object",
                    "properties": {
                        "exists": { "type": "boolean" },
                        "fresh": { "type": "boolean" },
                        "last_indexed_at": { "type": ["string", "null"] },
                        "age_seconds": { "type": ["integer", "null"] },
                        "stale": { "type": "boolean" },
                        "stale_threshold_seconds": { "type": "integer" }
                    }
                },
                "database": {
                    "type": "object",
                    "properties": {
                        "exists": { "type": "boolean" },
                        "conversations": { "type": "integer" },
                        "messages": { "type": "integer" },
                        "path": { "type": "string" }
                    }
                },
                "pending": {
                    "type": "object",
                    "properties": {
                        "sessions": { "type": "integer" },
                        "watch_active": { "type": ["boolean", "null"] }
                    }
                },
                "_meta": {
                    "type": "object",
                    "properties": {
                        "timestamp": { "type": "string" },
                        "data_dir": { "type": "string" },
                        "db_path": { "type": "string" }
                    }
                }
            }
        }),
    );

    schemas.insert(
        "capabilities".to_string(),
        json!({
            "type": "object",
            "properties": {
                "crate_version": { "type": "string" },
                "api_version": { "type": "integer" },
                "contract_version": { "type": "string" },
                "features": { "type": "array", "items": { "type": "string" } },
                "connectors": { "type": "array", "items": { "type": "string" } },
                "limits": {
                    "type": "object",
                    "properties": {
                        "max_limit": { "type": "integer" },
                        "max_content_length": { "type": "integer" },
                        "max_fields": { "type": "integer" },
                        "max_agg_buckets": { "type": "integer" }
                    }
                }
            }
        }),
    );

    schemas.insert(
        "api-version".to_string(),
        json!({
            "type": "object",
            "properties": {
                "crate_version": { "type": "string" },
                "api_version": { "type": "integer" },
                "contract_version": { "type": "string" }
            }
        }),
    );

    schemas.insert(
        "introspect".to_string(),
        json!({
            "type": "object",
            "properties": {
                "api_version": { "type": "integer" },
                "contract_version": { "type": "string" },
                "global_flags": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "short": { "type": ["string", "null"] },
                            "description": { "type": "string" },
                            "arg_type": { "type": "string" },
                            "value_type": { "type": ["string", "null"] },
                            "required": { "type": "boolean" },
                            "default": { "type": ["string", "null"] },
                            "enum_values": { "type": ["array", "null"] },
                            "repeatable": { "type": ["boolean", "null"] }
                        }
                    }
                },
                "commands": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "description": { "type": "string" },
                            "has_json_output": { "type": "boolean" },
                            "arguments": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "short": { "type": ["string", "null"] },
                                        "description": { "type": "string" },
                                        "arg_type": { "type": "string" },
                                        "value_type": { "type": ["string", "null"] },
                                        "required": { "type": "boolean" },
                                        "default": { "type": ["string", "null"] },
                                        "enum_values": { "type": ["array", "null"] },
                                        "repeatable": { "type": ["boolean", "null"] }
                                    }
                                }
                            }
                        }
                    }
                },
                "response_schemas": {
                    "type": "object",
                    "additionalProperties": { "type": "object" }
                }
            }
        }),
    );

    schemas.insert(
        "index".to_string(),
        json!({
            "type": "object",
            "properties": {
                "success": { "type": "boolean" },
                "elapsed_ms": { "type": "integer" },
                "full": { "type": ["boolean", "null"] },
                "force_rebuild": { "type": ["boolean", "null"] },
                "data_dir": { "type": ["string", "null"] },
                "db_path": { "type": ["string", "null"] },
                "conversations": { "type": ["integer", "null"] },
                "messages": { "type": ["integer", "null"] },
                "error": { "type": ["string", "null"] }
            }
        }),
    );

    schemas.insert(
        "diag".to_string(),
        json!({
            "type": "object",
            "properties": {
                "version": { "type": "string" },
                "platform": {
                    "type": "object",
                    "properties": {
                        "os": { "type": "string" },
                        "arch": { "type": "string" }
                    }
                },
                "paths": {
                    "type": "object",
                    "properties": {
                        "data_dir": { "type": "string" },
                        "db_path": { "type": "string" },
                        "index_path": { "type": "string" }
                    }
                },
                "database": {
                    "type": "object",
                    "properties": {
                        "exists": { "type": "boolean" },
                        "size_bytes": { "type": "integer" },
                        "conversations": { "type": "integer" },
                        "messages": { "type": "integer" }
                    }
                },
                "index": {
                    "type": "object",
                    "properties": {
                        "exists": { "type": "boolean" },
                        "size_bytes": { "type": "integer" }
                    }
                },
                "connectors": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "path": { "type": "string" },
                            "found": { "type": "boolean" }
                        }
                    }
                }
            }
        }),
    );

    schemas.insert(
        "view".to_string(),
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer" },
                "end_line": { "type": "integer" },
                "highlight_line": { "type": ["integer", "null"] },
                "lines": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "number": { "type": "integer" },
                            "content": { "type": "string" },
                            "highlighted": { "type": "boolean" }
                        }
                    }
                }
            }
        }),
    );

    schemas.insert(
        "stats".to_string(),
        json!({
            "type": "object",
            "properties": {
                "conversations": { "type": "integer" },
                "messages": { "type": "integer" },
                "by_agent": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent": { "type": "string" },
                            "count": { "type": "integer" }
                        }
                    }
                },
                "top_workspaces": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "count": { "type": "integer" }
                        }
                    }
                },
                "date_range": {
                    "type": "object",
                    "properties": {
                        "oldest": { "type": ["string", "null"] },
                        "newest": { "type": ["string", "null"] }
                    }
                },
                "db_path": { "type": "string" }
            }
        }),
    );

    schemas.insert(
        "health".to_string(),
        json!({
            "type": "object",
            "properties": {
                "healthy": { "type": "boolean" },
                "latency_ms": { "type": "integer" },
                "state": {
                    "type": "object",
                    "properties": {
                        "_meta": {
                            "type": "object",
                            "properties": {
                                "data_dir": { "type": "string" },
                                "db_path": { "type": "string" },
                                "timestamp": { "type": "string" }
                            }
                        },
                        "database": {
                            "type": "object",
                            "properties": {
                                "exists": { "type": "boolean" },
                                "conversations": { "type": "integer" },
                                "messages": { "type": "integer" }
                            }
                        },
                        "index": {
                            "type": "object",
                            "properties": {
                                "exists": { "type": "boolean" },
                                "fresh": { "type": "boolean" },
                                "last_indexed_at": { "type": ["string", "null"] },
                                "age_seconds": { "type": ["integer", "null"] },
                                "stale": { "type": "boolean" },
                                "stale_threshold_seconds": { "type": "integer" }
                            }
                        },
                        "pending": {
                            "type": "object",
                            "properties": {
                                "sessions": { "type": "integer" },
                                "watch_active": { "type": ["boolean", "null"] }
                            }
                        }
                    }
                }
            }
        }),
    );

    schemas
}

fn run_view(path: &PathBuf, line: Option<usize>, context: usize, json: bool) -> CliResult<()> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    if !path.exists() {
        return Err(CliError {
            code: 3,
            kind: "file-not-found",
            message: format!("File not found: {}", path.display()),
            hint: None,
            retryable: false,
        });
    }

    let file = File::open(path).map_err(|e| CliError {
        code: 9,
        kind: "file-open",
        message: format!("Failed to open file: {e}"),
        hint: None,
        retryable: false,
    })?;

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    if lines.is_empty() {
        return Err(CliError {
            code: 9,
            kind: "empty-file",
            message: format!("File is empty: {}", path.display()),
            hint: None,
            retryable: false,
        });
    }

    let target_line = line.unwrap_or(1);

    // Validate target line is within bounds
    if target_line == 0 {
        return Err(CliError {
            code: 2,
            kind: "invalid-line",
            message: "Line numbers start at 1, not 0".to_string(),
            hint: Some("Use -n 1 for the first line".to_string()),
            retryable: false,
        });
    }

    if target_line > lines.len() {
        return Err(CliError {
            code: 2,
            kind: "line-out-of-range",
            message: format!(
                "Line {} exceeds file length ({} lines)",
                target_line,
                lines.len()
            ),
            hint: Some(format!("Use -n {} for the last line", lines.len())),
            retryable: false,
        });
    }

    let start = target_line.saturating_sub(context + 1);
    let end = (target_line + context).min(lines.len());

    // Only highlight a specific line if -n was explicitly provided
    let highlight_line = line.is_some();

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let content_lines: Vec<serde_json::Value> = lines
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
            .map(|(i, l)| {
                serde_json::json!({
                    "line": i + 1,
                    "content": l,
                    "highlighted": highlight_line && i + 1 == target_line,
                })
            })
            .collect();

        let payload = serde_json::json!({
            "path": path.display().to_string(),
            "target_line": if highlight_line { Some(target_line) } else { None::<usize> },
            "context": context,
            "lines": content_lines,
            "total_lines": lines.len(),
        });
        return output_structured_value(payload, fmt);
    }

    println!("File: {}", path.display());
    if highlight_line {
        println!("Line: {target_line} (context: {context})");
    }
    println!("----------------------------------------");
    for (i, l) in lines.iter().enumerate().skip(start).take(end - start) {
        let line_num = i + 1;
        let marker = if highlight_line && line_num == target_line {
            ">"
        } else {
            " "
        };
        println!("{marker}{line_num:5} | {l}");
    }
    println!("----------------------------------------");
    if lines.len() > end {
        println!("... ({} more lines)", lines.len() - end);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_index_with_data(
    db_override: Option<PathBuf>,
    full: bool,
    force_rebuild: bool,
    watch: bool,
    watch_once: Option<Vec<PathBuf>>,
    data_dir_override: Option<PathBuf>,
    semantic: bool,
    build_hnsw: bool,
    embedder: String,
    progress: ProgressResolved,
    json: bool,
    idempotency_key: Option<String>,
) -> CliResult<()> {
    use rusqlite::Connection;
    use std::time::Instant;

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });
    let structured_output = structured_format.is_some();

    // Generate params hash for idempotency validation
    let params_hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        full.hash(&mut hasher);
        force_rebuild.hash(&mut hasher);
        watch.hash(&mut hasher);
        semantic.hash(&mut hasher);
        build_hnsw.hash(&mut hasher);
        embedder.hash(&mut hasher);
        format!("{}", data_dir.display()).hash(&mut hasher);
        hasher.finish()
    };

    // Check for cached idempotency result
    if let Some(key) = &idempotency_key
        && let Ok(conn) = Connection::open(&db_path)
    {
        // Ensure idempotency_keys table exists
        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS idempotency_keys (
                key TEXT PRIMARY KEY,
                params_hash TEXT NOT NULL,
                result_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            )",
            [],
        );

        // Clean expired keys
        let now_ms = chrono::Utc::now().timestamp_millis();
        let _ = conn.execute(
            "DELETE FROM idempotency_keys WHERE expires_at < ?1",
            [now_ms],
        );

        // Look up existing key
        let cached: Option<(String, String)> = conn
            .query_row(
                "SELECT params_hash, result_json FROM idempotency_keys WHERE key = ?1 AND expires_at > ?2",
                rusqlite::params![key, now_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        if let Some((stored_hash, result_json)) = cached {
            // Verify params match
            if stored_hash == params_hash.to_string() {
                // Return cached result
                if let Some(fmt) = structured_format {
                    // Parse and augment with cached flag
                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&result_json) {
                        val["cached"] = serde_json::json!(true);
                        val["idempotency_key"] = serde_json::json!(key);
                        output_structured_value(val, fmt)?;
                        return Ok(());
                    }
                } else {
                    eprintln!(
                        "Using cached result for idempotency key '{}' (use different key to force re-index)",
                        key
                    );
                    return Ok(());
                }
            } else {
                // Parameter mismatch - return error
                return Err(CliError {
                    code: 5,
                    kind: "idempotency_mismatch",
                    message: format!(
                        "Idempotency key '{}' was used with different parameters",
                        key
                    ),
                    hint: Some(
                        "Use a different idempotency key or wait for the existing one to expire (24h)".to_string(),
                    ),
                    retryable: false,
                });
            }
        }
    }

    let watch_once_paths = watch_once
        .filter(|paths| !paths.is_empty())
        .or_else(read_watch_once_paths_env);

    // Create progress tracker for real-time feedback
    let index_progress = std::sync::Arc::new(indexer::IndexingProgress::default());

    let opts = IndexOptions {
        full,
        force_rebuild,
        watch,
        watch_once_paths: watch_once_paths.clone(),
        db_path: db_path.clone(),
        data_dir: data_dir.clone(),
        semantic,
        build_hnsw,
        embedder: embedder.clone(),
        progress: Some(index_progress.clone()),
    };

    // Set up progress display
    let show_progress = !structured_output && matches!(progress, ProgressResolved::Bars);
    let show_plain = !structured_output && matches!(progress, ProgressResolved::Plain);

    if show_plain {
        eprintln!(
            "index starting (full={}, watch={}, watch_once={})",
            full,
            watch,
            watch_once_paths
                .as_ref()
                .map(std::vec::Vec::len)
                .unwrap_or_default()
        );
    }

    let start = Instant::now();

    // Run indexer in background thread so we can poll progress
    let opts_clone = opts.clone();
    let index_handle = std::thread::spawn(move || indexer::run_index(opts_clone, None));

    // Poll and display progress while indexer runs
    if show_progress {
        use indicatif::{ProgressBar, ProgressStyle};
        use std::sync::atomic::Ordering;

        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        // Set initial message BEFORE starting the tick
        pb.set_message(if full {
            "Starting full index...".to_string()
        } else {
            "Starting incremental index...".to_string()
        });
        pb.enable_steady_tick(Duration::from_millis(80));

        // Track last values to detect changes; use sentinel values to force first update
        let mut last_phase = usize::MAX;
        let mut last_current = usize::MAX;
        let mut last_agents = usize::MAX;
        let mut last_update = std::time::Instant::now();

        loop {
            // Check if indexer finished
            if index_handle.is_finished() {
                break;
            }

            let phase = index_progress.phase.load(Ordering::Relaxed);
            let total = index_progress.total.load(Ordering::Relaxed);
            let current = index_progress.current.load(Ordering::Relaxed);
            let agents = index_progress.discovered_agents.load(Ordering::Relaxed);
            let is_rebuilding = index_progress.is_rebuilding.load(Ordering::Relaxed);

            // Get agent names for display
            let agent_names: Vec<String> = index_progress
                .discovered_agent_names
                .lock()
                .map(|names| names.clone())
                .unwrap_or_default();

            let phase_str = match phase {
                1 => "Scanning",
                2 => "Indexing",
                _ => "Preparing",
            };

            let rebuild_indicator = if is_rebuilding { " (rebuilding)" } else { "" };

            let msg = if phase == 1 {
                let scan_progress = if total > 0 {
                    format!("{current}/{total} connectors")
                } else {
                    "scanning connectors".to_string()
                };
                if agents > 0 {
                    let names_preview = if agent_names.len() <= 3 {
                        agent_names.join(", ")
                    } else {
                        format!(
                            "{}, ... +{} more",
                            agent_names[..3].join(", "),
                            agent_names.len() - 3
                        )
                    };
                    format!(
                        "{}{}: {} · {} agent(s): {}",
                        phase_str, rebuild_indicator, scan_progress, agents, names_preview
                    )
                } else {
                    format!(
                        "{}{}: {} · detecting agents...",
                        phase_str, rebuild_indicator, scan_progress
                    )
                }
            } else if phase == 2 {
                // Indexing phase - show progress
                if total > 0 {
                    let pct = (current as f64 / total as f64 * 100.0).min(100.0);
                    format!(
                        "{}{}: {}/{} conversations ({:.0}%)",
                        phase_str, rebuild_indicator, current, total, pct
                    )
                } else {
                    format!("{}{}: Processing...", phase_str, rebuild_indicator)
                }
            } else {
                format!("{}{}...", phase_str, rebuild_indicator)
            };

            // Update when values change OR every 500ms to show activity
            let now = std::time::Instant::now();
            let should_update = phase != last_phase
                || current != last_current
                || agents != last_agents
                || now.duration_since(last_update).as_millis() > 500;

            if should_update {
                pb.set_message(msg);
                last_phase = phase;
                last_current = current;
                last_agents = agents;
                last_update = now;
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        // Final update
        let total = index_progress.total.load(Ordering::Relaxed);
        let current = index_progress.current.load(Ordering::Relaxed);
        let agents = index_progress.discovered_agents.load(Ordering::Relaxed);
        pb.finish_with_message(format!(
            "Done: {} conversations from {} agent(s)",
            current.max(total),
            agents
        ));
    } else if show_plain {
        // Plain mode: print periodic status updates
        use std::sync::atomic::Ordering;

        eprintln!("Starting index...");
        let mut last_phase = usize::MAX;
        let mut last_agents = 0;
        let mut last_current = 0;
        let mut last_scan_current = 0;

        loop {
            if index_handle.is_finished() {
                break;
            }

            let phase = index_progress.phase.load(Ordering::Relaxed);
            let total = index_progress.total.load(Ordering::Relaxed);
            let current = index_progress.current.load(Ordering::Relaxed);
            let agents = index_progress.discovered_agents.load(Ordering::Relaxed);

            // Print status on phase change
            if phase != last_phase {
                match phase {
                    1 => eprintln!("Scanning for agents..."),
                    2 => eprintln!("Indexing conversations..."),
                    _ => {}
                }
                last_phase = phase;
            }

            // Print scan progress during discovery
            if phase == 1 && current != last_scan_current {
                if total > 0 {
                    eprintln!("  Scanned {}/{} connectors", current, total);
                } else {
                    eprintln!("  Scanned {} connectors", current);
                }
                last_scan_current = current;
            }

            // Print agent discovery updates
            if agents > last_agents {
                eprintln!("  Found {} agent(s)", agents);
                last_agents = agents;
            }

            // Print indexing progress every 100 conversations
            if phase == 2 && current > last_current && current % 100 == 0 {
                if total > 0 {
                    eprintln!("  Indexed {}/{} conversations", current, total);
                } else {
                    eprintln!("  Indexed {} conversations", current);
                }
                last_current = current;
            }

            std::thread::sleep(Duration::from_millis(200));
        }
    } else {
        // No progress display (json mode or none): just wait for completion
        while !index_handle.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // Get the result from the indexer thread
    let res = index_handle
        .join()
        .map_err(|_| CliError {
            code: 9,
            kind: "index",
            message: "index thread panicked".to_string(),
            hint: None,
            retryable: true,
        })?
        .map_err(|e| {
            let chain = e
                .chain()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(" | ");
            CliError {
                code: 9,
                kind: "index",
                message: format!("index failed: {chain}"),
                hint: None,
                retryable: true,
            }
        });
    let elapsed_ms = start.elapsed().as_millis();

    if let Err(err) = &res {
        if let Some(fmt) = structured_format {
            let payload = serde_json::json!({
                "success": false,
                "error": err.message,
                "elapsed_ms": elapsed_ms,
            });
            output_structured_value(payload, fmt)?;
        } else {
            eprintln!("index debug error: {err:?}");
        }
    } else if let Some(fmt) = structured_format {
        // Get stats after successful indexing
        let (conversations, messages) = if let Ok(conn) = Connection::open(&db_path) {
            let convs: i64 = conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
                .unwrap_or(0);
            let msgs: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap_or(0);
            (convs, msgs)
        } else {
            (0, 0)
        };
        let mut payload = serde_json::json!({
            "success": true,
            "elapsed_ms": elapsed_ms,
            "full": full,
            "force_rebuild": force_rebuild,
            "data_dir": data_dir.display().to_string(),
            "db_path": db_path.display().to_string(),
            "conversations": conversations,
            "messages": messages,
        });

        // Add structured indexing stats if available (T7.4)
        if let Ok(stats) = index_progress.stats.lock()
            && let serde_json::Value::Object(ref mut map) = payload
        {
            map.insert(
                "indexing_stats".to_string(),
                serde_json::to_value(&*stats).unwrap_or_default(),
            );
        }

        // Store idempotency key if provided
        if let Some(key) = &idempotency_key {
            payload["idempotency_key"] = serde_json::json!(key);
            payload["cached"] = serde_json::json!(false);

            if let Ok(conn) = Connection::open(&db_path) {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let expires_ms = now_ms + 24 * 60 * 60 * 1000; // 24 hours
                let result_json = serde_json::to_string(&payload).unwrap_or_default();
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO idempotency_keys (key, params_hash, result_json, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![key, params_hash.to_string(), result_json, now_ms, expires_ms],
                );
            }
        }

        output_structured_value(payload, fmt)?;
    }

    if show_plain {
        eprintln!("index completed");
    }

    res
}

pub fn default_db_path() -> PathBuf {
    default_data_dir().join("agent_search.db")
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = dotenvy::var("CASS_DATA_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    directories::ProjectDirs::from("com", "coding-agent-search", "coding-agent-search")
        .map(|p| p.data_dir().to_path_buf())
        .or_else(|| dirs::home_dir().map(|h| h.join(".coding-agent-search")))
        .unwrap_or_else(|| PathBuf::from("./data"))
}

/// Read session paths from a file or stdin (when path is "-").
/// Returns a HashSet of session paths for filtering.
fn read_session_paths(source: &str) -> Result<std::collections::HashSet<String>, std::io::Error> {
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader};

    let reader: Box<dyn BufRead> = if source == "-" {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        Box::new(BufReader::new(std::fs::File::open(source)?))
    };

    let paths: HashSet<String> = reader
        .lines()
        .map_while(Result::ok)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();

    Ok(paths)
}

const OWNER: &str = "Dicklesworthstone";
const REPO: &str = "coding_agent_session_search";

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
}

async fn maybe_prompt_for_update(once: bool) -> Result<()> {
    if once
        || dotenvy::var("CI").is_ok()
        || dotenvy::var("TUI_HEADLESS").is_ok()
        || dotenvy::var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT").is_ok()
        || !io::stdin().is_terminal()
    {
        return Ok(());
    }

    let client = Client::builder()
        .user_agent("coding-agent-search (update-check)")
        .timeout(Duration::from_secs(3))
        .build()?;

    let Some((latest_tag, latest_ver)) = latest_release_version(&client).await else {
        return Ok(());
    };

    let current_ver =
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 1, 0));
    if latest_ver <= current_ver {
        return Ok(());
    }

    println!(
        "A newer version is available: current v{current_ver}, latest {latest_tag}. Update now? (y/N): "
    );
    print!("> ");
    io::stdout().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return Ok(());
    }
    if !matches!(input.trim(), "y" | "Y") {
        return Ok(());
    }

    info!(target: "update", "starting self-update to {}", latest_tag);
    match run_self_update(&latest_tag) {
        Ok(true) => {
            println!("Update complete. Please restart cass.");
            std::process::exit(0);
        }
        Ok(false) => {
            warn!(target: "update", "self-update failed (installer returned error)");
        }
        Err(err) => {
            warn!(target: "update", "self-update failed: {err}");
        }
    }

    Ok(())
}

async fn latest_release_version(client: &Client) -> Option<(String, Version)> {
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest");
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let info: ReleaseInfo = resp.json().await.ok()?;
    let tag = info.tag_name;
    let version_str = tag.trim_start_matches('v');
    let version = Version::parse(version_str).ok()?;
    Some((tag, version))
}

#[cfg(windows)]
fn run_self_update(tag: &str) -> Result<bool> {
    let ps_cmd = format!(
        "irm https://raw.githubusercontent.com/{OWNER}/{REPO}/{tag}/install.ps1 | iex; install.ps1 -EasyMode -Verify -Version {tag}"
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .status()?;
    if status.success() {
        info!(target: "update", "updated to {tag}");
        Ok(true)
    } else {
        warn!(target: "update", "installer returned non-zero status: {status:?}");
        Ok(false)
    }
}

#[cfg(not(windows))]
fn run_self_update(tag: &str) -> Result<bool> {
    let sh_cmd = format!(
        "curl -fsSL https://raw.githubusercontent.com/{OWNER}/{REPO}/{tag}/install.sh | bash -s -- --easy-mode --verify --version {tag}"
    );
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh_cmd)
        .status()?;
    if status.success() {
        info!(target: "update", "updated to {tag}");
        Ok(true)
    } else {
        warn!(target: "update", "installer returned non-zero status: {status:?}");
        Ok(false)
    }
}

// ============================================================================
// NEW COMMANDS: Export, Expand, Timeline
// ============================================================================

/// Detect if a path points to an OpenCode storage session file.
/// OpenCode stores sessions in: storage/session/{projectID}/{sessionID}.json
fn detect_opencode_session(path: &Path) -> bool {
    // Must be a JSON file
    if path.extension().map(|e| e != "json").unwrap_or(true) {
        return false;
    }

    // Primary check: verify directory structure
    // Path should be: {storage_root}/session/{projectID}/{sessionID}.json
    // with sibling message/ and/or part/ directories
    if let Some(parent) = path.parent()
        && let Some(session_dir) = parent.parent()
        && session_dir
            .file_name()
            .map(|n| n == "session")
            .unwrap_or(false)
        && let Some(storage_root) = session_dir.parent()
    {
        let message_dir = storage_root.join("message");
        let part_dir = storage_root.join("part");
        if message_dir.exists() || part_dir.exists() {
            return true;
        }
    }

    // Fallback: check if path follows opencode naming convention
    // Pattern: .../opencode/storage/session/...
    let components: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    for window in components.windows(3) {
        let w0 = window[0].to_string_lossy().to_lowercase();
        let w1 = window[1].to_string_lossy().to_lowercase();
        let w2 = window[2].to_string_lossy().to_lowercase();
        if w0.contains("opencode") && w1 == "storage" && w2 == "session" {
            return true;
        }
    }

    false
}

/// Load an OpenCode session for export.
/// Returns (title, start_ts, end_ts, messages as JSON values).
#[allow(clippy::type_complexity)]
fn load_opencode_session_for_export(
    session_path: &Path,
) -> anyhow::Result<(
    Option<String>,
    Option<i64>,
    Option<i64>,
    Vec<serde_json::Value>,
)> {
    use anyhow::Context;
    use std::collections::HashMap;
    use walkdir::WalkDir;

    // Parse session file
    let session_content = std::fs::read_to_string(session_path)
        .with_context(|| format!("read session file {}", session_path.display()))?;
    let session: serde_json::Value = serde_json::from_str(&session_content)
        .with_context(|| format!("parse session JSON {}", session_path.display()))?;

    let session_id = session["id"]
        .as_str()
        .context("session missing 'id' field")?;
    let session_title = session["title"].as_str().map(String::from);
    let session_start = session["time"]["created"].as_i64();
    let session_end = session["time"]["updated"].as_i64();

    // Find storage root by going up from session file
    // Path: storage/session/{projectID}/{sessionID}.json
    let storage_root = session_path
        .parent() // {projectID}/
        .and_then(|p| p.parent()) // session/
        .and_then(|p| p.parent()) // storage/
        .context("cannot determine storage root from session path")?;

    let message_dir = storage_root.join("message").join(session_id);
    let part_dir = storage_root.join("part");

    if !message_dir.exists() {
        anyhow::bail!("message directory not found: {}", message_dir.display());
    }

    // Build map of message_id -> parts
    #[derive(serde::Deserialize, Clone)]
    struct PartInfo {
        #[serde(rename = "messageID")]
        message_id: Option<String>,
        #[serde(rename = "type")]
        part_type: Option<String>,
        text: Option<String>,
        state: Option<PartState>,
    }
    #[derive(serde::Deserialize, Clone)]
    struct PartState {
        output: Option<String>,
    }

    let mut parts_by_msg: HashMap<String, Vec<PartInfo>> = HashMap::new();
    if part_dir.exists() {
        for entry in WalkDir::new(&part_dir).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if p.extension().map(|e| e == "json").unwrap_or(false)
                && let Ok(content) = std::fs::read_to_string(p)
                && let Ok(part) = serde_json::from_str::<PartInfo>(&content)
                && let Some(msg_id) = &part.message_id
            {
                parts_by_msg.entry(msg_id.clone()).or_default().push(part);
            }
        }
    }

    // Load messages
    #[derive(serde::Deserialize)]
    struct MsgInfo {
        id: String,
        role: Option<String>,
        #[serde(rename = "modelID")]
        model_id: Option<String>,
        time: Option<MsgTime>,
    }
    #[derive(serde::Deserialize)]
    struct MsgTime {
        created: Option<i64>,
    }

    let mut messages: Vec<(i64, serde_json::Value)> = Vec::new();

    for entry in WalkDir::new(&message_dir)
        .max_depth(1)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if !p.extension().map(|e| e == "json").unwrap_or(false) {
            continue;
        }

        let content = match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let msg_info: MsgInfo = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Assemble content from parts
        let parts = parts_by_msg.get(&msg_info.id).cloned().unwrap_or_default();
        let mut content_pieces: Vec<String> = Vec::new();
        for part in &parts {
            match part.part_type.as_deref() {
                Some("text") => {
                    if let Some(text) = &part.text
                        && !text.trim().is_empty()
                    {
                        content_pieces.push(text.clone());
                    }
                }
                Some("tool") => {
                    if let Some(state) = &part.state
                        && let Some(output) = &state.output
                        && !output.trim().is_empty()
                    {
                        content_pieces.push(format!("[Tool Output]\n{output}"));
                    }
                }
                Some("reasoning") => {
                    if let Some(text) = &part.text
                        && !text.trim().is_empty()
                    {
                        content_pieces.push(format!("[Reasoning]\n{text}"));
                    }
                }
                Some("patch") => {
                    if let Some(text) = &part.text
                        && !text.trim().is_empty()
                    {
                        content_pieces.push(format!("[Patch]\n{text}"));
                    }
                }
                _ => {}
            }
        }

        let assembled_content = content_pieces.join("\n\n");
        if assembled_content.trim().is_empty() {
            continue;
        }

        let role = msg_info.role.unwrap_or_else(|| "assistant".to_string());
        let timestamp = msg_info.time.as_ref().and_then(|t| t.created).unwrap_or(0);

        // Build JSON value matching expected format for formatters
        let msg_json = serde_json::json!({
            "role": role,
            "content": assembled_content,
            "timestamp": timestamp,
            "model": msg_info.model_id,
        });

        messages.push((timestamp, msg_json));
    }

    // Sort by timestamp
    messages.sort_by_key(|(ts, _)| *ts);
    let sorted_messages: Vec<serde_json::Value> = messages.into_iter().map(|(_, m)| m).collect();

    // Compute timestamps from messages if not in session
    let start = session_start.or_else(|| {
        sorted_messages
            .first()
            .and_then(|m| m["timestamp"].as_i64())
    });
    let end = session_end.or_else(|| sorted_messages.last().and_then(|m| m["timestamp"].as_i64()));

    Ok((session_title, start, end, sorted_messages))
}

/// Export a conversation to markdown or other formats
fn run_export(
    path: &Path,
    format: ConvExportFormat,
    output: Option<&Path>,
    include_tools: bool,
) -> CliResult<()> {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};

    if !path.exists() {
        return Err(CliError {
            code: 3,
            kind: "file-not-found",
            message: format!("Session file not found: {}", path.display()),
            hint: Some("Use 'cass search' to find session paths".to_string()),
            retryable: false,
        });
    }

    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut session_title: Option<String> = None;
    let mut session_start: Option<i64> = None;
    let mut _session_end: Option<i64> = None;

    // Check if this is an OpenCode storage session file
    // OpenCode stores sessions in: storage/session/{projectID}/{sessionID}.json
    // with messages in: storage/message/{sessionID}/*.json
    // and parts in: storage/part/{messageID}/*.json
    let is_opencode = detect_opencode_session(path);

    if is_opencode {
        // Load OpenCode session using split storage format
        match load_opencode_session_for_export(path) {
            Ok((title, start, end, msgs)) => {
                session_title = title;
                session_start = start;
                _session_end = end;
                messages = msgs;
            }
            Err(e) => {
                return Err(CliError {
                    code: 9,
                    kind: "opencode-parse",
                    message: format!("Failed to parse OpenCode session: {e}"),
                    hint: Some(
                        "Ensure the session file is valid and message/part directories exist"
                            .into(),
                    ),
                    retryable: false,
                });
            }
        }
    } else {
        // Standard JSONL format
        let file = File::open(path).map_err(|e| CliError {
            code: 9,
            kind: "file-open",
            message: format!("Failed to open file: {e}"),
            hint: None,
            retryable: false,
        })?;

        let reader = BufReader::new(file);

        for line in reader.lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(ts) = msg.get("timestamp").and_then(|t| t.as_i64()) {
                    if session_start.is_none_or(|start| ts < start) {
                        session_start = Some(ts);
                    }
                    if _session_end.is_none_or(|end| ts > end) {
                        _session_end = Some(ts);
                    }
                }
                messages.push(msg);
            }
        }
    }

    if messages.is_empty() {
        return Err(CliError {
            code: 9,
            kind: "empty-session",
            message: format!("No messages found in: {}", path.display()),
            hint: if is_opencode {
                Some("Check that storage/message/{sessionID}/ contains message files".into())
            } else {
                None
            },
            retryable: false,
        });
    }

    // Find title from first user message (only if no title already set)
    if session_title.is_none() {
        for msg in &messages {
            let role = extract_role(msg);
            if role == "user" {
                let content = extract_text_content(msg);
                if !content.is_empty() {
                    let first_line = content.lines().next().unwrap_or("Untitled Session");
                    session_title = Some(smart_truncate(first_line, 80));
                    break;
                }
            }
        }
    }

    let formatted = match format {
        ConvExportFormat::Markdown => {
            format_as_markdown(&messages, &session_title, session_start, include_tools)
        }
        ConvExportFormat::Text => format_as_text(&messages, include_tools),
        ConvExportFormat::Json => serde_json::to_string_pretty(&messages).unwrap_or_default(),
        ConvExportFormat::Html => {
            format_as_html(&messages, &session_title, session_start, include_tools)
        }
    };

    if let Some(out_path) = output {
        let mut out_file = File::create(out_path).map_err(|e| CliError {
            code: 9,
            kind: "file-create",
            message: format!("Failed to create output file: {e}"),
            hint: None,
            retryable: false,
        })?;
        out_file
            .write_all(formatted.as_bytes())
            .map_err(|e| CliError {
                code: 9,
                kind: "file-write",
                message: format!("Failed to write output: {e}"),
                hint: None,
                retryable: false,
            })?;
        println!("Exported to: {}", out_path.display());
    } else {
        println!("{formatted}");
    }

    Ok(())
}

/// Export a session as a beautiful, self-contained HTML file with optional encryption.
#[allow(clippy::too_many_arguments)]
fn run_export_html(
    session_path: &Path,
    output_dir: Option<&Path>,
    filename: Option<&str>,
    encrypt: bool,
    password: Option<&str>,
    password_stdin: bool,
    include_tools: bool,
    show_timestamps: bool,
    enable_cdns: bool,
    theme: &str,
    dry_run: bool,
    explain: bool,
    open: bool,
    json_output: bool,
) -> CliResult<()> {
    use chrono::TimeZone;
    use html_export::{
        ExportOptions as HtmlExportOptions, HtmlExporter, Message, TemplateMetadata,
        generate_full_filename, get_downloads_dir, is_valid_filename,
    };
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, Write};

    // --- Validate session exists ---
    if !session_path.exists() {
        let err = CliError {
            code: 3,
            kind: "session_not_found",
            message: format!("Session file not found: {}", session_path.display()),
            hint: Some("Use 'cass search' to find session paths".to_string()),
            retryable: false,
        };
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "success": false,
                    "error": {
                        "code": err.code,
                        "kind": err.kind,
                        "message": err.message,
                        "hint": err.hint,
                        "retryable": err.retryable
                    }
                })
            );
            return Err(err);
        }
        return Err(err);
    }

    // --- Get password if encryption requested ---
    let final_password: Option<String> = if encrypt {
        if let Some(p) = password {
            Some(p.to_string())
        } else if password_stdin {
            let mut pwd = String::new();
            io::stdin().read_line(&mut pwd).map_err(|e| CliError {
                code: 6,
                kind: "password_read_error",
                message: format!("Failed to read password from stdin: {e}"),
                hint: None,
                retryable: false,
            })?;
            Some(pwd.trim().to_string())
        } else {
            let err = CliError {
                code: 6,
                kind: "password_required",
                message: "Password required for encryption".to_string(),
                hint: Some("Use --password <pwd> or --password-stdin".to_string()),
                retryable: false,
            };
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "success": false,
                        "error": {
                            "code": err.code,
                            "kind": err.kind,
                            "message": err.message,
                            "hint": err.hint,
                            "retryable": err.retryable
                        }
                    })
                );
            }
            return Err(err);
        }
    } else {
        None
    };

    // --- Load session messages ---
    let mut raw_messages: Vec<serde_json::Value> = Vec::new();
    let mut session_title: Option<String> = None;
    let mut session_start: Option<i64> = None;
    let mut session_end: Option<i64> = None;
    let mut agent_name: Option<String> = None;
    let mut workspace: Option<String> = None;

    // Detect agent from path
    let path_str = session_path.to_string_lossy();
    if path_str.contains(".claude") {
        agent_name = Some("claude_code".to_string());
    } else if path_str.contains(".codex") {
        agent_name = Some("codex".to_string());
    } else if path_str.contains("cursor") {
        agent_name = Some("cursor".to_string());
    } else if path_str.contains(".gemini") {
        agent_name = Some("gemini".to_string());
    } else if path_str.contains(".vibe") {
        agent_name = Some("vibe".to_string());
    }

    // Extract workspace from path
    if let Some(parent) = session_path.parent() {
        workspace = Some(parent.display().to_string());
    }

    let is_opencode = detect_opencode_session(session_path);

    if is_opencode {
        match load_opencode_session_for_export(session_path) {
            Ok((title, start, end, msgs)) => {
                session_title = title;
                session_start = start;
                session_end = end;
                raw_messages = msgs;
                agent_name = Some("opencode".to_string());
            }
            Err(e) => {
                return Err(CliError {
                    code: 9,
                    kind: "opencode_parse",
                    message: format!("Failed to parse OpenCode session: {e}"),
                    hint: Some("Ensure the session file is valid".into()),
                    retryable: false,
                });
            }
        }
    } else {
        let file = File::open(session_path).map_err(|e| CliError {
            code: 9,
            kind: "file_open",
            message: format!("Failed to open file: {e}"),
            hint: None,
            retryable: false,
        })?;

        let reader = BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(ts) = msg.get("timestamp").and_then(|t| t.as_i64()) {
                    if session_start.is_none_or(|start| ts < start) {
                        session_start = Some(ts);
                    }
                    if session_end.is_none_or(|end| ts > end) {
                        session_end = Some(ts);
                    }
                }
                raw_messages.push(msg);
            }
        }
    }

    if raw_messages.is_empty() {
        return Err(CliError {
            code: 9,
            kind: "empty_session",
            message: format!("No messages found in: {}", session_path.display()),
            hint: None,
            retryable: false,
        });
    }

    // Find title from first user message
    if session_title.is_none() {
        for msg in &raw_messages {
            let role = extract_role(msg);
            if role == "user" {
                let content = extract_text_content(msg);
                if !content.is_empty() {
                    let first_line = content.lines().next().unwrap_or("Untitled Session");
                    session_title = Some(smart_truncate(first_line, 80));
                    break;
                }
            }
        }
    }

    // --- Convert to renderer::Message format (filtering empty messages) ---
    let messages: Vec<Message> = raw_messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            let role = extract_role(msg);
            let content = extract_text_content(msg);
            let ts = msg.get("timestamp").and_then(|t| t.as_i64());
            let timestamp = ts
                .and_then(|ts| chrono::Utc.timestamp_millis_opt(ts).single())
                .map(|dt| dt.to_rfc3339());

            // Extract tool call info if present
            let tool_call = if include_tools {
                extract_tool_call(msg)
            } else {
                None
            };

            // If we have a tool_call, strip the redundant "[Tool: X]" prefix from content
            // since the tool call details are shown separately in the HTML export
            let content = if tool_call.is_some() {
                strip_tool_marker(&content)
            } else {
                content
            };

            // Skip empty messages: no content AND no tool call AND unknown role
            // This filters out malformed/empty entries that would look broken
            if content.is_empty() && tool_call.is_none() && role == "unknown" {
                return None;
            }

            // Also skip messages that are completely empty (no content, no tool call)
            // but keep tool calls even if content is empty (shows the tool interaction)
            if content.is_empty() && tool_call.is_none() {
                return None;
            }

            Some(Message {
                role,
                content,
                timestamp,
                tool_call,
                index: Some(i),
                author: None,
            })
        })
        .collect();

    // Store original message count for explain/dry_run modes
    let message_count = messages.len();

    // --- Build metadata ---
    let duration = match (session_start, session_end) {
        (Some(start), Some(end)) if end > start => {
            let mins = (end - start) / 60_000;
            if mins >= 60 {
                Some(format!("{}h {}m", mins / 60, mins % 60))
            } else if mins > 0 {
                Some(format!("{}m", mins))
            } else {
                Some("< 1m".to_string())
            }
        }
        _ => None,
    };

    let metadata = TemplateMetadata {
        timestamp: session_start.map(|ts| {
            chrono::Utc
                .timestamp_millis_opt(ts)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_default()
        }),
        agent: agent_name.clone(),
        message_count,
        duration,
        project: workspace.clone(),
    };

    // --- Generate output path ---
    let output_directory = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(get_downloads_dir);

    let workspace_path = workspace.as_deref().map(Path::new);

    let mut final_filename = if let Some(name) = filename {
        name.to_string()
    } else {
        generate_full_filename(
            agent_name.as_deref().unwrap_or("cass"),
            workspace_path,
            session_start,
            session_title.as_deref(),
            session_title.as_deref(),
        )
    };

    if Path::new(&final_filename).extension().is_none() {
        final_filename.push_str(".html");
    }

    if filename.is_some() && !is_valid_filename(&final_filename) {
        return Err(CliError {
            code: 4,
            kind: "invalid_filename",
            message: format!("Invalid output filename: {final_filename}"),
            hint: Some("Avoid path separators and reserved characters".to_string()),
            retryable: false,
        });
    }

    let output_path = output_directory.join(final_filename);

    // Estimate file size (rough: 200 bytes per message + overhead)
    let estimated_size = message_count * 200 + 15000;

    // --- Explain mode ---
    if explain {
        let plan = serde_json::json!({
            "plan": {
                "session_path": session_path.display().to_string(),
                "agent": agent_name,
                "messages": message_count,
                "output_path": output_path.display().to_string(),
                "estimated_size_bytes": estimated_size,
                "options": {
                    "encrypted": encrypt,
                    "include_tools": include_tools,
                    "show_timestamps": show_timestamps,
                    "cdns_enabled": enable_cdns,
                    "default_theme": theme
                }
            },
            "warnings": []
        });
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
        return Ok(());
    }

    // --- Dry run mode ---
    if dry_run {
        let result = serde_json::json!({
            "dry_run": true,
            "valid": true,
            "session_path": session_path.display().to_string(),
            "output_path": output_path.display().to_string(),
            "messages": message_count,
            "encrypted": encrypt,
            "estimated_size_bytes": estimated_size
        });
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        return Ok(());
    }

    // --- Build export options ---
    let export_options = HtmlExportOptions {
        title: session_title.clone(),
        include_cdn: enable_cdns,
        syntax_highlighting: true,
        include_search: true,
        include_theme_toggle: true,
        encrypt,
        print_styles: true,
        agent_name: agent_name.clone(),
        show_timestamps,
        show_tool_calls: include_tools,
    };

    // --- Export ---
    let exporter = HtmlExporter::with_options(export_options);
    let title = session_title.as_deref().unwrap_or("Conversation Export");

    // Group messages for consolidated rendering (tool calls with parent messages)
    let message_groups = group_messages_for_export(messages);

    let html = exporter
        .export_messages(title, &message_groups, metadata, final_password.as_deref())
        .map_err(|e| CliError {
            code: 5,
            kind: "export_failed",
            message: format!("Failed to export HTML: {e}"),
            hint: None,
            retryable: false,
        })?;

    // --- Write file ---
    std::fs::create_dir_all(output_path.parent().unwrap_or(Path::new("."))).ok();
    let mut file = File::create(&output_path).map_err(|e| CliError {
        code: 4,
        kind: "output_not_writable",
        message: format!("Could not create output file: {e}"),
        hint: Some(format!(
            "Check permissions for {}",
            output_directory.display()
        )),
        retryable: false,
    })?;
    file.write_all(html.as_bytes()).map_err(|e| CliError {
        code: 4,
        kind: "write_failed",
        message: format!("Failed to write file: {e}"),
        hint: None,
        retryable: false,
    })?;

    let file_size = html.len();

    // --- Open in browser if requested ---
    if open {
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(&output_path).spawn();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("xdg-open")
                .arg(&output_path)
                .spawn();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("explorer")
                .arg(&output_path)
                .spawn();
        }
    }

    // --- Output result ---
    if json_output {
        let result = serde_json::json!({
            "success": true,
            "exported": {
                "session_path": session_path.display().to_string(),
                "output_path": output_path.display().to_string(),
                "filename": output_path.file_name().map(|n| n.to_string_lossy().to_string()),
                "size_bytes": file_size,
                "encrypted": encrypt,
                "messages_count": message_count,
                "agent": agent_name,
                "workspace": workspace,
                "title": session_title
            }
        });
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        println!("✓ Exported to {}", output_path.display());
        if encrypt {
            println!("  🔒 Encrypted with Web Crypto (AES-256-GCM)");
        }
        println!("  {} messages, {} bytes", message_count, file_size);
    }

    Ok(())
}

/// Extract tool call information from a message for HTML export.
///
/// Supports multiple formats:
/// 1. Claude/Anthropic format: `content` array with `type: "tool_use"` or `type: "tool_result"` blocks
/// 2. Cursor/generic format: `type: "tool"` at top level with `message.tool_name`, `message.tool_input`, `message.tool_output`
fn extract_tool_call(msg: &serde_json::Value) -> Option<html_export::ToolCall> {
    // Format 2: Cursor/generic format - check for top-level type: "tool"
    if let Some(msg_type) = msg.get("type").and_then(|t| t.as_str())
        && msg_type == "tool"
    {
        // Look for tool info in the message object
        let inner = msg.get("message").unwrap_or(msg);

        let tool_name = inner
            .get("tool_name")
            .and_then(|n| n.as_str())
            .unwrap_or("tool");

        let tool_input = inner.get("tool_input").map(|i| {
            if i.is_object() || i.is_array() {
                serde_json::to_string_pretty(i).unwrap_or_default()
            } else if let Some(s) = i.as_str() {
                s.to_string()
            } else {
                i.to_string()
            }
        });

        let tool_output = inner.get("tool_output").map(|o| {
            if o.is_object() || o.is_array() {
                serde_json::to_string_pretty(o).unwrap_or_default()
            } else if let Some(s) = o.as_str() {
                s.to_string()
            } else {
                o.to_string()
            }
        });

        // Determine status from explicit status field or presence of output
        let status_str = inner
            .get("status")
            .and_then(|s| s.as_str())
            .or_else(|| msg.get("status").and_then(|s| s.as_str()));

        let status = match status_str {
            Some("success") => Some(html_export::ToolStatus::Success),
            Some("error") => Some(html_export::ToolStatus::Error),
            Some("pending") => Some(html_export::ToolStatus::Pending),
            _ if tool_output.is_some() => Some(html_export::ToolStatus::Success),
            // For exported conversations, don't show "pending" - just hide the status badge
            _ => None,
        };

        return Some(html_export::ToolCall {
            name: tool_name.to_string(),
            input: tool_input.unwrap_or_default(),
            output: tool_output,
            status,
        });
    }

    // Format 1: Claude/Anthropic format - content array with tool_use/tool_result blocks
    let content = msg
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| msg.get("content"));

    if let Some(arr) = content.and_then(|c| c.as_array()) {
        for block in arr {
            if let Some(block_type) = block.get("type").and_then(|t| t.as_str()) {
                match block_type {
                    "tool_use" => {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let input = block.get("input").map(|i| {
                            if i.is_object() || i.is_array() {
                                serde_json::to_string_pretty(i).unwrap_or_default()
                            } else if let Some(s) = i.as_str() {
                                s.to_string()
                            } else {
                                i.to_string()
                            }
                        });
                        return Some(html_export::ToolCall {
                            name: name.to_string(),
                            input: input.unwrap_or_default(),
                            output: None,
                            // For exported conversations, don't show "pending" for tool invocations
                            // without explicit status - the output may be in a separate message
                            status: None,
                        });
                    }
                    "tool_result" => {
                        let content = block.get("content").map(|c| {
                            if c.is_object() || c.is_array() {
                                serde_json::to_string_pretty(c).unwrap_or_default()
                            } else if let Some(s) = c.as_str() {
                                s.to_string()
                            } else {
                                c.to_string()
                            }
                        });
                        return Some(html_export::ToolCall {
                            name: "tool_result".to_string(),
                            input: String::new(),
                            output: content,
                            status: Some(html_export::ToolStatus::Success),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    None
}

// ============================================================================
// Message Grouping Algorithm for Consolidated HTML Export
// ============================================================================

/// Agent format for message structure detection.
///
/// Different coding agents use different message formats. This enum helps
/// the grouping algorithm understand how to parse and correlate messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFormat {
    /// Claude Code: content array with tool_use/tool_result blocks, correlation via tool_use_id
    ClaudeCode,
    /// Codex CLI: function_call and function role messages, correlation via function name
    Codex,
    /// Cursor: type: "tool" at top level with tool_name/tool_input/tool_output
    Cursor,
    /// OpenCode: special handling already exists
    OpenCode,
    /// Generic/unknown format
    Generic,
}

/// Message classification for grouping decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClassification {
    /// User message with actual text content
    UserContent,
    /// Assistant message with text (may also have embedded tools)
    AssistantContent,
    /// Assistant message with only tool calls, no text
    AssistantToolOnly,
    /// Response to a tool call (tool_result, function response)
    ToolResult,
    /// System message
    System,
    /// Empty or skip-worthy message
    Empty,
}

/// Detect the agent format from a set of messages.
///
/// Examines message structure to determine which agent produced them.
/// This enables format-specific correlation logic.
pub fn detect_agent_format(messages: &[html_export::Message]) -> AgentFormat {
    use tracing::trace;

    // Check first few messages for format indicators
    for msg in messages.iter().take(10) {
        let role = msg.role.as_str();

        // Codex uses "function" role for tool results
        if role == "function" {
            trace!(
                agent_format = "codex",
                "Detected Codex format from function role"
            );
            return AgentFormat::Codex;
        }

        // Check for tool_call presence and structure
        if let Some(ref tc) = msg.tool_call {
            // Cursor format has specific tool names like "tool"
            if tc.name == "tool" || tc.name.starts_with("tool_") {
                trace!(
                    agent_format = "cursor",
                    "Detected Cursor format from tool name pattern"
                );
                return AgentFormat::Cursor;
            }
            // Claude Code uses standard tool names (Bash, Read, Write, etc.)
            if matches!(
                tc.name.as_str(),
                "Bash" | "Read" | "Write" | "Edit" | "Glob" | "Grep" | "Task" | "WebFetch"
            ) {
                trace!(
                    agent_format = "claude_code",
                    "Detected Claude Code format from tool name"
                );
                return AgentFormat::ClaudeCode;
            }
        }
    }

    trace!(
        agent_format = "generic",
        "Using generic format (no specific pattern detected)"
    );
    AgentFormat::Generic
}

/// Classify a message for grouping purposes.
///
/// Determines how a message should be handled in the grouping algorithm:
/// - Starting a new group
/// - Attaching to current group
/// - Being a tool result
/// - Being skipped
pub fn classify_message(msg: &html_export::Message, _format: AgentFormat) -> MessageClassification {
    use tracing::trace;

    let role = msg.role.as_str();
    let has_content = !msg.content.trim().is_empty();
    let has_tool = msg.tool_call.is_some();

    trace!(
        role = role,
        has_content = has_content,
        has_tool = has_tool,
        "Classifying message"
    );

    match role {
        "user" => {
            if has_content {
                MessageClassification::UserContent
            } else {
                MessageClassification::Empty
            }
        }
        "assistant" | "agent" => {
            if has_content {
                MessageClassification::AssistantContent
            } else if has_tool {
                MessageClassification::AssistantToolOnly
            } else {
                MessageClassification::Empty
            }
        }
        "tool" | "function" => {
            // Tool result messages
            MessageClassification::ToolResult
        }
        "system" => MessageClassification::System,
        _ => {
            // Unknown role - check if it has meaningful content
            if has_content || has_tool {
                MessageClassification::AssistantContent
            } else {
                MessageClassification::Empty
            }
        }
    }
}

/// Extract correlation ID from a message for tool call/result matching.
///
/// Different formats use different correlation mechanisms:
/// - Claude Code: tool_use_id in content blocks
/// - Codex: function call name
/// - Generic: message index fallback
pub fn extract_correlation_id(msg: &html_export::Message, format: AgentFormat) -> Option<String> {
    use tracing::trace;

    // First, try to use the tool call name as a simple correlation
    // This works for most formats as a baseline
    if let Some(ref tc) = msg.tool_call {
        let corr_id = match format {
            AgentFormat::ClaudeCode => {
                // Claude uses tool_use_id but we don't have access to raw JSON here
                // Fall back to tool name + index
                Some(format!("claude-{}", tc.name))
            }
            AgentFormat::Codex => {
                // Codex correlates by function name
                Some(format!("codex-{}", tc.name))
            }
            AgentFormat::Cursor => Some(format!("cursor-{}", tc.name)),
            AgentFormat::OpenCode => Some(format!("opencode-{}", tc.name)),
            AgentFormat::Generic => Some(format!("generic-{}", tc.name)),
        };
        trace!(correlation_id = ?corr_id, tool_name = %tc.name, "Extracted correlation ID");
        return corr_id;
    }

    // For tool results without explicit tool_call, use index as fallback
    msg.index.map(|idx| format!("index-{}", idx))
}

/// Flush the current group into the groups vector if it exists.
fn flush_group(
    groups: &mut Vec<html_export::MessageGroup>,
    current_group: &mut Option<html_export::MessageGroup>,
) {
    if let Some(group) = current_group.take() {
        tracing::trace!(
            group_type = ?group.group_type,
            tool_count = group.tool_count(),
            "Flushing message group"
        );
        groups.push(group);
    }
}

/// Groups flat messages into MessageGroups with tool correlation.
///
/// # Algorithm
/// 1. Detect agent format from message structure
/// 2. Classify each message
/// 3. User/Assistant content messages start new groups
/// 4. Tool-only messages attach to current assistant group
/// 5. Tool results correlate by ID to matching tool call
/// 6. System messages are standalone groups
/// 7. Track timestamps for group range
///
/// # Logging
/// - INFO: Group formation summary
/// - DEBUG: Each message classification
/// - TRACE: Correlation matching details
///
/// # Example
/// ```ignore
/// let messages: Vec<Message> = load_messages();
/// let groups = group_messages_for_export(messages);
/// for group in groups {
///     render_message_group(&group);
/// }
/// ```
pub fn group_messages_for_export(
    messages: Vec<html_export::Message>,
) -> Vec<html_export::MessageGroup> {
    use tracing::{debug, info, trace};

    info!(message_count = messages.len(), "Starting message grouping");

    let format = detect_agent_format(&messages);
    debug!(?format, "Detected agent format");

    let mut groups: Vec<html_export::MessageGroup> = Vec::new();
    let mut current_group: Option<html_export::MessageGroup> = None;

    for (idx, msg) in messages.iter().enumerate() {
        let classification = classify_message(msg, format);
        debug!(
            idx = idx,
            classification = ?classification,
            role = %msg.role,
            content_preview = %msg.content.chars().take(50).collect::<String>(),
            "Classified message"
        );

        match classification {
            MessageClassification::UserContent => {
                // User messages start a new group
                flush_group(&mut groups, &mut current_group);
                let group = html_export::MessageGroup::user(msg.clone());
                current_group = Some(group);
            }

            MessageClassification::AssistantContent => {
                // Assistant content starts a new group
                flush_group(&mut groups, &mut current_group);
                let mut group = html_export::MessageGroup::assistant(msg.clone());

                // If assistant has embedded tool calls, add them
                if let Some(ref tc) = msg.tool_call {
                    let corr_id = extract_correlation_id(msg, format);
                    group.add_tool_call(tc.clone(), corr_id);
                    trace!(tool_name = %tc.name, "Added embedded tool call to assistant group");
                }
                current_group = Some(group);
            }

            MessageClassification::AssistantToolOnly => {
                // Tool-only messages attach to current group or create tool-only group
                if let Some(ref mut g) = current_group {
                    // Attach to existing group
                    if let Some(ref tc) = msg.tool_call {
                        let corr_id = extract_correlation_id(msg, format);
                        g.add_tool_call(tc.clone(), corr_id);
                        g.update_end_timestamp(msg.timestamp.clone());
                        trace!(tool_name = %tc.name, "Attached tool call to current group");
                    }
                } else {
                    // Create a new tool-only group
                    let mut group = html_export::MessageGroup::tool_only(msg.clone());
                    if let Some(ref tc) = msg.tool_call {
                        let corr_id = extract_correlation_id(msg, format);
                        group.add_tool_call(tc.clone(), corr_id);
                    }
                    current_group = Some(group);
                    trace!("Created new tool-only group");
                }
            }

            MessageClassification::ToolResult => {
                // Tool results attach to current group
                if let Some(ref mut g) = current_group {
                    // Create a ToolResult from the message
                    let tool_name = msg
                        .tool_call
                        .as_ref()
                        .map(|tc| tc.name.clone())
                        .unwrap_or_else(|| "tool_result".to_string());

                    let content = if let Some(ref tc) = msg.tool_call {
                        tc.output.clone().unwrap_or_else(|| msg.content.clone())
                    } else {
                        msg.content.clone()
                    };

                    let status = msg
                        .tool_call
                        .as_ref()
                        .and_then(|tc| tc.status)
                        .unwrap_or(html_export::ToolStatus::Success);

                    let mut result =
                        html_export::ToolResult::new(tool_name.clone(), content, status);

                    // Try to get correlation ID
                    if let Some(corr_id) = extract_correlation_id(msg, format) {
                        result = result.with_correlation_id(corr_id);
                    }

                    g.add_tool_result(result);
                    g.update_end_timestamp(msg.timestamp.clone());
                    trace!(tool_name = %tool_name, "Added tool result to group");
                } else {
                    debug!(
                        idx = idx,
                        "Orphan tool result, no current group to attach to"
                    );
                }
            }

            MessageClassification::System => {
                // System messages are standalone groups
                flush_group(&mut groups, &mut current_group);
                let group = html_export::MessageGroup::system(msg.clone());
                groups.push(group);
                trace!("Added standalone system group");
            }

            MessageClassification::Empty => {
                trace!(idx = idx, "Skipping empty message");
            }
        }
    }

    // Flush any remaining group
    flush_group(&mut groups, &mut current_group);

    info!(
        group_count = groups.len(),
        original_messages = messages.len(),
        "Message grouping complete"
    );

    groups
}

// ============================================================================
// Unit Tests for Message Grouping Algorithm
// ============================================================================

#[cfg(test)]
mod message_grouping_tests {
    use super::*;
    use html_export::{Message, MessageGroupType, ToolCall, ToolStatus};

    // Helper to create a user message
    fn msg_user(content: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: content.to_string(),
            timestamp: Some("2026-01-15T10:00:00Z".to_string()),
            tool_call: None,
            index: None,
            author: None,
        }
    }

    // Helper to create an assistant message
    fn msg_assistant(content: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: content.to_string(),
            timestamp: Some("2026-01-15T10:00:05Z".to_string()),
            tool_call: None,
            index: None,
            author: None,
        }
    }

    // Helper to create an assistant message with tool call
    fn msg_assistant_with_tool(content: &str, tool_name: &str, tool_input: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: content.to_string(),
            timestamp: Some("2026-01-15T10:00:05Z".to_string()),
            tool_call: Some(ToolCall {
                name: tool_name.to_string(),
                input: tool_input.to_string(),
                output: None,
                status: None,
            }),
            index: None,
            author: None,
        }
    }

    // Helper to create a tool result message
    fn msg_tool_result(tool_name: &str, output: &str, status: ToolStatus) -> Message {
        Message {
            role: "tool".to_string(),
            content: output.to_string(),
            timestamp: Some("2026-01-15T10:00:10Z".to_string()),
            tool_call: Some(ToolCall {
                name: tool_name.to_string(),
                input: String::new(),
                output: Some(output.to_string()),
                status: Some(status),
            }),
            index: None,
            author: None,
        }
    }

    // Helper to create a system message
    fn msg_system(content: &str) -> Message {
        Message {
            role: "system".to_string(),
            content: content.to_string(),
            timestamp: Some("2026-01-15T09:59:00Z".to_string()),
            tool_call: None,
            index: None,
            author: None,
        }
    }

    // Helper to create an empty message
    fn msg_empty() -> Message {
        Message {
            role: "assistant".to_string(),
            content: String::new(),
            timestamp: Some("2026-01-15T10:00:00Z".to_string()),
            tool_call: None,
            index: None,
            author: None,
        }
    }

    // ========================================================================
    // Basic Grouping Tests
    // ========================================================================

    #[test]
    fn test_single_user_message() {
        let msgs = vec![msg_user("Hello")];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_type, MessageGroupType::User);
        assert_eq!(groups[0].primary.content, "Hello");
    }

    #[test]
    fn test_single_assistant_message() {
        let msgs = vec![msg_assistant("Hi there!")];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_type, MessageGroupType::Assistant);
    }

    #[test]
    fn test_user_assistant_pair() {
        let msgs = vec![msg_user("Hello"), msg_assistant("Hi there!")];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].group_type, MessageGroupType::User);
        assert_eq!(groups[1].group_type, MessageGroupType::Assistant);
    }

    #[test]
    fn test_assistant_with_single_tool() {
        let msgs = vec![
            msg_assistant_with_tool("Let me check that file.", "Read", "/path/file.rs"),
            msg_tool_result("Read", "file contents here", ToolStatus::Success),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 1, "Should group assistant + tool into one");
        assert_eq!(groups[0].group_type, MessageGroupType::Assistant);
        assert_eq!(groups[0].tool_calls.len(), 1, "Should have 1 tool call");
    }

    #[test]
    fn test_assistant_with_multiple_tools() {
        let msgs = vec![
            msg_assistant_with_tool("Running multiple commands.", "Bash", "ls"),
            msg_tool_result("Bash", "file1 file2", ToolStatus::Success),
            msg_assistant_with_tool("", "Read", "/README.md"),
            msg_tool_result("Read", "# Title", ToolStatus::Success),
        ];
        let groups = group_messages_for_export(msgs);
        // First assistant group, then tool-only groups that get attached
        assert!(!groups.is_empty(), "Should have at least one group");
    }

    // ========================================================================
    // System Message Tests
    // ========================================================================

    #[test]
    fn test_system_message_standalone() {
        let msgs = vec![
            msg_system("You are a helpful assistant."),
            msg_user("Hello"),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].group_type, MessageGroupType::System);
        assert_eq!(groups[1].group_type, MessageGroupType::User);
    }

    #[test]
    fn test_system_message_in_middle() {
        let msgs = vec![
            msg_user("Hello"),
            msg_assistant("Hi!"),
            msg_system("Context reminder"),
            msg_user("Continue"),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[2].group_type, MessageGroupType::System);
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_messages_filtered() {
        let msgs = vec![
            msg_user("Hello"),
            msg_empty(), // Should be filtered
            msg_assistant("Hi there!"),
        ];
        let groups = group_messages_for_export(msgs);
        // Empty message should be skipped
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_consecutive_user_messages() {
        let msgs = vec![
            msg_user("First question"),
            msg_user("Second question"),
            msg_user("Third question"),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(
            groups.len(),
            3,
            "Each user message should be separate group"
        );
        for group in &groups {
            assert_eq!(group.group_type, MessageGroupType::User);
        }
    }

    #[test]
    fn test_consecutive_assistant_messages() {
        let msgs = vec![
            msg_assistant("First response"),
            msg_assistant("Second response"),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 2, "Each assistant message should be separate");
    }

    #[test]
    fn test_orphan_tool_result() {
        // Tool result without preceding tool call should be handled gracefully
        let msgs = vec![
            msg_user("Hello"),
            msg_tool_result("Read", "orphan result", ToolStatus::Success),
        ];
        let groups = group_messages_for_export(msgs);
        // Should have user group, orphan tool result might be dropped or attached
        assert!(!groups.is_empty());
    }

    #[test]
    fn test_unicode_content() {
        let msgs = vec![
            msg_user("Test Unicode: 你好世界! 🎉🚀 مرحبا العالم"),
            msg_assistant("Handling multilingual: Привет мир 日本語"),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 2);
        assert!(groups[0].primary.content.contains("你好"));
        assert!(groups[1].primary.content.contains("Привет"));
    }

    #[test]
    fn test_html_special_characters() {
        let msgs = vec![
            msg_user("<script>alert('xss')</script>"),
            msg_assistant("Response with & \"quotes\" 'single'"),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 2);
        // Content should be preserved (escaping happens at render time)
        assert!(groups[0].primary.content.contains("<script>"));
    }

    #[test]
    fn test_empty_input_returns_empty() {
        let msgs: Vec<Message> = vec![];
        let groups = group_messages_for_export(msgs);
        assert!(groups.is_empty());
    }

    // ========================================================================
    // Tool Status Tests
    // ========================================================================

    #[test]
    fn test_tool_success_status() {
        let msgs = vec![
            msg_assistant_with_tool("Running command", "Bash", "echo hello"),
            msg_tool_result("Bash", "hello", ToolStatus::Success),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 1);

        // Verify tool call exists
        assert!(
            !groups[0].tool_calls.is_empty(),
            "Should have at least one tool call"
        );

        // Check if result was correlated (may not always correlate depending on format)
        let tc = &groups[0].tool_calls[0];
        assert_eq!(tc.call.name, "Bash", "Tool call should be Bash");
    }

    #[test]
    fn test_tool_error_status() {
        let msgs = vec![
            msg_assistant_with_tool("Reading file", "Read", "/nonexistent"),
            msg_tool_result("Read", "File not found", ToolStatus::Error),
        ];
        let groups = group_messages_for_export(msgs);
        assert_eq!(groups.len(), 1);

        // Verify tool call exists
        assert!(
            !groups[0].tool_calls.is_empty(),
            "Should have at least one tool call"
        );

        // Tool call should exist with Read name
        let tc = &groups[0].tool_calls[0];
        assert_eq!(tc.call.name, "Read", "Tool call should be Read");
    }

    // ========================================================================
    // Agent Format Detection Tests
    // ========================================================================

    #[test]
    fn test_detect_claude_format() {
        let msgs = vec![msg_assistant_with_tool("Let me check", "Read", "/file.rs")];
        let format = detect_agent_format(&msgs);
        assert_eq!(format, AgentFormat::ClaudeCode);
    }

    #[test]
    fn test_detect_generic_format() {
        let msgs = vec![msg_user("Hello"), msg_assistant("Hi")];
        let format = detect_agent_format(&msgs);
        // No tool calls, so should be generic
        assert_eq!(format, AgentFormat::Generic);
    }

    // ========================================================================
    // Message Classification Tests
    // ========================================================================

    #[test]
    fn test_classify_user_content() {
        let msg = msg_user("Hello");
        let class = classify_message(&msg, AgentFormat::Generic);
        assert_eq!(class, MessageClassification::UserContent);
    }

    #[test]
    fn test_classify_assistant_content() {
        let msg = msg_assistant("Hi there");
        let class = classify_message(&msg, AgentFormat::Generic);
        assert_eq!(class, MessageClassification::AssistantContent);
    }

    #[test]
    fn test_classify_system() {
        let msg = msg_system("You are helpful");
        let class = classify_message(&msg, AgentFormat::Generic);
        assert_eq!(class, MessageClassification::System);
    }

    #[test]
    fn test_classify_empty() {
        let msg = msg_empty();
        let class = classify_message(&msg, AgentFormat::Generic);
        assert_eq!(class, MessageClassification::Empty);
    }

    #[test]
    fn test_classify_tool_result() {
        let msg = msg_tool_result("Read", "contents", ToolStatus::Success);
        let class = classify_message(&msg, AgentFormat::Generic);
        assert_eq!(class, MessageClassification::ToolResult);
    }

    // ========================================================================
    // Performance Tests
    // ========================================================================

    #[test]
    fn test_large_session_performance() {
        use std::time::Instant;

        // Generate 1000 messages
        let mut msgs = Vec::with_capacity(1000);
        for i in 0..500 {
            msgs.push(msg_user(&format!("Question {}", i)));
            msgs.push(msg_assistant(&format!("Answer {}", i)));
        }

        let start = Instant::now();
        let groups = group_messages_for_export(msgs);
        let elapsed = start.elapsed();

        assert_eq!(groups.len(), 1000);
        assert!(
            elapsed.as_millis() < 500,
            "Grouping 1000 messages took {}ms, should be < 500ms",
            elapsed.as_millis()
        );
    }

    // ========================================================================
    // Timestamp Tests
    // ========================================================================

    #[test]
    fn test_group_timestamps_captured() {
        let mut user_msg = msg_user("Hello");
        user_msg.timestamp = Some("2026-01-15T10:00:00Z".to_string());

        let msgs = vec![user_msg];
        let groups = group_messages_for_export(msgs);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].start_timestamp,
            Some("2026-01-15T10:00:00Z".to_string())
        );
    }
}

fn format_as_markdown(
    messages: &[serde_json::Value],
    title: &Option<String>,
    start_ts: Option<i64>,
    include_tools: bool,
) -> String {
    use chrono::{TimeZone, Utc};
    let mut md = String::new();
    md.push_str("# ");
    md.push_str(title.as_deref().unwrap_or("Conversation Export"));
    md.push('\n');

    if let Some(ts) = start_ts
        && let Some(dt) = Utc.timestamp_millis_opt(ts).single()
    {
        md.push_str(&format!(
            "\n*Started: {}*\n",
            dt.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    md.push_str("\n---\n\n");

    for msg in messages {
        let role = extract_role(msg);
        match role.as_str() {
            "user" => md.push_str("## 👤 User\n\n"),
            "assistant" => md.push_str("## 🤖 Assistant\n\n"),
            _ => md.push_str(&format!("## {}\n\n", role)),
        }

        let content = extract_text_content(msg);
        if !content.is_empty() {
            md.push_str(&content);
            md.push_str("\n\n");
        }

        // Also handle tool blocks if include_tools is set
        if include_tools {
            let content_val = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| msg.get("content"));
            if let Some(arr) = content_val.and_then(|c| c.as_array()) {
                for block in arr {
                    if let Some(block_type) = block.get("type").and_then(|t| t.as_str()) {
                        match block_type {
                            "tool_use" => {
                                let name =
                                    block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                md.push_str(&format!("**Tool: {}**\n", name));
                                if let Some(input) = block.get("input") {
                                    md.push_str("```json\n");
                                    md.push_str(
                                        &serde_json::to_string_pretty(input).unwrap_or_default(),
                                    );
                                    md.push_str("\n```\n\n");
                                }
                            }
                            "tool_result" => {
                                md.push_str("**Tool Result:**\n");
                                if let Some(c) = block.get("content").and_then(|c| c.as_str()) {
                                    let preview: String = c.chars().take(500).collect();
                                    md.push_str("```\n");
                                    md.push_str(&preview);
                                    if c.len() > 500 {
                                        md.push_str("\n... (truncated)");
                                    }
                                    md.push_str("\n```\n\n");
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        md.push_str("---\n\n");
    }
    md
}

fn format_as_text(messages: &[serde_json::Value], include_tools: bool) -> String {
    let mut text = String::new();
    for msg in messages {
        let role = extract_role(msg);
        text.push_str(&format!("=== {} ===\n\n", role.to_uppercase()));

        let content = extract_text_content(msg);
        if !content.is_empty() {
            text.push_str(&content);
            text.push_str("\n\n");
        }

        // Also handle tool blocks if include_tools is set
        if include_tools {
            // Check nested message.content for tool blocks
            let content_val = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| msg.get("content"));
            if let Some(arr) = content_val.and_then(|c| c.as_array()) {
                for block in arr {
                    if let Some(block_type) = block.get("type").and_then(|t| t.as_str())
                        && block_type == "tool_use"
                    {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        text.push_str(&format!("[Tool: {}]\n", name));
                    }
                }
            }
        }
    }
    text
}

fn format_as_html(
    messages: &[serde_json::Value],
    title: &Option<String>,
    start_ts: Option<i64>,
    include_tools: bool,
) -> String {
    use chrono::{TimeZone, Utc};
    let title_str = title.as_deref().unwrap_or("Conversation Export");
    let date_str = start_ts
        .and_then(|ts| Utc.timestamp_millis_opt(ts).single())
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();

    let mut html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>{title_str}</title>
    <style>
        body {{ font-family: system-ui, sans-serif; max-width: 800px; margin: 0 auto; padding: 20px; background: #0d0d12; color: #e0e0e5; }}
        .message {{ background: #18181f; border-radius: 8px; padding: 16px; margin: 12px 0; box-shadow: 0 1px 3px rgba(0,0,0,0.3); }}
        .user {{ border-left: 4px solid #3b82f6; }}
        .assistant {{ border-left: 4px solid #22c55e; }}
        .role {{ font-weight: bold; color: #a0a0a8; margin-bottom: 8px; }}
        .content {{ white-space: pre-wrap; line-height: 1.6; }}
        h1 {{ color: #f0f0f5; }}
        .meta {{ color: #6b7280; font-size: 0.9em; }}
        /* Tool badges */
        .tool-badges {{ display: flex; flex-wrap: wrap; gap: 4px; margin-top: 8px; }}
        .tool-badge {{ position: relative; display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; font-size: 11px; font-family: 'JetBrains Mono', ui-monospace, monospace; background: #1e1e26; border: 1px solid #2e2e38; border-radius: 4px; cursor: pointer; transition: all 0.15s; }}
        .tool-badge:hover {{ border-color: #f59e0b; background: rgba(245,158,11,0.1); }}
        .tool-badge-name {{ font-weight: 600; color: #f59e0b; }}
        .tool-popover {{ position: absolute; bottom: calc(100% + 8px); left: 50%; transform: translateX(-50%) scale(0.95); min-width: 280px; max-width: 400px; padding: 12px; background: #18181f; border: 1px solid #2e2e38; border-radius: 8px; box-shadow: 0 8px 32px rgba(0,0,0,0.4); opacity: 0; visibility: hidden; transition: all 0.15s; z-index: 1000; pointer-events: none; text-align: left; white-space: normal; }}
        .tool-badge:hover .tool-popover {{ opacity: 1; visibility: visible; transform: translateX(-50%) scale(1); pointer-events: auto; }}
        .tool-popover::after {{ content: ''; position: absolute; top: 100%; left: 50%; transform: translateX(-50%); border: 6px solid transparent; border-top-color: #2e2e38; }}
        .tool-popover-header {{ display: flex; align-items: center; gap: 8px; padding-bottom: 8px; margin-bottom: 8px; border-bottom: 1px solid #2e2e38; font-weight: 600; color: #f59e0b; }}
        .tool-popover-section {{ margin-bottom: 8px; }}
        .tool-popover-label {{ font-size: 9px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.8px; color: #6b7280; margin-bottom: 4px; display: block; }}
        .tool-popover pre {{ margin: 0; padding: 8px; font-size: 10px; background: #0d0d12; border-radius: 4px; max-height: 150px; overflow: auto; white-space: pre-wrap; word-break: break-word; }}
    </style>
</head>
<body>
    <h1>{title_str}</h1>
    <p class="meta">{date_str}</p>
"#
    );

    for msg in messages {
        let role = extract_role(msg);
        let role_class = if role == "user" { "user" } else { "assistant" };
        let role_display = match role.as_str() {
            "user" => "👤 User",
            "assistant" => "🤖 Assistant",
            "system" => "⚙️ System",
            _ => "💬 Message",
        };

        html.push_str(&format!(
            r#"    <div class="message {role_class}">
        <div class="role">{role_display}</div>
        <div class="content">"#
        ));

        // Use extract_text_content for consistent content extraction
        let content = extract_text_content(msg);
        // Strip "[Tool: X]" markers when we're showing tool badges
        let content = if include_tools {
            strip_tool_marker(&content)
        } else {
            content
        };
        html.push_str(&html_escape(&content));

        // Also handle tool use blocks if requested
        if include_tools {
            // Check for tool_use in nested message.content array
            let content_val = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| msg.get("content"));
            if let Some(arr) = content_val.and_then(|c| c.as_array()) {
                let mut tool_badges = Vec::new();
                for block in arr {
                    if let Some("tool_use") = block.get("type").and_then(|t| t.as_str()) {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let icon = match name.to_lowercase().as_str() {
                            "bash" | "shell" => "💻",
                            "read" | "read_file" => "📖",
                            "write" | "write_file" => "📝",
                            "edit" => "✏️",
                            "glob" | "find" => "🔍",
                            "grep" | "search" => "🔎",
                            "webfetch" | "fetch" | "http" => "🌐",
                            "websearch" => "🔍",
                            "task" => "🚀",
                            _ => "🔧",
                        };
                        // Get input preview (truncated)
                        let input_preview = block
                            .get("input")
                            .map(|v| {
                                let s = serde_json::to_string_pretty(v).unwrap_or_default();
                                if s.len() > 400 {
                                    format!("{}…", &s[..400])
                                } else {
                                    s
                                }
                            })
                            .unwrap_or_default();

                        let popover_content = if !input_preview.is_empty() {
                            format!(
                                r#"<div class="tool-popover-section"><span class="tool-popover-label">Input</span><pre>{}</pre></div>"#,
                                html_escape(&input_preview)
                            )
                        } else {
                            String::new()
                        };

                        tool_badges.push(format!(
                            r#"<span class="tool-badge" tabindex="0">
                                <span>{icon}</span>
                                <span class="tool-badge-name">{name}</span>
                                <div class="tool-popover">
                                    <div class="tool-popover-header">{icon} {name}</div>
                                    {popover}
                                </div>
                            </span>"#,
                            icon = icon,
                            name = html_escape(name),
                            popover = popover_content,
                        ));
                    }
                }
                if !tool_badges.is_empty() {
                    html.push_str(r#"<div class="tool-badges">"#);
                    for badge in tool_badges {
                        html.push_str(&badge);
                    }
                    html.push_str("</div>");
                }
            }
        }

        html.push_str("</div>\n    </div>\n");
    }
    html.push_str("</body>\n</html>\n");
    html
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Show messages around a specific line in a session file
fn run_expand(path: &Path, line: usize, context: usize, json: bool) -> CliResult<()> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    if !path.exists() {
        return Err(CliError {
            code: 3,
            kind: "file-not-found",
            message: format!("Session file not found: {}", path.display()),
            hint: Some("Use 'cass search' to find session paths".to_string()),
            retryable: false,
        });
    }

    let file = File::open(path).map_err(|e| CliError {
        code: 9,
        kind: "file-open",
        message: format!("Failed to open file: {e}"),
        hint: None,
        retryable: false,
    })?;

    let reader = BufReader::new(file);
    let mut messages: Vec<(usize, serde_json::Value)> = Vec::new();
    let mut target_msg_idx: Option<usize> = None;
    let mut current_line: usize = 0;

    for raw_line in reader.lines().map_while(Result::ok) {
        current_line += 1;
        if raw_line.trim().is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&raw_line) {
            if current_line == line {
                target_msg_idx = Some(messages.len());
            }
            messages.push((current_line, msg));
        }
    }

    if target_msg_idx.is_none() && line > 0 {
        for (idx, (msg_line, _)) in messages.iter().enumerate() {
            if *msg_line >= line {
                target_msg_idx = Some(idx);
                break;
            }
        }
        if target_msg_idx.is_none() && !messages.is_empty() {
            target_msg_idx = Some(messages.len() - 1);
        }
    }

    let target_idx = target_msg_idx.ok_or_else(|| CliError {
        code: 2,
        kind: "line-not-found",
        message: format!("No message found at or near line {}", line),
        hint: Some(format!("File has {} messages", messages.len())),
        retryable: false,
    })?;

    let start = target_idx.saturating_sub(context);
    let end = (target_idx + context + 1).min(messages.len());

    let context_messages: Vec<_> = messages[start..end]
        .iter()
        .enumerate()
        .map(|(i, (line_num, msg))| {
            let is_target = start + i == target_idx;
            (line_num, msg, is_target)
        })
        .collect();

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let output: Vec<serde_json::Value> = context_messages
            .iter()
            .map(|(line_num, msg, is_target)| {
                let role = extract_role(msg);
                let content = extract_text_content(msg);
                serde_json::json!({
                    "line": line_num,
                    "role": role,
                    "is_target": is_target,
                    "content": content,
                })
            })
            .collect();
        return output_structured_value(serde_json::Value::Array(output), fmt);
    }

    println!("\n📍 Context around line {} in {}\n", line, path.display());
    println!("{}", "─".repeat(60));

    for (line_num, msg, is_target) in context_messages {
        let role = extract_role(msg);
        let content = extract_text_content(msg);
        let preview: String = content.chars().take(300).collect();
        let marker = if is_target { ">>>" } else { "   " };
        let role_icon = match role.as_str() {
            "user" => "👤",
            "assistant" => "🤖",
            _ => "📝",
        };

        println!(
            "{} L{:>4} {} {}",
            marker,
            line_num,
            role_icon,
            role.to_uppercase()
        );
        println!("        {}", preview.replace('\n', " "));
        if content.len() > 300 {
            println!("        ... ({} more chars)", content.len() - 300);
        }
        println!();
    }

    println!("{}", "─".repeat(60));
    println!(
        "Showing messages {} to {} of {} total",
        start + 1,
        end,
        messages.len()
    );
    Ok(())
}

fn extract_text_content(msg: &serde_json::Value) -> String {
    // Use the well-tested flatten_content helper from connectors module
    // It handles: direct strings, {"type": "text"}, {"type": "input_text"},
    // blocks with "text" but no "type", and tool_use blocks
    fn try_flatten(content: &serde_json::Value) -> Option<String> {
        let result = crate::connectors::flatten_content(content);
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    // Try direct content first (standard format)
    if let Some(content) = msg.get("content")
        && let Some(text) = try_flatten(content)
    {
        return text;
    }
    // Try nested message.content (Claude Code format)
    if let Some(inner) = msg.get("message")
        && let Some(content) = inner.get("content")
        && let Some(text) = try_flatten(content)
    {
        return text;
    }
    // Try nested payload.content (Codex format: {"type": "response_item", "payload": {"content": ...}})
    if let Some(payload) = msg.get("payload")
        && let Some(content) = payload.get("content")
        && let Some(text) = try_flatten(content)
    {
        return text;
    }
    String::new()
}

/// Extract role from message (supports various formats)
fn extract_role(msg: &serde_json::Value) -> String {
    // Try direct role
    if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
        return role.to_string();
    }
    // Try nested message.role (Claude Code format)
    if let Some(inner) = msg.get("message")
        && let Some(role) = inner.get("role").and_then(|r| r.as_str())
    {
        return role.to_string();
    }
    // Try nested payload.role (Codex format: {"type": "response_item", "payload": {"role": "user", ...}})
    if let Some(payload) = msg.get("payload")
        && let Some(role) = payload.get("role").and_then(|r| r.as_str())
    {
        return role.to_string();
    }
    // Try type field (Claude Code also uses "type": "user" or "type": "assistant")
    if let Some(type_val) = msg.get("type").and_then(|t| t.as_str()) {
        match type_val {
            "user" => return "user".to_string(),
            "assistant" => return "assistant".to_string(),
            _ => {}
        }
    }
    "unknown".to_string()
}

/// Strip redundant "[Tool: X]" markers from content when tool call is shown separately.
///
/// When a message has a tool_call object that's rendered as a collapsible section,
/// including the "[Tool: X]" text in the content is redundant and looks bad.
fn strip_tool_marker(content: &str) -> String {
    let trimmed = content.trim();

    // Check if content starts with "[Tool: X]" pattern
    if trimmed.starts_with("[Tool:")
        && let Some(close_idx) = trimmed.find(']')
    {
        // Get content after the tool marker
        let after = trimmed[close_idx + 1..].trim();
        if after.is_empty() {
            // Entire content was just "[Tool: X]" - return empty
            return String::new();
        }
        // Return the content after the marker
        return after.to_string();
    }

    content.to_string()
}

/// Truncate a string smartly at word boundaries with ellipsis.
///
/// Returns the original string if it fits within max_len.
/// Otherwise, truncates at the last word boundary before max_len and adds "…".
fn smart_truncate(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_len {
        return s.to_string();
    }

    // Find the last space before the limit
    let char_indices: Vec<_> = s.char_indices().take(max_len).collect();
    if char_indices.is_empty() {
        return "…".to_string();
    }

    // Look for last word boundary (space)
    let last_idx = char_indices.last().map(|(i, _)| *i).unwrap_or(0);
    let truncated = &s[..=last_idx];

    // Find last space to break at word boundary
    if let Some(last_space) = truncated.rfind(|c: char| c.is_whitespace())
        && last_space > max_len / 2
    {
        // Only break at word if we're not losing too much
        return format!("{}…", truncated[..last_space].trim_end());
    }

    // No good word boundary, just truncate
    format!("{}…", truncated.trim_end())
}

/// Show activity timeline for a time range
#[allow(clippy::too_many_arguments)]
fn run_timeline(
    since: Option<&str>,
    until: Option<&str>,
    today: bool,
    agents: &[String],
    data_dir: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
    group_by: TimelineGrouping,
    source: Option<String>,
) -> CliResult<()> {
    use crate::sources::provenance::SourceFilter;
    use chrono::{Local, TimeZone, Utc};
    use std::collections::HashMap;

    // Parse source filter (P3.2)
    let source_filter = source.as_ref().map(|s| SourceFilter::parse(s));

    let lazy = crate::storage::sqlite::LazyDb::from_overrides(data_dir, db_override);
    let conn = lazy.get("timeline").map_err(lazy_db_to_cli_error)?;

    let now = Local::now();
    let (start_ts, end_ts) = if today {
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        let local_start = match Local.from_local_datetime(&start_of_day) {
            chrono::LocalResult::Single(dt) => dt,
            chrono::LocalResult::Ambiguous(dt, _) => dt,
            chrono::LocalResult::None => Local.from_utc_datetime(&start_of_day),
        };
        (local_start.timestamp_millis(), now.timestamp_millis())
    } else {
        let start = since
            .and_then(parse_datetime_flexible)
            .unwrap_or_else(|| (now - chrono::Duration::days(7)).timestamp_millis());
        let end = until
            .and_then(parse_datetime_flexible)
            .unwrap_or_else(|| now.timestamp_millis());
        (start, end)
    };

    let mut sql = String::from(
        "SELECT c.id, a.slug as agent, c.title, c.started_at, c.ended_at, c.source_path,
                COUNT(m.id) as message_count, c.source_id, c.origin_host, s.kind as origin_kind
         FROM conversations c
         JOIN agents a ON c.agent_id = a.id
         LEFT JOIN sources s ON c.source_id = s.id
         LEFT JOIN messages m ON m.conversation_id = c.id
         WHERE c.started_at >= ?1 AND c.started_at <= ?2",
    );

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(start_ts), Box::new(end_ts)];

    if !agents.is_empty() {
        sql.push_str(" AND a.slug IN (");
        for (i, agent) in agents.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("?{}", params.len() + 1));
            params.push(Box::new(agent.clone()));
        }
        sql.push(')');
    }

    // Source filter (P3.2)
    if let Some(ref filter) = source_filter {
        match filter {
            SourceFilter::All => {
                // No filtering needed
            }
            SourceFilter::Local => {
                sql.push_str(" AND c.source_id = 'local'");
            }
            SourceFilter::Remote => {
                sql.push_str(" AND c.source_id != 'local'");
            }
            SourceFilter::SourceId(id) => {
                sql.push_str(&format!(" AND c.source_id = ?{}", params.len() + 1));
                params.push(Box::new(id.clone()));
            }
        }
    }

    sql.push_str(" GROUP BY c.id ORDER BY c.started_at DESC");

    let mut stmt = conn.prepare(&sql).map_err(|e| CliError {
        code: 9,
        kind: "db-query",
        message: format!("Query failed: {e}"),
        hint: None,
        retryable: false,
    })?;

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,            // id
                row.get::<_, String>(1)?,         // agent
                row.get::<_, Option<String>>(2)?, // title
                row.get::<_, i64>(3)?,            // started_at
                row.get::<_, Option<i64>>(4)?,    // ended_at
                row.get::<_, String>(5)?,         // source_path
                row.get::<_, i64>(6)?,            // message_count
                row.get::<_, String>(7)?,         // source_id (P3.2)
                row.get::<_, Option<String>>(8)?, // origin_host (P3.5)
                row.get::<_, Option<String>>(9)?, // origin_kind (P3.5)
            ))
        })
        .map_err(|e| CliError {
            code: 9,
            kind: "db-query",
            message: format!("Query failed: {e}"),
            hint: None,
            retryable: false,
        })?;

    #[allow(clippy::type_complexity)]
    let mut sessions: Vec<(
        i64,
        String,
        Option<String>,
        i64,
        Option<i64>,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    for r in rows.flatten() {
        sessions.push(r);
    }

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let output = match group_by {
            TimelineGrouping::None => {
                let items: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(
                        |(
                            id,
                            agent,
                            title,
                            started,
                            ended,
                            path,
                            msg_count,
                            source_id,
                            origin_host,
                            origin_kind,
                        )| {
                            let duration = ended.map(|e| e - started);
                            // Use "local" as default origin_kind if not in DB (backward compat)
                            let kind = origin_kind.as_deref().unwrap_or("local");
                            serde_json::json!({
                                "id": id, "agent": agent, "title": title,
                                "started_at": started, "ended_at": ended,
                                "duration_seconds": duration, "source_path": path,
                                "message_count": msg_count,
                                // Provenance fields (P3.5)
                                "source_id": source_id,
                                "origin_kind": kind,
                                "origin_host": origin_host,
                            })
                        },
                    )
                    .collect();
                serde_json::json!({
                    "range": { "start": start_ts, "end": end_ts },
                    "total_sessions": sessions.len(),
                    "sessions": items,
                })
            }
            TimelineGrouping::Hour | TimelineGrouping::Day => {
                let mut groups: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
                for (
                    id,
                    agent,
                    title,
                    started,
                    ended,
                    path,
                    msg_count,
                    source_id,
                    origin_host,
                    origin_kind,
                ) in &sessions
                {
                    let dt = Utc
                        .timestamp_millis_opt(*started)
                        .single()
                        .unwrap_or_else(Utc::now);
                    let key = match group_by {
                        TimelineGrouping::Hour => dt.format("%Y-%m-%d %H:00").to_string(),
                        TimelineGrouping::Day => dt.format("%Y-%m-%d").to_string(),
                        _ => unreachable!(),
                    };
                    // Use "local" as default origin_kind if not in DB (backward compat)
                    let kind = origin_kind.as_deref().unwrap_or("local");
                    groups.entry(key).or_default().push(serde_json::json!({
                        "id": id, "agent": agent, "title": title,
                        "started_at": started, "ended_at": ended,
                        "source_path": path, "message_count": msg_count,
                        // Provenance fields (P3.5)
                        "source_id": source_id,
                        "origin_kind": kind,
                        "origin_host": origin_host,
                    }));
                }
                serde_json::json!({
                    "range": { "start": start_ts, "end": end_ts },
                    "total_sessions": sessions.len(),
                    "groups": groups,
                })
            }
        };
        return output_structured_value(output, fmt);
    }

    let start_dt = Utc
        .timestamp_millis_opt(start_ts)
        .single()
        .unwrap_or_else(Utc::now);
    let end_dt = Utc
        .timestamp_millis_opt(end_ts)
        .single()
        .unwrap_or_else(Utc::now);

    println!("\n📅 Activity Timeline");
    println!(
        "   {} to {}",
        start_dt.format("%Y-%m-%d %H:%M"),
        end_dt.format("%Y-%m-%d %H:%M")
    );
    println!("{}", "─".repeat(70));

    if sessions.is_empty() {
        println!("\n   No sessions found in this time range.\n");
        return Ok(());
    }

    let mut current_group = String::new();
    for (
        _id,
        agent,
        title,
        started,
        ended,
        _path,
        msg_count,
        source_id,
        origin_host,
        _origin_kind,
    ) in &sessions
    {
        let dt = Utc
            .timestamp_millis_opt(*started)
            .single()
            .unwrap_or_else(Utc::now);

        let group_key = match group_by {
            TimelineGrouping::Hour => dt.format("%Y-%m-%d %H:00").to_string(),
            TimelineGrouping::Day => dt.format("%Y-%m-%d (%A)").to_string(),
            TimelineGrouping::None => String::new(),
        };

        if group_key != current_group && group_by != TimelineGrouping::None {
            println!("\n  📆 {}", group_key);
            current_group = group_key;
        }

        let duration = ended.map(|e| {
            // Timestamps are in milliseconds, divide by 60_000 to get minutes
            let mins = (e - started) / 60_000;
            if mins < 60 {
                format!("{}m", mins)
            } else {
                format!("{}h{}m", mins / 60, mins % 60)
            }
        });

        let title_str = title.as_deref().unwrap_or("(untitled)");
        let title_preview: String = title_str.chars().take(40).collect();

        let agent_icon = match agent.as_str() {
            "claude_code" => "🟣",
            "codex" => "🟢",
            "gemini" => "🔵",
            "amp" => "🟡",
            "cursor" => "⚪",
            "pi_agent" => "🟠",
            _ => "⚫",
        };

        // Source badge for remote sessions (P3.2, P3.5)
        // Prefer origin_host if available, otherwise use source_id
        let source_badge = if source_id != "local" {
            let label = origin_host.as_deref().unwrap_or(source_id.as_str());
            format!(" [{}]", label)
        } else {
            String::new()
        };

        println!(
            "     {} {} {:>5} │ {:>3} msgs │ {}{}",
            dt.format("%H:%M"),
            agent_icon,
            duration.as_deref().unwrap_or(""),
            msg_count,
            title_preview,
            source_badge
        );
    }

    println!("\n{}", "─".repeat(70));
    println!("   Total: {} sessions\n", sessions.len());
    Ok(())
}

/// Handle sources subcommands (P5.x)
fn run_sources_command(cmd: SourcesCommand) -> CliResult<()> {
    match cmd {
        SourcesCommand::List { verbose, json } => {
            run_sources_list(verbose, json)?;
        }
        SourcesCommand::Add {
            url,
            name,
            preset,
            paths,
            no_test,
        } => {
            run_sources_add(&url, name, preset, paths, no_test)?;
        }
        SourcesCommand::Remove { name, purge, yes } => {
            run_sources_remove(&name, purge, yes)?;
        }
        SourcesCommand::Doctor { source, json } => {
            run_sources_doctor(source.as_deref(), json)?;
        }
        SourcesCommand::Sync {
            source,
            no_index,
            verbose,
            dry_run,
            json,
        } => {
            run_sources_sync(source, no_index, verbose, dry_run, json)?;
        }
        SourcesCommand::Mappings(action) => {
            run_mappings_command(action)?;
        }
        SourcesCommand::Discover {
            preset,
            skip_existing,
            json,
        } => {
            run_sources_discover(&preset, skip_existing, json)?;
        }
        SourcesCommand::Setup {
            dry_run,
            non_interactive,
            hosts,
            skip_install,
            skip_index,
            skip_sync,
            timeout,
            resume,
            verbose,
            json,
        } => {
            run_sources_setup(sources::setup::SetupOptions {
                dry_run,
                non_interactive: non_interactive || json,
                hosts,
                skip_install,
                skip_index,
                skip_sync,
                timeout,
                resume,
                verbose,
                json,
            })?;
        }
    }
    Ok(())
}

/// List configured sources (P5.3)
fn run_sources_list(verbose: bool, json: bool) -> CliResult<()> {
    use crate::sources::config::SourcesConfig;

    let config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: Some("Run 'cass sources add' to configure a source".into()),
        retryable: false,
    })?;

    // Get config path for display
    let config_path = SourcesConfig::config_path()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "unknown".into());

    let structured_format = if json {
        Some(RobotFormat::Json)
    } else {
        robot_format_from_env()
    }
    .map(|fmt| {
        if matches!(fmt, RobotFormat::Sessions) {
            RobotFormat::Compact
        } else {
            fmt
        }
    });

    if let Some(fmt) = structured_format {
        let sources_json: Vec<serde_json::Value> = config
            .sources
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "type": s.source_type.as_str(),
                    "host": s.host,
                    "paths": s.paths,
                    "sync_schedule": s.sync_schedule.to_string(),
                    "platform": s.platform.map(|p| p.to_string()),
                })
            })
            .collect();

        let output = serde_json::json!({
            "config_path": config_path,
            "sources": sources_json,
            "total": config.sources.len(),
        });
        return output_structured_value(output, fmt);
    }

    println!("CASS Sources Configuration");
    println!("===========================");
    println!("Config: {config_path}");
    println!();

    if config.sources.is_empty() {
        println!("No sources configured.");
        println!();
        println!("To add a source, run:");
        println!("  cass sources add user@hostname --preset macos-defaults");
        return Ok(());
    }

    if verbose {
        // Verbose output with full details
        for source in &config.sources {
            println!("Source: {}", source.name);
            println!("  Type: {}", source.source_type);
            if let Some(ref host) = source.host {
                println!("  Host: {host}");
            }
            println!("  Schedule: {}", source.sync_schedule);
            if let Some(platform) = source.platform {
                println!("  Platform: {platform}");
            }
            if !source.paths.is_empty() {
                println!("  Paths:");
                for path in &source.paths {
                    println!("    - {path}");
                }
            }
            if !source.path_mappings.is_empty() {
                println!("  Path Mappings:");
                for mapping in &source.path_mappings {
                    if let Some(agents) = &mapping.agents {
                        println!(
                            "    {} -> {} (agents: {})",
                            mapping.from,
                            mapping.to,
                            agents.join(", ")
                        );
                    } else {
                        println!("    {} -> {}", mapping.from, mapping.to);
                    }
                }
            }
            println!();
        }
    } else {
        // Table output
        println!("  {:15} {:8} {:30} {:>5}", "NAME", "TYPE", "HOST", "PATHS");
        println!("  {}", "-".repeat(62));
        for source in &config.sources {
            let host = source.host.as_deref().unwrap_or("-");
            let host_truncated = if host.len() > 30 {
                format!("{}...", &host[..27])
            } else {
                host.to_string()
            };
            println!(
                "  {:15} {:8} {:30} {:>5}",
                source.name,
                source.source_type.as_str(),
                host_truncated,
                source.paths.len()
            );
        }
        println!();
    }

    println!("Total: {} source(s)", config.sources.len());

    Ok(())
}

/// Add a new remote source (P5.2)
fn run_sources_add(
    url: &str,
    name: Option<String>,
    preset: Option<String>,
    paths_arg: Vec<String>,
    no_test: bool,
) -> CliResult<()> {
    use crate::sources::config::{Platform, SourceDefinition, SourcesConfig, get_preset_paths};
    use crate::sources::provenance::SourceKind;

    // Parse URL to extract host
    let (host, source_id) = parse_source_url(url, name.as_deref())?;

    // Determine paths: preset, explicit args, or error
    let paths = if let Some(ref preset_name) = preset {
        get_preset_paths(preset_name).map_err(|e| CliError {
            code: 10,
            kind: "config",
            message: format!("Invalid preset: {e}"),
            hint: Some("Valid presets: macos-defaults, linux-defaults".into()),
            retryable: false,
        })?
    } else if !paths_arg.is_empty() {
        paths_arg
    } else {
        return Err(CliError {
            code: 10,
            kind: "config",
            message: "No paths specified".into(),
            hint: Some("Use --preset macos-defaults or --path <path> to specify paths".into()),
            retryable: false,
        });
    };

    // Test SSH connectivity unless --no-test
    if !no_test {
        println!("Testing SSH connectivity to {host}...");
        test_ssh_connectivity(&host)?;
        println!("  Connected successfully");
    }

    // Load existing config
    let mut config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: None,
        retryable: false,
    })?;

    // Check for duplicate
    if config.sources.iter().any(|s| s.name == source_id) {
        return Err(CliError {
            code: 10,
            kind: "config",
            message: format!("Source '{source_id}' already exists"),
            hint: Some("Use a different --name or remove the existing source first".into()),
            retryable: false,
        });
    }

    // Determine platform from preset
    let platform = preset.as_ref().and_then(|p| {
        if p.contains("macos") {
            Some(Platform::Macos)
        } else if p.contains("linux") {
            Some(Platform::Linux)
        } else {
            None
        }
    });

    // Create source definition
    let source = SourceDefinition {
        name: source_id.clone(),
        source_type: SourceKind::Ssh,
        host: Some(host.clone()),
        paths: paths.clone(),
        platform,
        ..Default::default()
    };

    // Add and save
    config.add_source(source).map_err(|e| CliError {
        code: 10,
        kind: "config",
        message: format!("Failed to add source: {e}"),
        hint: None,
        retryable: false,
    })?;

    config.save().map_err(|e| CliError {
        code: 11,
        kind: "config",
        message: format!("Failed to save config: {e}"),
        hint: Some("Check file permissions on config directory".into()),
        retryable: false,
    })?;

    // Success output
    let config_path = SourcesConfig::config_path()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/cass/sources.toml".into());

    println!();
    println!("Added source '{source_id}'");
    println!("  Host: {host}");
    println!("  Paths: {} path(s)", paths.len());
    println!("  Config: {config_path}");
    println!();
    println!("Next steps:");
    println!("  cass sources sync {source_id}   # Fetch sessions from this source");
    println!("  cass sources list               # View all configured sources");

    Ok(())
}

/// Parse source URL and extract host and source_id.
/// Accepts formats: user@host, ssh://user@host
fn parse_source_url(url: &str, name: Option<&str>) -> Result<(String, String), CliError> {
    // Strip ssh:// prefix if present
    let host = url.strip_prefix("ssh://").unwrap_or(url);

    // Basic hardening: avoid whitespace/control chars and option-injection.
    if host.trim().is_empty()
        || host.starts_with('-')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(CliError {
            code: 10,
            kind: "config",
            message: "Invalid host: contains whitespace/control characters or starts with '-'"
                .into(),
            hint: Some("Use format: user@hostname (e.g., user@laptop.local)".into()),
            retryable: false,
        });
    }

    // Validate URL contains @
    if !host.contains('@') {
        return Err(CliError {
            code: 10,
            kind: "config",
            message: "Invalid URL format: missing username".into(),
            hint: Some("Use format: user@hostname (e.g., user@laptop.local)".into()),
            retryable: false,
        });
    }

    // Generate source_id from hostname if not provided
    let source_id = if let Some(n) = name {
        n.to_string()
    } else {
        // Extract hostname part (after @)
        let hostname_part = host.split('@').nth(1).unwrap_or(host);
        // Take first segment before any dots
        hostname_part
            .split('.')
            .next()
            .unwrap_or(hostname_part)
            .to_string()
    };

    Ok((host.to_string(), source_id))
}

/// Test SSH connectivity to a host.
fn test_ssh_connectivity(host: &str) -> CliResult<()> {
    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "--",
            host,
            "echo",
            "ok",
        ])
        .output()
        .map_err(|e| CliError {
            code: 12,
            kind: "ssh",
            message: format!("Failed to run ssh command: {e}"),
            hint: Some("Ensure ssh is installed and in PATH".into()),
            retryable: false,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError {
            code: 12,
            kind: "ssh",
            message: format!("SSH connection failed to {host}"),
            hint: Some(format!(
                "Error: {}. Ensure SSH key is set up for this host.",
                stderr.trim()
            )),
            retryable: true,
        });
    }

    Ok(())
}

/// Remove a configured source (P5.7)
fn run_sources_remove(name: &str, purge: bool, skip_confirm: bool) -> CliResult<()> {
    use crate::sources::config::SourcesConfig;

    // Load existing config
    let mut config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: None,
        retryable: false,
    })?;

    // Check source exists
    if !config.sources.iter().any(|s| s.name == name) {
        return Err(CliError {
            code: 13,
            kind: "not_found",
            message: format!("Source '{name}' not found"),
            hint: Some("Run 'cass sources list' to see configured sources".into()),
            retryable: false,
        });
    }

    // Confirmation prompt
    if !skip_confirm {
        let msg = if purge {
            format!(
                "Remove source '{name}' and delete indexed data? This cannot be undone. [y/N]: "
            )
        } else {
            format!("Remove source '{name}' from configuration? [y/N]: ")
        };
        print!("{msg}");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| CliError {
                code: 14,
                kind: "io",
                message: format!("Failed to read input: {e}"),
                hint: None,
                retryable: false,
            })?;

        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Remove from config
    config.remove_source(name);
    config.save().map_err(|e| CliError {
        code: 11,
        kind: "config",
        message: format!("Failed to save config: {e}"),
        hint: Some("Check file permissions on config directory".into()),
        retryable: false,
    })?;

    println!("Removed '{name}' from configuration.");

    // Handle purge
    if purge {
        // Find and remove synced data directory
        let data_dir = default_data_dir();
        let source_dir = data_dir.join("remotes").join(name);
        if source_dir.exists() {
            std::fs::remove_dir_all(&source_dir).map_err(|e| CliError {
                code: 15,
                kind: "io",
                message: format!("Failed to delete synced data: {e}"),
                hint: None,
                retryable: false,
            })?;
            println!("Deleted synced data at {}", source_dir.display());
        }
        println!("Note: Run 'cass reindex' to remove entries from the search index.");
    }

    Ok(())
}

/// Diagnostic check result for sources doctor command (P5.6)
#[derive(serde::Serialize)]
struct DiagnosticCheck {
    name: String,
    status: String, // "pass", "warn", "fail"
    message: String,
    remediation: Option<String>,
}

/// Aggregated diagnostics for a single source (P5.6)
#[derive(serde::Serialize)]
struct SourceDiagnostics {
    source_id: String,
    checks: Vec<DiagnosticCheck>,
    passed: usize,
    warnings: usize,
    failed: usize,
}

/// Diagnose source connectivity and configuration issues (P5.6)
fn run_sources_doctor(source_filter: Option<&str>, json_output: bool) -> CliResult<()> {
    use crate::sources::config::SourcesConfig;
    use colored::Colorize;

    let config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: Some("Run 'cass sources add' to configure a source".into()),
        retryable: false,
    })?;

    if config.sources.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "error": "No sources configured",
                    "sources": []
                })
            );
        } else {
            println!("No remote sources configured.");
            println!("Run 'cass sources add <url>' to add one.");
        }
        return Ok(());
    }

    // Filter sources if specified
    let sources_to_check: Vec<_> = config
        .sources
        .iter()
        .filter(|s| source_filter.is_none() || source_filter == Some(s.name.as_str()))
        .collect();

    if sources_to_check.is_empty() {
        return Err(CliError {
            code: 13,
            kind: "not_found",
            message: format!("Source '{}' not found", source_filter.unwrap_or("unknown")),
            hint: Some("Run 'cass sources list' to see configured sources".into()),
            retryable: false,
        });
    }

    let mut all_diagnostics = Vec::new();

    for source in sources_to_check {
        let mut checks = Vec::new();

        // Check 1: SSH connectivity
        let host = source.host.as_deref().unwrap_or("unknown");
        let ssh_check = check_ssh_connectivity(host);
        checks.push(ssh_check);

        // Check 2: rsync availability on remote
        let rsync_check = check_rsync_available(host);
        checks.push(rsync_check);

        // Check 3: Remote paths exist
        for path in &source.paths {
            let path_check = check_remote_path(host, path);
            checks.push(path_check);
        }

        // Check 4: Local storage writable
        let storage_check = check_local_storage(&source.name);
        checks.push(storage_check);

        // Compute summary
        let passed = checks.iter().filter(|c| c.status == "pass").count();
        let warnings = checks.iter().filter(|c| c.status == "warn").count();
        let failed = checks.iter().filter(|c| c.status == "fail").count();

        all_diagnostics.push(SourceDiagnostics {
            source_id: source.name.clone(),
            checks,
            passed,
            warnings,
            failed,
        });
    }

    // Output results
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&all_diagnostics).unwrap()
        );
    } else {
        for diag in &all_diagnostics {
            println!();
            println!("{}", format!("Checking source: {}", diag.source_id).bold());
            println!();

            for check in &diag.checks {
                let icon = match check.status.as_str() {
                    "pass" => "✓".green(),
                    "warn" => "⚠".yellow(),
                    "fail" => "✗".red(),
                    _ => "?".normal(),
                };
                let name_styled = match check.status.as_str() {
                    "pass" => check.name.green(),
                    "warn" => check.name.yellow(),
                    "fail" => check.name.red(),
                    _ => check.name.normal(),
                };
                println!("  {} {}", icon, name_styled);
                println!("    {}", check.message.dimmed());
                if let Some(ref hint) = check.remediation {
                    println!("    {}: {}", "Hint".cyan(), hint);
                }
            }

            println!();
            println!(
                "Summary: {} passed, {} warnings, {} failed",
                diag.passed.to_string().green(),
                diag.warnings.to_string().yellow(),
                diag.failed.to_string().red()
            );
        }
    }

    // Set exit code based on results
    let total_failed: usize = all_diagnostics.iter().map(|d| d.failed).sum();
    if total_failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Check SSH connectivity to a host
fn check_ssh_connectivity(host: &str) -> DiagnosticCheck {
    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "--",
            host,
            "true",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => DiagnosticCheck {
            name: "SSH Connectivity".into(),
            status: "pass".into(),
            message: format!("Connected to {} successfully", host),
            remediation: None,
        },
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let remediation = if stderr.contains("Permission denied") {
                Some("Ensure SSH key is added to remote authorized_keys".into())
            } else if stderr.contains("Connection refused") {
                Some("Verify SSH server is running on remote host".into())
            } else if stderr.contains("Could not resolve") {
                Some("Check hostname is correct and DNS resolves".into())
            } else {
                Some("Check SSH configuration and network connectivity".into())
            };
            DiagnosticCheck {
                name: "SSH Connectivity".into(),
                status: "fail".into(),
                message: stderr.trim().to_string(),
                remediation,
            }
        }
        Err(e) => DiagnosticCheck {
            name: "SSH Connectivity".into(),
            status: "fail".into(),
            message: format!("Failed to run ssh: {}", e),
            remediation: Some("Ensure SSH client is installed and in PATH".into()),
        },
    }
}

/// Check rsync availability on remote
fn check_rsync_available(host: &str) -> DiagnosticCheck {
    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "--",
            host,
            "rsync",
            "--version",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let version = stdout
                .lines()
                .next()
                .unwrap_or("version unknown")
                .to_string();
            DiagnosticCheck {
                name: "rsync Available".into(),
                status: "pass".into(),
                message: version,
                remediation: None,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            DiagnosticCheck {
                name: "rsync Available".into(),
                status: "fail".into(),
                message: format!("rsync not found: {}", stderr.trim()),
                remediation: Some("Install rsync on the remote host".into()),
            }
        }
        Err(e) => DiagnosticCheck {
            name: "rsync Available".into(),
            status: "warn".into(),
            message: format!("Could not check rsync: {}", e),
            remediation: Some("SSH connectivity may have failed".into()),
        },
    }
}

fn sh_quote(value: &str) -> String {
    if value.is_empty() {
        "''".to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// Check if a remote path exists
fn check_remote_path(host: &str, path: &str) -> DiagnosticCheck {
    let quoted = sh_quote(path);
    let cmd = format!("test -d {quoted} && ls -1 {quoted} | wc -l");
    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "--",
            host,
            "sh",
            "-c",
            &cmd,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let count = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(0);
            DiagnosticCheck {
                name: format!("Remote Path: {}", path),
                status: if count > 0 { "pass" } else { "warn" }.into(),
                message: if count > 0 {
                    format!("Path exists, {} items found", count)
                } else {
                    "Path exists but is empty".into()
                },
                remediation: if count == 0 {
                    Some("No agent sessions on this machine yet".into())
                } else {
                    None
                },
            }
        }
        Ok(_) => DiagnosticCheck {
            name: format!("Remote Path: {}", path),
            status: "fail".into(),
            message: "Path does not exist".into(),
            remediation: Some("Remove this path or create it on the remote".into()),
        },
        Err(e) => DiagnosticCheck {
            name: format!("Remote Path: {}", path),
            status: "warn".into(),
            message: format!("Could not check path: {}", e),
            remediation: Some("SSH connectivity may have failed".into()),
        },
    }
}

/// Check if local storage directory is writable
fn check_local_storage(source_name: &str) -> DiagnosticCheck {
    let data_dir = default_data_dir();
    let source_dir = data_dir.join("remotes").join(source_name);

    // Try to create the directory if it doesn't exist
    if !source_dir.exists() {
        if std::fs::create_dir_all(&source_dir).is_ok() {
            return DiagnosticCheck {
                name: "Local Storage".into(),
                status: "pass".into(),
                message: format!("{} is writable", source_dir.display()),
                remediation: None,
            };
        } else {
            return DiagnosticCheck {
                name: "Local Storage".into(),
                status: "fail".into(),
                message: format!("Cannot create {}", source_dir.display()),
                remediation: Some("Check file permissions on data directory".into()),
            };
        }
    }

    // Directory exists, check if writable
    let test_file = source_dir.join(".doctor_test");
    if std::fs::write(&test_file, b"test").is_ok() {
        let _ = std::fs::remove_file(&test_file);
        DiagnosticCheck {
            name: "Local Storage".into(),
            status: "pass".into(),
            message: format!("{} is writable", source_dir.display()),
            remediation: None,
        }
    } else {
        DiagnosticCheck {
            name: "Local Storage".into(),
            status: "fail".into(),
            message: format!("{} is not writable", source_dir.display()),
            remediation: Some("Check file permissions on data directory".into()),
        }
    }
}

/// Sync sessions from remote sources (P5.5)
fn run_sources_sync(
    source_filter: Option<Vec<String>>,
    no_index: bool,
    verbose: bool,
    dry_run: bool,
    json_output: bool,
) -> CliResult<()> {
    use crate::sources::config::SourcesConfig;
    use crate::sources::sync::{SyncEngine, SyncStatus};
    use colored::Colorize;

    let config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: Some("Run 'cass sources add' to configure a source".into()),
        retryable: false,
    })?;

    // Filter to remote sources only
    let remote_sources: Vec<_> = config.remote_sources().collect();

    if remote_sources.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "no_sources",
                    "message": "No remote sources configured"
                })
            );
        } else {
            println!(
                "{}",
                "No remote sources configured. Run 'cass sources add' first.".yellow()
            );
        }
        return Ok(());
    }

    // Filter to specified sources if provided
    let sources_to_sync: Vec<_> = if let Some(ref names) = source_filter {
        remote_sources
            .into_iter()
            .filter(|s| names.contains(&s.name))
            .collect()
    } else {
        remote_sources
    };

    if sources_to_sync.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "no_match",
                    "message": "No sources match the filter"
                })
            );
        } else {
            println!("{}", "No sources match the specified filter.".yellow());
        }
        return Ok(());
    }

    // Use the same data dir as the rest of the app (DB, Tantivy index, remotes mirror).
    // Override with `CASS_DATA_DIR` or command-specific `--data-dir` flags elsewhere.
    let data_dir = default_data_dir();

    // Create sync engine
    let engine = SyncEngine::new(&data_dir);

    // Load existing sync status
    let mut status = SyncStatus::load(&data_dir).unwrap_or_default();

    if dry_run && !json_output {
        println!("{}", "DRY RUN - no changes will be made".cyan().bold());
        println!();
    }

    let mut all_reports = Vec::new();
    let mut total_files = 0u64;
    let mut total_bytes = 0u64;

    for source in &sources_to_sync {
        if !json_output {
            println!(
                "{} {}...",
                "Syncing".cyan().bold(),
                source.name.white().bold()
            );
        }

        if dry_run {
            // In dry run, just show what would be synced
            if !json_output {
                for path in &source.paths {
                    println!("  {} {}", "Would sync:".dimmed(), path);
                }
                println!();
            }
            continue;
        }

        // Perform actual sync
        let report = match engine.sync_source(source) {
            Ok(r) => r,
            Err(e) => {
                if json_output {
                    all_reports.push(serde_json::json!({
                        "source": source.name,
                        "status": "error",
                        "error": e.to_string()
                    }));
                } else {
                    println!("  {} {}", "Error:".red().bold(), e.to_string().red());
                }
                continue;
            }
        };

        // Update status
        status.update(&source.name, &report);

        // Print results
        if json_output {
            all_reports.push(serde_json::json!({
                "source": source.name,
                "status": if report.all_succeeded { "success" } else { "partial" },
                "method": report.method.to_string(),
                "paths": report.path_results.iter().map(|r| serde_json::json!({
                    "path": r.remote_path,
                    "success": r.success,
                    "files": r.files_transferred,
                    "bytes": r.bytes_transferred,
                    "error": r.error,
                })).collect::<Vec<_>>(),
                "total_files": report.total_files(),
                "total_bytes": report.total_bytes(),
                "duration_ms": report.total_duration_ms,
            }));
        } else {
            for result in &report.path_results {
                if result.success {
                    if verbose || result.files_transferred > 0 {
                        println!(
                            "  {}: {} files ({} bytes)",
                            result.remote_path.dimmed(),
                            result.files_transferred.to_string().green(),
                            format_bytes(result.bytes_transferred)
                        );
                    } else {
                        println!(
                            "  {}: {}",
                            result.remote_path.dimmed(),
                            "up to date".green()
                        );
                    }
                } else {
                    println!(
                        "  {}: {}",
                        result.remote_path.dimmed(),
                        result.error.as_deref().unwrap_or("failed").red()
                    );
                }
            }
            println!(
                "  {} {} files, {}",
                "Total:".dimmed(),
                report.total_files(),
                format_bytes(report.total_bytes())
            );
            println!();
        }

        total_files += report.total_files();
        total_bytes += report.total_bytes();
    }

    // Save sync status
    if !dry_run && let Err(e) = status.save(&data_dir) {
        tracing::warn!("Failed to save sync status: {}", e);
    }

    // Output summary
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "complete",
                "dry_run": dry_run,
                "sources": all_reports,
                "total_files": total_files,
                "total_bytes": total_bytes,
                "will_reindex": !no_index && !dry_run,
            }))
            .unwrap_or_default()
        );
    }

    // Trigger re-index if requested
    if !no_index && !dry_run && total_files > 0 {
        if !json_output {
            println!(
                "{} {} new files...",
                "Re-indexing".cyan().bold(),
                total_files
            );
        }

        // Call indexer to include synced sessions
        let progress = if json_output {
            ProgressResolved::None
        } else if std::io::stdout().is_terminal() {
            ProgressResolved::Bars
        } else {
            ProgressResolved::Plain
        };

        run_index_with_data(
            None,           // db_override (uses data_dir default)
            false,          // full
            false,          // force_rebuild
            false,          // watch
            None,           // watch_once
            Some(data_dir), // data_dir
            false,          // semantic
            false,          // build_hnsw
            "fastembed".to_string(),
            progress,
            json_output,
            None, // idempotency_key
        )?;
    }

    Ok(())
}

/// Auto-discover SSH hosts from ~/.ssh/config (P5.6)
fn run_sources_discover(preset: &str, skip_existing: bool, json_output: bool) -> CliResult<()> {
    use crate::sources::config::{SourcesConfig, discover_ssh_hosts, get_preset_paths};
    use colored::Colorize;

    // Get preset paths
    let preset_paths = get_preset_paths(preset).map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Invalid preset: {e}"),
        hint: Some("Valid presets: linux-defaults, macos-defaults".into()),
        retryable: false,
    })?;

    // Discover SSH hosts
    let discovered = discover_ssh_hosts();

    if discovered.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "no_hosts",
                    "message": "No SSH hosts found in ~/.ssh/config"
                })
            );
        } else {
            println!("{}", "No SSH hosts found in ~/.ssh/config".yellow());
        }
        return Ok(());
    }

    // Load existing config to check for duplicates
    let existing_config = SourcesConfig::load().ok();
    let existing_names: std::collections::HashSet<String> = existing_config
        .as_ref()
        .map(|c| c.remote_sources().map(|s| s.name.clone()).collect())
        .unwrap_or_default();

    // Filter hosts
    let hosts_to_add: Vec<_> = if skip_existing {
        discovered
            .into_iter()
            .filter(|h| !existing_names.contains(&h.name))
            .collect()
    } else {
        discovered
    };

    if hosts_to_add.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "all_existing",
                    "message": "All discovered hosts are already configured"
                })
            );
        } else {
            println!(
                "{}",
                "All discovered SSH hosts are already configured as sources.".green()
            );
        }
        return Ok(());
    }

    // Output discovered hosts
    if json_output {
        let hosts_json: Vec<_> = hosts_to_add
            .iter()
            .map(|h| {
                serde_json::json!({
                    "name": h.name,
                    "hostname": h.hostname,
                    "user": h.user,
                    "port": h.port,
                    "identity_file": h.identity_file,
                    "already_configured": existing_names.contains(&h.name),
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "discovered",
                "preset": preset,
                "preset_paths": preset_paths,
                "hosts": hosts_json,
                "count": hosts_to_add.len(),
            }))
            .unwrap_or_default()
        );
    } else {
        println!(
            "{} {} SSH hosts from ~/.ssh/config:\n",
            "Discovered".cyan().bold(),
            hosts_to_add.len()
        );

        for host in &hosts_to_add {
            let status = if existing_names.contains(&host.name) {
                " (already configured)".yellow().to_string()
            } else {
                String::new()
            };

            println!("  {} {}{}", "→".cyan(), host.name.white().bold(), status);

            if let Some(hostname) = &host.hostname {
                println!("      Hostname: {}", hostname.dimmed());
            }
            if let Some(user) = &host.user {
                println!("      User: {}", user.dimmed());
            }
            if let Some(port) = host.port
                && port != 22
            {
                println!("      Port: {}", port.to_string().dimmed());
            }
        }

        println!();
        println!("{} {}", "Preset:".dimmed(), preset);
        println!("{}", "Paths to sync:".dimmed());
        for path in &preset_paths {
            println!("  - {}", path.dimmed());
        }

        println!();
        println!("{} To add a host as a source, use:", "Next step:".yellow());
        println!(
            "  {}",
            "cass sources add --name <host> --host <host> --paths '~/.claude/projects'".dimmed()
        );
    }

    Ok(())
}

/// Run the interactive sources setup wizard
fn run_sources_setup(opts: sources::setup::SetupOptions) -> CliResult<()> {
    use sources::setup::{SetupError, run_setup};

    match run_setup(&opts) {
        Ok(result) => {
            if opts.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "success",
                        "dry_run": result.dry_run,
                        "sources_added": result.sources_added,
                        "hosts_installed": result.hosts_installed,
                        "hosts_indexed": result.hosts_indexed,
                        "total_sessions": result.total_sessions,
                    })
                );
            }
            Ok(())
        }
        Err(SetupError::Cancelled) => {
            if opts.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "cancelled",
                        "message": "Setup cancelled by user"
                    })
                );
            }
            Ok(())
        }
        Err(SetupError::Interrupted) => {
            // Progress saved, exit cleanly
            std::process::exit(130);
        }
        Err(SetupError::NoHosts) => {
            if opts.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "no_hosts",
                        "message": "No SSH hosts found or selected"
                    })
                );
            }
            Ok(())
        }
        Err(e) => Err(CliError {
            code: 9,
            kind: "setup",
            message: format!("Setup failed: {e}"),
            hint: Some("Run with --verbose for more details".into()),
            retryable: true,
        }),
    }
}

/// Handle models subcommands
fn run_models_command(cmd: ModelsCommand) -> CliResult<()> {
    match cmd {
        ModelsCommand::Status { json } => run_models_status(json),
        ModelsCommand::Install {
            model,
            mirror,
            from_file,
            yes,
            data_dir,
        } => run_models_install(
            &model,
            mirror.as_deref(),
            from_file.as_deref(),
            yes,
            data_dir,
        ),
        ModelsCommand::Verify {
            repair,
            data_dir,
            json,
        } => run_models_verify(repair, data_dir, json),
        ModelsCommand::Remove {
            model,
            yes,
            data_dir,
        } => run_models_remove(&model, yes, data_dir),
        ModelsCommand::CheckUpdate { json, data_dir } => run_models_check_update(json, data_dir),
    }
}

/// Show semantic model installation status
fn run_models_status(json_output: bool) -> CliResult<()> {
    use crate::search::fastembed_embedder::FastEmbedder;
    use crate::search::model_download::{ModelManifest, ModelState, check_model_installed};

    let data_dir = default_data_dir();
    let model_dir = FastEmbedder::default_model_dir(&data_dir);
    let manifest = ModelManifest::minilm_v2();

    let state = check_model_installed(&model_dir);
    let total_size = manifest.total_size();
    let total_size_mb = total_size as f64 / 1_048_576.0;

    // Check for file sizes on disk
    let mut installed_size: u64 = 0;
    let mut file_info: Vec<serde_json::Value> = Vec::new();
    for mfile in &manifest.files {
        let file_path = model_dir.join(&mfile.name);
        let exists = file_path.is_file();
        let size = if exists {
            file_path.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        if exists {
            installed_size += size;
        }
        file_info.push(serde_json::json!({
            "name": mfile.name,
            "expected_size": mfile.size,
            "actual_size": size,
            "exists": exists,
            "size_match": exists && size == mfile.size,
        }));
    }

    if json_output {
        let output = serde_json::json!({
            "model_id": manifest.id,
            "model_dir": model_dir.display().to_string(),
            "state": match &state {
                ModelState::Ready => "ready",
                ModelState::NotInstalled => "not_installed",
                ModelState::NeedsConsent => "needs_consent",
                ModelState::Downloading { .. } => "downloading",
                ModelState::Verifying => "verifying",
                ModelState::Disabled { .. } => "disabled",
                ModelState::VerificationFailed { .. } => "verification_failed",
                ModelState::UpdateAvailable { .. } => "update_available",
                ModelState::Cancelled => "cancelled",
            },
            "state_detail": state.summary(),
            "revision": manifest.revision,
            "license": manifest.license,
            "total_size_bytes": total_size,
            "installed_size_bytes": installed_size,
            "files": file_info,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
        use colored::Colorize;

        println!("Semantic Search Model Status");
        println!("============================");
        println!();
        println!("Model:    {} ({})", manifest.id, manifest.license);
        println!(
            "Revision: {}",
            manifest.revision.get(..12).unwrap_or(&manifest.revision)
        );
        println!("Location: {}", model_dir.display());
        println!("Size:     {:.1} MB", total_size_mb);
        println!();

        let status_str = match &state {
            ModelState::Ready => "Ready".green().to_string(),
            ModelState::NotInstalled => "Not Installed".yellow().to_string(),
            ModelState::NeedsConsent => "Needs Consent".yellow().to_string(),
            ModelState::Downloading { progress_pct, .. } => {
                format!("Downloading ({}%)", progress_pct)
                    .cyan()
                    .to_string()
            }
            ModelState::Verifying => "Verifying".cyan().to_string(),
            ModelState::Disabled { reason } => format!("Disabled: {}", reason).red().to_string(),
            ModelState::VerificationFailed { reason } => {
                format!("Verification Failed: {}", reason).red().to_string()
            }
            ModelState::UpdateAvailable {
                current_revision,
                latest_revision,
            } => format!(
                "Update Available ({} -> {})",
                current_revision.get(..8).unwrap_or(current_revision),
                latest_revision.get(..8).unwrap_or(latest_revision)
            )
            .yellow()
            .to_string(),
            ModelState::Cancelled => "Cancelled".yellow().to_string(),
        };
        println!("Status: {}", status_str);
        println!();

        // Show files
        println!("Files:");
        for mfile in &manifest.files {
            let file_path = model_dir.join(&mfile.name);
            let exists = file_path.is_file();
            let size = if exists {
                file_path.metadata().map(|m| m.len()).unwrap_or(0)
            } else {
                0
            };
            let size_mb = mfile.size as f64 / 1_048_576.0;

            let status = if exists && size == mfile.size {
                "✓".green().to_string()
            } else if exists {
                "⚠".yellow().to_string()
            } else {
                "✗".red().to_string()
            };
            println!("  {} {} ({:.1} MB)", status, mfile.name, size_mb);
        }
        println!();

        // Suggestions based on state
        match state {
            ModelState::NotInstalled | ModelState::NeedsConsent => {
                println!("To install the model, run:");
                println!("  cass models install");
            }
            ModelState::VerificationFailed { .. } => {
                println!("To repair the model, run:");
                println!("  cass models verify --repair");
            }
            ModelState::UpdateAvailable { .. } => {
                println!("To update the model, run:");
                println!("  cass models install");
            }
            ModelState::Ready => {
                println!("Model is ready for semantic search.");
            }
            _ => {}
        }
    }

    Ok(())
}

/// Download and install the semantic search model
fn run_models_install(
    model_name: &str,
    mirror: Option<&str>,
    from_file: Option<&Path>,
    skip_confirm: bool,
    data_dir_override: Option<PathBuf>,
) -> CliResult<()> {
    use crate::search::fastembed_embedder::FastEmbedder;
    use crate::search::model_download::{ModelDownloader, ModelManifest, check_model_installed};
    use colored::Colorize;
    use indicatif::{ProgressBar, ProgressStyle};

    // Only support the default model for now
    if model_name != "all-minilm-l6-v2" {
        return Err(CliError {
            code: 20,
            kind: "model",
            message: format!(
                "Unknown model '{}'. Only 'all-minilm-l6-v2' is supported.",
                model_name
            ),
            hint: Some("Use 'cass models status' to see available models".into()),
            retryable: false,
        });
    }

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let model_dir = FastEmbedder::default_model_dir(&data_dir);
    let manifest = ModelManifest::minilm_v2();

    // Check if from_file is specified
    if let Some(file_path) = from_file {
        return Err(CliError {
            code: 21,
            kind: "model",
            message: format!("--from-file not yet implemented: {}", file_path.display()),
            hint: Some("Download from HuggingFace instead".into()),
            retryable: false,
        });
    }

    // Check if mirror is specified
    if let Some(mirror_url) = mirror {
        return Err(CliError {
            code: 21,
            kind: "model",
            message: format!("--mirror not yet implemented: {}", mirror_url),
            hint: Some("Download from HuggingFace instead".into()),
            retryable: false,
        });
    }

    // Check current state
    let state = check_model_installed(&model_dir);
    if state.is_ready() {
        println!("{} Model is already installed and verified.", "✓".green());
        println!("  Location: {}", model_dir.display());
        return Ok(());
    }

    let total_size = manifest.total_size();
    let total_size_mb = total_size as f64 / 1_048_576.0;

    // Confirm download unless -y flag
    if !skip_confirm {
        println!("Semantic Search Model Installation");
        println!("===================================");
        println!();
        println!("Model:   {} ({})", manifest.id, manifest.license);
        println!("Size:    {:.1} MB", total_size_mb);
        println!("Source:  HuggingFace ({})", manifest.repo);
        println!();
        println!("This will download the model from HuggingFace.");
        print!("Continue? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if !input.trim().eq_ignore_ascii_case("y") && !input.trim().eq_ignore_ascii_case("yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Create model directory parent
    if let Some(parent) = model_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CliError {
            code: 22,
            kind: "io",
            message: format!("Failed to create model directory: {}", e),
            hint: None,
            retryable: false,
        })?;
    }

    println!();
    println!("Downloading model files...");

    // Set up progress bar
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%)")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );

    let downloader = ModelDownloader::new(model_dir.clone());
    let pb_clone = pb.clone();
    let manifest_clone = manifest.clone();

    // Run download on a fresh OS thread to avoid nested Tokio runtime drops from reqwest::blocking.
    let result = std::thread::spawn(move || {
        downloader.download(
            &manifest_clone,
            Some(Box::new(move |progress| {
                pb_clone.set_position(progress.total_bytes);
            })),
        )
    })
    .join()
    .map_err(|_| {
        pb.abandon_with_message("Download failed");
        CliError {
            code: 23,
            kind: "download",
            message: "Model download thread panicked".to_string(),
            hint: Some("Retry the command; if it persists, report the panic output.".into()),
            retryable: true,
        }
    })?;

    match result {
        Ok(()) => {
            pb.finish_with_message("Download complete");
            println!();
            println!("{} Model installed successfully!", "✓".green());
            println!("  Location: {}", model_dir.display());
            println!();
            println!("Semantic search is now available. Run 'cass search' to try it out.");
            Ok(())
        }
        Err(e) => {
            pb.abandon_with_message("Download failed");
            Err(CliError {
                code: 23,
                kind: "download",
                message: format!("Model download failed: {}", e),
                hint: Some("Check your network connection and try again".into()),
                retryable: true,
            })
        }
    }
}

/// Verify model file integrity
fn run_models_verify(
    repair: bool,
    data_dir_override: Option<PathBuf>,
    json_output: bool,
) -> CliResult<()> {
    use crate::search::fastembed_embedder::FastEmbedder;
    use crate::search::model_download::{ModelManifest, compute_sha256};
    use colored::Colorize;

    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let model_dir = FastEmbedder::default_model_dir(&data_dir);
    let manifest = ModelManifest::minilm_v2();

    if !model_dir.is_dir() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "not_installed",
                    "model_dir": model_dir.display().to_string(),
                    "error": "Model directory does not exist",
                }))
                .unwrap_or_default()
            );
        } else {
            println!("{} Model is not installed.", "✗".red());
            println!("  Expected location: {}", model_dir.display());
            println!();
            println!("To install, run:");
            println!("  cass models install");
        }
        return Ok(());
    }

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut all_valid = true;
    let mut files_to_repair: Vec<&str> = Vec::new();

    if !json_output {
        println!("Verifying model files...");
        println!();
    }

    for mfile in &manifest.files {
        let file_path = model_dir.join(&mfile.name);
        let exists = file_path.is_file();

        let (valid, actual_hash, error) = if exists {
            match compute_sha256(&file_path) {
                Ok(hash) => {
                    let matches = hash == mfile.sha256;
                    (matches, Some(hash), None)
                }
                Err(e) => (false, None, Some(e.to_string())),
            }
        } else {
            (false, None, Some("File not found".to_string()))
        };

        if !valid {
            all_valid = false;
            files_to_repair.push(&mfile.name);
        }

        results.push(serde_json::json!({
            "file": mfile.name,
            "exists": exists,
            "valid": valid,
            "expected_sha256": mfile.sha256,
            "actual_sha256": actual_hash,
            "error": error,
        }));

        if !json_output {
            let status = if valid {
                "✓".green().to_string()
            } else {
                "✗".red().to_string()
            };
            println!("  {} {}", status, mfile.name);
            if let Some(ref err) = error {
                println!("      Error: {}", err);
            } else if !valid {
                println!(
                    "      Expected: {}",
                    mfile.sha256.get(..16).unwrap_or(&mfile.sha256)
                );
                if let Some(ref actual) = actual_hash {
                    println!("      Got:      {}", actual.get(..16).unwrap_or(actual));
                }
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "model_dir": model_dir.display().to_string(),
                "all_valid": all_valid,
                "files": results,
            }))
            .unwrap_or_default()
        );
    } else {
        println!();
        if all_valid {
            println!("{} All model files verified successfully.", "✓".green());
        } else {
            println!(
                "{} {} file(s) failed verification.",
                "✗".red(),
                files_to_repair.len()
            );
            if repair {
                println!();
                println!("Repairing by re-downloading model files...");
                println!();
                // Actually perform the repair by re-running install
                return run_models_install("all-minilm-l6-v2", None, None, true, data_dir_override);
            } else {
                println!();
                println!("To repair corrupted files, run:");
                println!("  cass models verify --repair");
                println!("Or reinstall:");
                println!("  cass models install -y");
            }
        }
    }

    Ok(())
}

/// Remove model files
fn run_models_remove(
    model_name: &str,
    skip_confirm: bool,
    data_dir_override: Option<PathBuf>,
) -> CliResult<()> {
    use crate::search::fastembed_embedder::FastEmbedder;
    use colored::Colorize;

    // Only support the default model for now
    if model_name != "all-minilm-l6-v2" {
        return Err(CliError {
            code: 20,
            kind: "model",
            message: format!(
                "Unknown model '{}'. Only 'all-minilm-l6-v2' is supported.",
                model_name
            ),
            hint: Some("Use 'cass models status' to see available models".into()),
            retryable: false,
        });
    }

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let model_dir = FastEmbedder::default_model_dir(&data_dir);

    if !model_dir.is_dir() {
        println!("{} Model is not installed.", "✗".yellow());
        println!("  Expected location: {}", model_dir.display());
        return Ok(());
    }

    // Calculate size
    let mut total_size: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(&model_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                total_size += metadata.len();
            }
        }
    }
    let size_mb = total_size as f64 / 1_048_576.0;

    if !skip_confirm {
        println!("Remove Semantic Search Model");
        println!("============================");
        println!();
        println!("Model:    {}", model_name);
        println!("Location: {}", model_dir.display());
        println!("Size:     {:.1} MB", size_mb);
        println!();
        println!("This will remove all model files. Semantic search will be unavailable");
        println!("until the model is reinstalled.");
        print!("Continue? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if !input.trim().eq_ignore_ascii_case("y") && !input.trim().eq_ignore_ascii_case("yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Remove the directory
    std::fs::remove_dir_all(&model_dir).map_err(|e| CliError {
        code: 24,
        kind: "io",
        message: format!("Failed to remove model directory: {}", e),
        hint: Some("Check file permissions".into()),
        retryable: false,
    })?;

    println!();
    println!("{} Model removed successfully.", "✓".green());
    println!("  Freed {:.1} MB", size_mb);
    println!();
    println!("To reinstall, run:");
    println!("  cass models install");

    Ok(())
}

/// Check for model updates
fn run_models_check_update(json_output: bool, data_dir_override: Option<PathBuf>) -> CliResult<()> {
    use crate::search::fastembed_embedder::FastEmbedder;
    use crate::search::model_download::{
        ModelManifest, ModelState, check_model_installed, check_version_mismatch,
    };
    use colored::Colorize;

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let model_dir = FastEmbedder::default_model_dir(&data_dir);
    let manifest = ModelManifest::minilm_v2();

    let state = check_model_installed(&model_dir);

    if !state.is_ready() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "update_available": false,
                    "reason": "model_not_installed",
                    "current_revision": null,
                    "latest_revision": manifest.revision,
                }))
                .unwrap_or_default()
            );
        } else {
            println!("{} Model is not installed.", "✗".yellow());
            println!(
                "  Latest revision: {}",
                manifest.revision.get(..12).unwrap_or(&manifest.revision)
            );
            println!();
            println!("To install, run:");
            println!("  cass models install");
        }
        return Ok(());
    }

    // Check for version mismatch
    let update_info = check_version_mismatch(&model_dir, &manifest);

    if let Some(ModelState::UpdateAvailable {
        current_revision,
        latest_revision,
    }) = update_info
    {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "update_available": true,
                    "current_revision": current_revision,
                    "latest_revision": latest_revision,
                }))
                .unwrap_or_default()
            );
        } else {
            println!("{} Update available!", "⚠".yellow());
            println!(
                "  Current: {}",
                current_revision.get(..12).unwrap_or(&current_revision)
            );
            println!(
                "  Latest:  {}",
                latest_revision.get(..12).unwrap_or(&latest_revision)
            );
            println!();
            println!("To update, run:");
            println!("  cass models install");
        }
    } else if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "update_available": false,
                "current_revision": manifest.revision,
                "latest_revision": manifest.revision,
            }))
            .unwrap_or_default()
        );
    } else {
        println!("{} Model is up to date.", "✓".green());
        println!(
            "  Revision: {}",
            manifest.revision.get(..12).unwrap_or(&manifest.revision)
        );
    }

    Ok(())
}

/// Handle mappings subcommands (P6.3)
fn run_mappings_command(action: MappingsAction) -> CliResult<()> {
    match action {
        MappingsAction::List { source, json } => {
            run_mappings_list(&source, json)?;
        }
        MappingsAction::Add {
            source,
            from,
            to,
            agents,
        } => {
            run_mappings_add(&source, &from, &to, agents)?;
        }
        MappingsAction::Remove { source, index } => {
            run_mappings_remove(&source, index)?;
        }
        MappingsAction::Test {
            source,
            path,
            agent,
        } => {
            run_mappings_test(&source, &path, agent.as_deref())?;
        }
    }
    Ok(())
}

/// List path mappings for a source (P6.3)
fn run_mappings_list(source_name: &str, json_output: bool) -> CliResult<()> {
    use crate::sources::config::SourcesConfig;

    let config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: None,
        retryable: false,
    })?;

    let source = config.find_source(source_name).ok_or_else(|| CliError {
        code: 12,
        kind: "source",
        message: format!("Source '{}' not found", source_name),
        hint: Some("Use 'cass sources list' to see available sources".into()),
        retryable: false,
    })?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": source_name,
                "mappings": source.path_mappings,
            }))
            .unwrap_or_default()
        );
    } else {
        println!("Path mappings for source '{}':", source_name);
        println!();

        if source.path_mappings.is_empty() {
            println!("  No path mappings configured.");
            println!();
            println!("Add mappings with:");
            println!(
                "  cass sources mappings add {} --from /remote/path --to /local/path",
                source_name
            );
        } else {
            for (idx, mapping) in source.path_mappings.iter().enumerate() {
                println!("  [{}] {} → {}", idx, mapping.from, mapping.to);
                if let Some(ref agents) = mapping.agents {
                    println!("      agents: {}", agents.join(", "));
                }
            }
        }
        println!();
    }

    Ok(())
}

/// Add a path mapping to a source (P6.3)
fn run_mappings_add(
    source_name: &str,
    from: &str,
    to: &str,
    agents: Option<Vec<String>>,
) -> CliResult<()> {
    use crate::sources::config::{PathMapping, SourcesConfig};

    let mut config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: None,
        retryable: false,
    })?;

    let source = config
        .find_source_mut(source_name)
        .ok_or_else(|| CliError {
            code: 12,
            kind: "source",
            message: format!("Source '{}' not found", source_name),
            hint: Some("Use 'cass sources list' to see available sources".into()),
            retryable: false,
        })?;

    // Create the mapping
    let mapping = if let Some(agent_list) = agents {
        PathMapping::with_agents(from, to, agent_list)
    } else {
        PathMapping::new(from, to)
    };

    // Check for duplicates
    let already_exists = source
        .path_mappings
        .iter()
        .any(|m| m.from == mapping.from && m.to == mapping.to && m.agents == mapping.agents);

    if already_exists {
        return Err(CliError {
            code: 13,
            kind: "mapping",
            message: "This mapping already exists".into(),
            hint: None,
            retryable: false,
        });
    }

    source.path_mappings.push(mapping.clone());

    config.save().map_err(|e| CliError {
        code: 11,
        kind: "config",
        message: format!("Failed to save config: {e}"),
        hint: Some("Check file permissions on config directory".into()),
        retryable: false,
    })?;

    println!("Added mapping to source '{}':", source_name);
    println!("  {} → {}", mapping.from, mapping.to);
    if let Some(agents) = &mapping.agents {
        println!("  agents: {}", agents.join(", "));
    }
    println!();
    println!("Test with:");
    println!("  cass sources mappings test {} {}", source_name, from);

    Ok(())
}

/// Remove a path mapping from a source (P6.3)
fn run_mappings_remove(source_name: &str, index: usize) -> CliResult<()> {
    use crate::sources::config::SourcesConfig;

    let mut config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: None,
        retryable: false,
    })?;

    let source = config
        .find_source_mut(source_name)
        .ok_or_else(|| CliError {
            code: 12,
            kind: "source",
            message: format!("Source '{}' not found", source_name),
            hint: Some("Use 'cass sources list' to see available sources".into()),
            retryable: false,
        })?;

    if index >= source.path_mappings.len() {
        return Err(CliError {
            code: 14,
            kind: "mapping",
            message: format!(
                "Invalid index {}. Source has {} mapping(s).",
                index,
                source.path_mappings.len()
            ),
            hint: Some("Use 'cass sources mappings list' to see valid indices".into()),
            retryable: false,
        });
    }

    let removed = source.path_mappings.remove(index);

    config.save().map_err(|e| CliError {
        code: 11,
        kind: "config",
        message: format!("Failed to save config: {e}"),
        hint: Some("Check file permissions on config directory".into()),
        retryable: false,
    })?;

    println!("Removed mapping from source '{}':", source_name);
    println!("  {} → {}", removed.from, removed.to);

    Ok(())
}

/// Test how a path would be rewritten for a source (P6.3)
fn run_mappings_test(source_name: &str, path: &str, agent: Option<&str>) -> CliResult<()> {
    use crate::sources::config::{PathMapping, SourcesConfig};
    use colored::Colorize;

    let config = SourcesConfig::load().map_err(|e| CliError {
        code: 9,
        kind: "config",
        message: format!("Failed to load sources config: {e}"),
        hint: None,
        retryable: false,
    })?;

    let source = config.find_source(source_name).ok_or_else(|| CliError {
        code: 12,
        kind: "source",
        message: format!("Source '{}' not found", source_name),
        hint: Some("Use 'cass sources list' to see available sources".into()),
        retryable: false,
    })?;

    // Find the matching mapping
    let mut matching_mapping = None;
    let rewritten = source.rewrite_path_for_agent(path, agent);

    // Find which rule matched (if any)
    if rewritten != path {
        // Find the longest matching prefix
        let mut best_match: Option<&PathMapping> = None;
        for mapping in &source.path_mappings {
            if !mapping.applies_to_agent(agent) {
                continue;
            }
            if path.starts_with(&mapping.from)
                && best_match.is_none_or(|best| mapping.from.len() > best.from.len())
            {
                best_match = Some(mapping);
            }
        }
        matching_mapping = best_match;
    }

    println!();
    println!("Input:  {}", path);
    println!("Output: {}", rewritten);

    if let Some(mapping) = matching_mapping {
        println!("Rule:   {} → {}", mapping.from, mapping.to);
        if let Some(ref agents) = mapping.agents {
            println!("        agents: {}", agents.join(", "));
        }
        println!("Status: {} mapped", "✓".green());
    } else if rewritten == path {
        println!("Status: {} no matching rule", "✗".yellow());

        if !source.path_mappings.is_empty() {
            println!();
            println!("Available rules:");
            for mapping in &source.path_mappings {
                println!("  {} → {}", mapping.from, mapping.to);
                if let Some(ref agents) = mapping.agents {
                    println!("    agents: {}", agents.join(", "));
                }
            }
        }
    }

    if let Some(a) = agent {
        println!();
        println!("(Tested with agent: {})", a);
    }
    println!();

    Ok(())
}

fn parse_datetime_flexible(s: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};

    fn local_from_naive(dt: NaiveDateTime) -> i64 {
        match Local.from_local_datetime(&dt) {
            chrono::LocalResult::Single(local) => local.timestamp_millis(),
            chrono::LocalResult::Ambiguous(local, _) => local.timestamp_millis(),
            chrono::LocalResult::None => Local.from_utc_datetime(&dt).timestamp_millis(),
        }
    }

    fn local_midnight_ts(date: NaiveDate) -> Option<i64> {
        let dt = date.and_hms_opt(0, 0, 0)?;
        Some(local_from_naive(dt))
    }

    // Returns timestamp in milliseconds to match SQLite storage format
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }

    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        && let Some(ts) = local_midnight_ts(date)
    {
        return Some(ts);
    }

    let now = Local::now();
    match s.to_lowercase().as_str() {
        "today" => {
            let date = now.date_naive();
            local_midnight_ts(date)
        }
        "yesterday" => {
            let yesterday = (now - chrono::Duration::days(1)).date_naive();
            local_midnight_ts(yesterday)
        }
        _ => {
            if let Some(days_str) = s.strip_suffix('d')
                && let Ok(days) = days_str.parse::<i64>()
            {
                return Some((now - chrono::Duration::days(days)).timestamp_millis());
            }
            if let Some(hours_str) = s.strip_suffix('h')
                && let Ok(hours) = hours_str.parse::<i64>()
            {
                return Some((now - chrono::Duration::hours(hours)).timestamp_millis());
            }
            None
        }
    }
}
