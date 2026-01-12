pub mod bookmarks;
pub mod connectors;
pub mod encryption;
pub mod export;
pub mod indexer;
pub mod model;
pub mod pages;
pub mod search;
pub mod sources;
pub mod storage;
pub mod ui;
pub mod update_check;

use anyhow::Result;
use base64::{Engine, prelude::*};
use chrono::Utc;
use clap::{Arg, ArgAction, Command, CommandFactory, Parser, Subcommand, ValueEnum, ValueHint};
use indexer::IndexOptions;
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
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
    #[arg(long)]
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

        /// Override data dir (matches index --data-dir)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Run indexer
    Index {
        /// Perform full rebuild
        #[arg(long)]
        full: bool,

        /// Force Tantivy index rebuild even if schema matches
        #[arg(long, default_value_t = false)]
        force_rebuild: bool,

        /// Watch for changes and reindex automatically
        #[arg(long)]
        watch: bool,

        /// Trigger a single watch cycle for specific paths (comma-separated or repeated)
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        watch_once: Option<Vec<PathBuf>>,

        /// Override data dir (index + db). Defaults to platform data dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Output as JSON (for automation)
        #[arg(long)]
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
        /// Robot output format: json (pretty), jsonl (streaming), compact (single-line)
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
    },
    /// Show statistics about indexed data
    Stats {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
        json: bool,
    },
    /// Quick state/health check (alias of status)
    State {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON (default for robot consumption)
        #[arg(long)]
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
        #[arg(long)]
        json: bool,
    },
    /// Full API schema introspection - commands, arguments, and response schemas
    Introspect {
        /// Output as JSON (default for robot consumption)
        #[arg(long)]
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
        #[arg(long)]
        json: bool,
    },
    /// Minimal health check (<50ms). Exit 0=healthy, 1=unhealthy. For agent pre-flight checks.
    Health {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON (`{"healthy": bool, "latency_ms": N}`)
        #[arg(long)]
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
        #[arg(long)]
        json: bool,
        /// Apply safe fixes automatically (rebuilds index/db from source data)
        #[arg(long)]
        fix: bool,
        /// Run all checks verbosely (show passed checks too)
        #[arg(long, short)]
        verbose: bool,
        /// Force index rebuild even if index appears healthy
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
        json: bool,
        /// Group by: hour, day, or none
        #[arg(long, value_enum, default_value_t = TimelineGrouping::Hour)]
        group_by: TimelineGrouping,
        /// Filter by source: 'local', 'remote', 'all', or a specific source hostname
        #[arg(long)]
        source: Option<String>,
    },
    /// Export encrypted searchable archive for GitHub Pages (P4.1)
    Pages {
        /// Export only (skip wizard and encryption) to specified directory
        #[arg(long)]
        export_only: Option<PathBuf>,

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

        /// Output secret scan results as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage remote sources (P5.x)
    #[command(subcommand)]
    Sources(SourcesCommand),
    /// Manage semantic search models
    #[command(subcommand)]
    Models(ModelsCommand),
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
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
        json: bool,
    },
}

/// Subcommands for managing semantic search models
#[derive(Subcommand, Debug, Clone)]
pub enum ModelsCommand {
    /// Show model installation status
    Status {
        /// Output as JSON
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
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
        #[arg(long)]
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
            let known_flags = ["--limit", "--offset", "--agent", "--workspace", "--days"];
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

pub async fn run() -> CliResult<()> {
    let raw_args: Vec<String> = std::env::args().collect();
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
                        return Err(CliError::usage("Could not parse arguments", Some(friendly)));
                    }
                }
            } else {
                // No recovery possible
                let friendly = format_friendly_parse_error(err, &raw_args, &normalized_args);
                return Err(CliError::usage("Could not parse arguments", Some(friendly)));
            }
        }
    };

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
        data_dir: None,
    });

    if cli.robot_help {
        print_robot_help(wrap)?;
        return Ok(());
    }

    if let Commands::RobotDocs { topic } = command.clone() {
        print_robot_docs(topic, wrap)?;
        return Ok(());
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
                once: false,
                reset_state,
                data_dir,
                ..
            } = command.clone()
            {
                let bg_data_dir = log_dir.clone();
                let bg_db = cli.db.clone();
                // Create shared progress tracker
                let progress = std::sync::Arc::new(indexer::IndexingProgress::default());
                spawn_background_indexer(bg_data_dir, bg_db, Some(progress.clone()));

                ui::tui::run_tui(data_dir, false, reset_state, Some(progress), None).map_err(
                    |e| CliError {
                        code: 9,
                        kind: "tui",
                        message: format!("tui failed: {e}"),
                        hint: None,
                        retryable: false,
                    },
                )?;
            } else if let Commands::Tui {
                once,
                reset_state,
                data_dir,
                ..
            } = command.clone()
            {
                ui::tui::run_tui(data_dir, once, reset_state, None, None).map_err(|e| {
                    CliError {
                        code: 9,
                        kind: "tui",
                        message: format!("tui failed: {e}"),
                        hint: None,
                        retryable: false,
                    }
                })?;
            }
        }
        Commands::Index { .. }
        | Commands::Search { .. }
        | Commands::Stats { .. }
        | Commands::Diag { .. }
        | Commands::Status { .. }
        | Commands::View { .. } => {
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
                } => {
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
                    agents,
                    workspaces,
                    since,
                    until,
                    path_mode,
                    dry_run,
                    scan_secrets,
                    fail_on_secrets,
                    secrets_allow,
                    secrets_deny,
                    json,
                } => {
                    if scan_secrets {
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
                        crate::pages::wizard::PagesWizard::new()
                            .run()
                            .map_err(|e| CliError {
                                code: 9,
                                kind: "pages",
                                message: format!("Wizard failed: {e}"),
                                hint: None,
                                retryable: false,
                            })?;
                    }
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
                    run_models_command(subcmd)?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Compute lightweight state snapshot (index/db freshness) for robot meta and state command reuse
fn state_meta_json(data_dir: &Path, db_path: &Path, stale_threshold: u64) -> serde_json::Value {
    use rusqlite::Connection;
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

    if db_exists && let Ok(conn) = Connection::open(db_path) {
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
        Some(Commands::Expand { .. }) => "expand".to_string(),
        Some(Commands::Timeline { .. }) => "timeline".to_string(),
        Some(Commands::Sources(..)) => "sources".to_string(),
        Some(Commands::Models(..)) => "models".to_string(),
        Some(Commands::Pages { .. }) => "pages".to_string(),
        None => "(default)".to_string(),
    }
}

/// Returns true if the command is using robot/JSON output mode.
/// Used to auto-suppress INFO logs for clean machine-parseable output.
fn is_robot_mode(command: &Commands) -> bool {
    match command {
        Commands::Search {
            json,
            robot_format,
            robot_meta,
            ..
        } => *json || robot_format.is_some() || *robot_meta,
        Commands::Index { json, .. } => *json,
        Commands::Stats { json, .. } => *json,
        Commands::Diag { json, .. } => *json,
        Commands::Status { json, .. } => *json,
        Commands::Health { json, .. } => *json,
        Commands::Doctor { json, .. } => *json,
        Commands::ApiVersion { json, .. } => *json,
        Commands::State { json, .. } => *json,
        Commands::View { json, .. } => *json,
        Commands::Capabilities { json, .. } => *json,
        Commands::Introspect { json, .. } => *json,
        Commands::Context { json, .. } => *json,
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
            "    --agent A         Filter by agent (codex, claude_code, gemini, opencode, amp, cline)".to_string(),
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
            "  cass tui [--once] [--data-dir DIR] [--reset-state]".to_string(),
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
            "  NO_COLOR / CASS_NO_COLOR                 disable color".to_string(),
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
            "  Output: --robot/--json; JSONL via --robot-format jsonl; compact via --robot-format compact".to_string(),
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
    };

    println!("{}", render_block(&lines, wrap));
    Ok(())
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
) -> CliResult<()> {
    use crate::search::query::{QueryExplanation, SearchClient, SearchFilters, SearchMode};
    use crate::search::tantivy::index_dir;
    use crate::sources::provenance::SourceFilter;
    use std::collections::HashSet;

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

    let client = SearchClient::open(&index_path, Some(&db_path))
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
    // Priority: robot_format > json flag > display format > default plain
    let effective_robot = robot_format
        .or(if *json { Some(RobotFormat::Json) } else { None })
        .or({
            if robot_auto {
                Some(RobotFormat::Json)
            } else {
                None
            }
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
                timeout_ms.unwrap()
            ),
            hint: Some("Increase --timeout value or simplify query".to_string()),
            retryable: true,
        });
    }

    // Determine effective search mode (default to Lexical)
    let effective_mode = mode.unwrap_or(SearchMode::Lexical);

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
            let hits = client
                .search_semantic(query, filters.clone(), search_limit, search_offset, field_mask)
                .map_err(|e| {
                    let err_str = e.to_string();
                    if err_str.contains("unavailable") || err_str.contains("no embedder") {
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
    let filtered_hits: Vec<serde_json::Value> = result
        .hits
        .iter()
        .map(|hit| filter_hit_fields(hit, &resolved_fields))
        .map(|hit| apply_content_truncation(hit, truncation_budgets))
        .collect();

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
    use rusqlite::Connection;

    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));

    if !db_path.exists() {
        return Err(CliError {
            code: 3,
            kind: "missing-db",
            message: format!(
                "Database not found at {}. Run 'cass index --full' first.",
                db_path.display()
            ),
            hint: None,
            retryable: true,
        });
    }

    let conn = Connection::open(&db_path).map_err(|e| CliError {
        code: 9,
        kind: "db-open",
        message: format!("Failed to open database: {e}"),
        hint: None,
        retryable: false,
    })?;

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

    // Get date range with source filter
    let date_sql = format!(
        "SELECT MIN(started_at), MAX(started_at) FROM conversations c{source_where} WHERE started_at IS NOT NULL"
    );
    let (oldest, newest): (Option<i64>, Option<i64>) = if let Some(ref param) = source_param {
        conn.query_row(&date_sql, [param], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((None, None))
    } else {
        conn.query_row(&date_sql, [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((None, None))
    };

    // Get per-source breakdown if requested (P3.7)
    let source_rows: Vec<(String, i64, i64)> = if by_source {
        let mut stmt = conn
            .prepare(
                "SELECT c.source_id, COUNT(DISTINCT c.id) as convs, COUNT(m.id) as msgs
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             GROUP BY c.source_id
             ORDER BY convs DESC",
            )
            .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
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
    } else {
        Vec::new()
    };

    if json {
        let mut payload = serde_json::json!({
            "conversations": conversation_count,
            "messages": message_count,
            "by_agent": agent_rows.iter().map(|(a, c)| serde_json::json!({"agent": a, "count": c})).collect::<Vec<_>>(),
            "top_workspaces": ws_rows.iter().map(|(w, c)| serde_json::json!({"workspace": w, "count": c})).collect::<Vec<_>>(),
            "date_range": {
                "oldest": oldest.map(|ts| chrono::DateTime::from_timestamp_millis(ts).map(|d| d.to_rfc3339())),
                "newest": newest.map(|ts| chrono::DateTime::from_timestamp_millis(ts).map(|d| d.to_rfc3339())),
            },
            "db_path": db_path.display().to_string(),
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

        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        // Header with source filter indicator
        let title = if let Some(ref filter) = source_filter {
            format!("CASS Index Statistics (source: {})", filter)
        } else {
            "CASS Index Statistics".to_string()
        };
        println!("{title}");
        println!("{}", "=".repeat(title.len()));
        println!("Database: {}", db_path.display());
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

    let codex_path = home.join(".codex/sessions");
    let claude_path = home.join(".claude/projects");
    let cline_path = config_dir.join("Code/User/globalStorage/saoudrizwan.claude-dev");
    let gemini_path = home.join(".gemini/tmp");
    let opencode_path = home.join(".opencode");
    let amp_path = config_dir.join("Code/User/globalStorage/sourcegraph.amp");
    let cursor_path = crate::connectors::cursor::CursorConnector::app_support_dir()
        .unwrap_or_else(|| home.join("Library/Application Support/Cursor/User"));
    let chatgpt_path = crate::connectors::chatgpt::ChatGptConnector::app_support_dir()
        .unwrap_or_else(|| home.join("Library/Application Support/com.openai.chat"));

    let agent_paths: Vec<(&str, &std::path::Path, bool)> = vec![
        ("codex", &codex_path, codex_path.exists()),
        ("claude", &claude_path, claude_path.exists()),
        ("cline", &cline_path, cline_path.exists()),
        ("gemini", &gemini_path, gemini_path.exists()),
        ("opencode", &opencode_path, opencode_path.exists()),
        ("amp", &amp_path, amp_path.exists()),
        ("cursor", &cursor_path, cursor_path.exists()),
        ("chatgpt", &chatgpt_path, chatgpt_path.exists()),
    ];

    let platform = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
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

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
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
    let state = state_meta_json(&data_dir, &db_path, stale_threshold);

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

    if json {
        let payload = serde_json::json!({
            "healthy": healthy,
            "latency_ms": latency_ms,
            "state": state
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
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
    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
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
    use rusqlite::Connection;

    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));

    if !db_path.exists() {
        return Err(CliError {
            code: 3,
            kind: "missing_index",
            message: "Database not found".to_string(),
            hint: Some("Run 'cass index --full' to create the database.".to_string()),
            retryable: true,
        });
    }

    let conn = Connection::open(&db_path).map_err(|e| CliError {
        code: 9,
        kind: "db-open",
        message: format!("Failed to open database: {e}"),
        hint: None,
        retryable: false,
    })?;

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

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
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
        connectors: vec![
            "codex".to_string(),
            "claude_code".to_string(),
            "gemini".to_string(),
            "opencode".to_string(),
            "amp".to_string(),
            "cline".to_string(),
            "aider".to_string(),
            "cursor".to_string(),
            "chatgpt".to_string(),
            "pi_agent".to_string(),
        ],
        limits: CapabilitiesLimits {
            max_limit: 10000,
            max_content_length: 0, // 0 = unlimited
            max_fields: 50,
            max_agg_buckets: 10,
        },
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).unwrap_or_default()
        );
    } else {
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
    }

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

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).unwrap_or_default()
        );
    } else {
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

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!("CASS API Version");
        println!("================");
        println!("crate: {}", env!("CARGO_PKG_VERSION"));
        println!("api:   v{}", 1);
        println!("contract: v{CONTRACT_VERSION}");
    }

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

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
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
    }

    Ok(())
}

use crossbeam_channel::Sender;
use indexer::IndexerEvent;

fn spawn_background_indexer(
    data_dir: PathBuf,
    db: Option<PathBuf>,
    progress: Option<std::sync::Arc<indexer::IndexingProgress>>,
) -> Option<Sender<IndexerEvent>> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let tx_clone = tx.clone();
    let progress_for_error = progress.clone();
    std::thread::spawn(move || {
        let db_path = db.unwrap_or_else(|| data_dir.join("agent_search.db"));
        let opts = IndexOptions {
            full: false,
            force_rebuild: false,
            watch: true,
            watch_once_paths: read_watch_once_paths_env(),
            db_path,
            data_dir,
            progress,
        };
        // Pass the receiver to run_index so it can listen for commands
        if let Err(e) = indexer::run_index(opts, Some((tx_clone, rx))) {
            warn!("Background indexer failed: {}", e);
            if let Some(p) = progress_for_error {
                if let Ok(mut last_error) = p.last_error.lock() {
                    *last_error = Some(e.to_string());
                }
                p.phase.store(0, std::sync::atomic::Ordering::Relaxed);
                p.is_rebuilding
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    Some(tx)
}

#[allow(clippy::too_many_arguments)]
fn run_index_with_data(
    db_override: Option<PathBuf>,
    full: bool,
    force_rebuild: bool,
    watch: bool,
    watch_once: Option<Vec<PathBuf>>,
    data_dir_override: Option<PathBuf>,
    progress: ProgressResolved,
    json: bool,
    idempotency_key: Option<String>,
) -> CliResult<()> {
    use rusqlite::Connection;
    use std::time::Instant;

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));

    // Generate params hash for idempotency validation
    let params_hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        full.hash(&mut hasher);
        force_rebuild.hash(&mut hasher);
        watch.hash(&mut hasher);
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
                if json {
                    // Parse and augment with cached flag
                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&result_json) {
                        val["cached"] = serde_json::json!(true);
                        val["idempotency_key"] = serde_json::json!(key);
                        println!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
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
        progress: Some(index_progress.clone()),
    };

    // Set up progress display
    let show_progress = !json && matches!(progress, ProgressResolved::Bars);
    let show_plain = !json && matches!(progress, ProgressResolved::Plain);

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
        if json {
            let payload = serde_json::json!({
                "success": false,
                "error": err.message,
                "elapsed_ms": elapsed_ms,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        } else {
            eprintln!("index debug error: {err:?}");
        }
    } else if json {
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

        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
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
                    if session_start.is_none() || ts < session_start.unwrap() {
                        session_start = Some(ts);
                    }
                    if _session_end.is_none() || ts > _session_end.unwrap() {
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
                    session_title = Some(
                        content
                            .lines()
                            .next()
                            .unwrap_or("Untitled Session")
                            .chars()
                            .take(80)
                            .collect(),
                    );
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
        body {{ font-family: system-ui, sans-serif; max-width: 800px; margin: 0 auto; padding: 20px; background: #f5f5f5; }}
        .message {{ background: white; border-radius: 8px; padding: 16px; margin: 12px 0; box-shadow: 0 1px 3px rgba(0,0,0,0.1); }}
        .user {{ border-left: 4px solid #2563eb; }}
        .assistant {{ border-left: 4px solid #16a34a; }}
        .role {{ font-weight: bold; color: #374151; margin-bottom: 8px; }}
        .content {{ white-space: pre-wrap; line-height: 1.6; }}
        .tool {{ background: #f3f4f6; padding: 8px; border-radius: 4px; font-family: monospace; font-size: 0.9em; margin: 8px 0; }}
        h1 {{ color: #1f2937; }}
        .meta {{ color: #6b7280; font-size: 0.9em; }}
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
        html.push_str(&html_escape(&content));

        // Also handle tool use blocks if requested
        if include_tools {
            // Check for tool_use in nested message.content array
            let content_val = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| msg.get("content"));
            if let Some(arr) = content_val.and_then(|c| c.as_array()) {
                for block in arr {
                    if let Some("tool_use") = block.get("type").and_then(|t| t.as_str()) {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        html.push_str(&format!(
                            r#"<div class="tool">🔧 {}</div>"#,
                            html_escape(name)
                        ));
                    }
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

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
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
    }
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
    use rusqlite::Connection;
    use std::collections::HashMap;

    // Parse source filter (P3.2)
    let source_filter = source.as_ref().map(|s| SourceFilter::parse(s));

    let data_root = data_dir.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_root.join("agent_search.db"));

    if !db_path.exists() {
        return Err(CliError {
            code: 3,
            kind: "db-not-found",
            message: "No database found. Run 'cass index' first.".to_string(),
            hint: Some(format!("Expected: {}", db_path.display())),
            retryable: true,
        });
    }

    let conn = Connection::open(&db_path).map_err(|e| CliError {
        code: 9,
        kind: "db-open",
        message: format!("Failed to open database: {e}"),
        hint: None,
        retryable: true,
    })?;

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

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
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
    }
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

    if json {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_default()
        );
    } else {
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
    }

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

    let result = downloader.download(
        &manifest,
        Some(Box::new(move |progress| {
            pb_clone.set_position(progress.total_bytes);
        })),
    );

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
                && (best_match.is_none()
                    || mapping.from.len() > best_match.as_ref().unwrap().from.len())
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
