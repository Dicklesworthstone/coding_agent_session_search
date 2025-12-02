pub mod bookmarks;
pub mod connectors;
pub mod export;
pub mod indexer;
pub mod model;
pub mod search;
pub mod storage;
pub mod ui;
pub mod update_check;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
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
    std::env::var("CASS_TEST_WATCH_PATHS")
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
    /// Path to the SQLite database (defaults to platform data dir)
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

        /// Delete persisted UI state (tui_state.json) before launch
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
        /// Include extended metadata in robot output (elapsed_ms, wildcard_fallback, cache_stats)
        #[arg(long)]
        robot_meta: bool,
        /// Select specific fields in JSON output (comma-separated). Use 'minimal' for source_path,line_number,agent
        /// or 'summary' for source_path,line_number,agent,title,score. Example: --fields source_path,line_number
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
        /// Server-side aggregation by field(s). Comma-separated: agent,workspace,date,match_type
        /// Returns buckets with counts instead of full results. Use with --limit to get both.
        #[arg(long, value_delimiter = ',')]
        aggregate: Option<Vec<String>>,
    },
    /// Show statistics about indexed data
    Stats {
        /// Override data dir
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
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
    ExitCodes,
    Examples,
    Contracts,
    Wrap,
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

pub async fn run() -> CliResult<()> {
    let cli = Cli::parse();
    let stdout_is_tty = io::stdout().is_terminal();
    let stderr_is_tty = io::stderr().is_terminal();
    configure_color(cli.color, stdout_is_tty, stderr_is_tty);

    let wrap_cfg = WrapConfig::new(cli.wrap, cli.nowrap);
    let progress_resolved = resolve_progress(cli.progress, stdout_is_tty);

    let start_ts = Utc::now();
    let start_instant = Instant::now();
    let command_label = describe_command(&cli);

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
        let exit_code = result.as_ref().map(|_| 0).unwrap_or_else(|e| e.code);
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
        && std::env::var("TUI_HEADLESS").is_err()
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
                        TimeFilter::new(
                            days,
                            today,
                            yesterday,
                            week,
                            since.as_deref(),
                            until.as_deref(),
                        ),
                        aggregate,
                    )?;
                }
                Commands::Stats { data_dir, json } => {
                    run_stats(&data_dir, cli.db.clone(), json)?;
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
                    stale_threshold,
                } => {
                    run_status(&data_dir, cli.db.clone(), json, stale_threshold)?;
                }
                Commands::State {
                    data_dir,
                    json,
                    stale_threshold,
                } => {
                    run_status(&data_dir, cli.db.clone(), json, stale_threshold)?;
                }
                Commands::View {
                    path,
                    line,
                    context,
                    json,
                } => {
                    run_view(&path, line, context, json)?;
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
                    stale_threshold,
                } => {
                    run_status(&data_dir, None, json, stale_threshold)?;
                }
                Commands::Introspect { json } => {
                    run_introspect(json)?;
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

    let index_path = data_dir.join("tantivy_index");
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
            .and_then(|v| v.get("pending_count").and_then(|c| c.as_u64()))
            .unwrap_or(0)
    } else {
        0
    };

    let index_age_secs = last_indexed_at.map(|ts| {
        let ts_secs = ts / 1000;
        now_secs.saturating_sub(ts_secs as u64)
    });
    let is_stale = index_age_secs
        .map(|age| age > stale_threshold)
        .unwrap_or(true);
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
        "pending_sessions": pending.and_then(|p| p.get("sessions")),
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
        Commands::ApiVersion { json, .. } => *json,
        Commands::State { json, .. } => *json,
        Commands::View { json, .. } => *json,
        Commands::Capabilities { json, .. } => *json,
        Commands::Introspect { json, .. } => *json,
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
            "  cass search <query> [OPTIONS]".to_string(),
            "    --agent A         Filter by agent (codex, claude_code, gemini, opencode, amp, cline)".to_string(),
            "    --workspace W     Filter by workspace path".to_string(),
            "    --limit N         Max results (default: 10)".to_string(),
            "    --offset N        Pagination offset (default: 0)".to_string(),
            "    --json | --robot  JSON output for automation".to_string(),
            "    --fields F1,F2    Select specific fields in hits (reduces token usage)".to_string(),
            "                      Presets: minimal (path,line,agent), summary (+title,score)".to_string(),
            "                      Fields: score,agent,workspace,source_path,snippet,content,title,created_at,line_number,match_type".to_string(),
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
        RobotTopic::Schemas => render_schema_docs(),
        RobotTopic::ExitCodes => vec![
            "exit-codes:".to_string(),
            " 0 ok | 2 usage | 3 missing index/db | 4 network | 5 data-corrupt | 6 incompatible-version | 7 lock/busy | 8 partial | 9 unknown".to_string(),
        ],
        RobotTopic::Examples => vec![
            "examples:".to_string(),
            "".to_string(),
            "# Basic search with JSON output for agents".to_string(),
            "  cass search \"your query\" --robot".to_string(),
            "# Token-budgeted search with cursor + request-id".to_string(),
            "  cass search \"error\" --robot --max-tokens 200 --request-id run-1 --limit 2 --robot-meta".to_string(),
            "  cass search \"error\" --robot --cursor <_meta.next_cursor> --request-id run-1b --robot-meta".to_string(),
            "".to_string(),
            "# Search with time filters".to_string(),
            "  cass search \"bug\" --today                 # today only".to_string(),
            "  cass search \"api\" --week                  # last 7 days".to_string(),
            "  cass search \"feature\" --days 30           # last 30 days".to_string(),
            "  cass search \"fix\" --since 2025-01-01      # since date".to_string(),
            "  cass search \"error\" --robot --limit 5 --offset 5  # paginate robot output".to_string(),
            "".to_string(),
            "# Filter by agent or workspace".to_string(),
            "  cass search \"error\" --agent codex         # codex sessions only".to_string(),
            "  cass search \"test\" --workspace /myproject # specific project".to_string(),
            "".to_string(),
            "# Follow up on search results".to_string(),
            "  cass view /path/to/session.jsonl -n 42   # view line 42 with context".to_string(),
            "  cass view /path/to/session.jsonl -n 42 -C 10  # 10 lines context".to_string(),
            "".to_string(),
            "# Get index statistics".to_string(),
            "  cass stats --json                        # JSON stats".to_string(),
            "  cass stats                               # Human-readable stats".to_string(),
            "".to_string(),
            "# Aggregation (overview queries - 99% token reduction)".to_string(),
            "  cass search \"error\" --json --aggregate agent    # count by agent".to_string(),
            "  cass search \"*\" --json --aggregate agent,workspace  # multi-field agg".to_string(),
            "  cass search \"bug\" --json --aggregate date --week  # time distribution".to_string(),
            "".to_string(),
            "# Quick health check (ideal for agents)".to_string(),
            "  cass status --json                       # health check JSON".to_string(),
            "  cass status --stale-threshold 3600       # custom stale threshold (1hr)".to_string(),
            "".to_string(),
            "# Diagnostics".to_string(),
            "  cass diag --json                         # JSON diagnostic info".to_string(),
            "  cass diag --verbose                      # Human-readable with sizes".to_string(),
            "".to_string(),
            "# Capabilities introspection (for agent self-configuration)".to_string(),
            "  cass capabilities --json                 # JSON with version, features, limits".to_string(),
            "  cass capabilities                        # Human-readable summary".to_string(),
            "".to_string(),
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
            .map(str::to_string)
            .unwrap_or_else(|| "?".to_string())
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

fn write_trace_line(
    path: &PathBuf,
    label: &str,
    _cli: &Cli,
    start_ts: &chrono::DateTime<Utc>,
    duration_ms: u128,
    exit_code: i32,
    error: Option<&CliError>,
) -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
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
        "contract_version": CONTRACT_VERSION,
        "crate_version": env!("CARGO_PKG_VERSION"),
    });

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", payload)?;
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
            .unwrap();

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
            let days_ago = now - Duration::days(d as i64);
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

    // Try full datetime first: YYYY-MM-DDTHH:MM:SS
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Local
            .from_local_datetime(&dt)
            .single()
            .map(|d| d.timestamp_millis());
    }

    // Try date only: YYYY-MM-DD
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Local
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .map(|d| d.timestamp_millis());
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
    time_filter: TimeFilter,
    aggregate: Option<Vec<String>>,
) -> CliResult<()> {
    use crate::search::query::{SearchClient, SearchFilters};
    use crate::search::tantivy::index_dir;
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

    // Apply cursor overrides (base64-encoded JSON { "offset": usize, "limit": usize })
    let mut limit_val = *limit;
    let mut offset_val = *offset;
    if let Some(ref cursor_str) = cursor {
        let decoded = BASE64.decode(cursor_str).map_err(|e| CliError {
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
        if let Some(o) = cursor_json.get("offset").and_then(|v| v.as_u64()) {
            offset_val = o as usize;
        }
        if let Some(l) = cursor_json.get("limit").and_then(|v| v.as_u64()) {
            limit_val = l as usize;
        }
    }

    // Determine the effective output format
    // Priority: robot_format > json flag > display format > default plain
    let effective_robot = robot_format.or(if *json { Some(RobotFormat::Json) } else { None });

    // Parse aggregate fields if provided
    let agg_fields = aggregate
        .as_ref()
        .map(|f| parse_aggregate_fields(f))
        .unwrap_or_default();
    let has_aggregation = !agg_fields.is_empty();

    // Use search_with_fallback to get full metadata (wildcard_fallback, cache_stats)
    let sparse_threshold = 3; // Threshold for triggering wildcard fallback

    // When aggregating, we need more results for accurate counts
    // Fetch up to 1000 for aggregation starting at offset 0, then apply offset/limit
    let (search_limit, search_offset) = if has_aggregation {
        (1000.max(limit_val + offset_val), 0)
    } else {
        (limit_val, offset_val)
    };

    let result = client
        .search_with_fallback(
            query,
            filters.clone(),
            search_limit,
            search_offset,
            sparse_threshold,
        )
        .map_err(|e| CliError {
            code: 9,
            kind: "search",
            message: format!("search failed: {e}"),
            hint: None,
            retryable: true,
        })?;

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
        Some(BASE64.encode(payload))
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
                .and_then(|v: &serde_json::Value| v.as_u64())
                .map(|s| format!("{s} seconds"))
                .unwrap_or_else(|| "an unknown age".to_string());
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
        )?;
    } else if display_result.hits.is_empty() {
        eprintln!("No results found.");
    } else if let Some(display) = display_format {
        // Human-readable display formats
        output_display_results(&display_result.hits, display, wrap)?;
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
) -> CliResult<()> {
    match format {
        DisplayFormat::Table => {
            // Aligned columns with headers
            println!("{:<6} {:<12} {:<25} SNIPPET", "SCORE", "AGENT", "WORKSPACE");
            println!("{}", "-".repeat(80));
            for hit in hits {
                let workspace = truncate_start(&hit.workspace, 24);
                let snippet = hit.snippet.replace('\n', " ");
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
                    let dt = chrono::DateTime::from_timestamp_millis(ts)
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    println!("- **Created**: {}", dt);
                }
                let snippet = apply_wrap(&hit.snippet, wrap);
                println!("\n```\n{}\n```\n", snippet);
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
                "*" | "all" => vec![], // Empty means include all - handled specially
                other => vec![other.to_string()],
            })
            .collect()
    })
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

/// Truncate a string to max_len characters, UTF-8 safe, with ellipsis
fn truncate_content(s: &str, max_len: usize) -> (String, bool) {
    let char_count = s.chars().count();
    if char_count <= max_len {
        (s.to_string(), false)
    } else {
        // Leave room for "..." (3 chars)
        let truncate_at = max_len.saturating_sub(3);
        let truncated: String = s.chars().take(truncate_at).collect();
        (format!("{}...", truncated), true)
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
                obj.insert(
                    format!("{}_truncated", field),
                    serde_json::Value::Bool(true),
                );
            }
        }
    }

    serde_json::Value::Object(obj)
}

/// Clamp hits to an approximate token budget (4 chars ≈ 1 token). Returns (hits, est_tokens, clamped?)
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
    for hit in hits.into_iter() {
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
    let clamped = kept.len() < input_len || est.map(|e| e > tokens).unwrap_or(false);
    (kept, est, clamped)
}

/// Output search results in robot-friendly format
#[allow(clippy::too_many_arguments)]
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
) -> CliResult<()> {
    // Expand presets (minimal, summary, all, *)
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
    let agg_json = if !aggregations.is_empty() {
        Some(serde_json::to_value(aggregations).unwrap_or_default())
    } else {
        None
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

            // Add extended metadata if requested
            if include_meta && let serde_json::Value::Object(ref mut map) = payload {
                let mut meta = serde_json::json!({
                    "elapsed_ms": elapsed_ms,
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
                map.insert("_meta".to_string(), meta);

                if let Some(warn) = &warning {
                    map.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
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
            println!("{}", out);
        }
        RobotFormat::Jsonl => {
            // JSONL: one object per line, optional _meta header
            if include_meta || agg_json.is_some() || !result.suggestions.is_empty() {
                let mut meta = serde_json::json!({
                    "_meta": {
                        "query": query,
                        "limit": limit,
                        "offset": offset,
                        "count": filtered_hits.len(),
                        "total_matches": total_matches,
                        "elapsed_ms": elapsed_ms,
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
                if let Some(warn) = &warning
                    && let Some(m) = meta.get_mut("_meta").and_then(|v| v.as_object_mut())
                {
                    m.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
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

            if include_meta && let serde_json::Value::Object(ref mut map) = payload {
                let mut meta = serde_json::json!({
                    "elapsed_ms": elapsed_ms,
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
                map.insert("_meta".to_string(), meta);
                if let Some(warn) = &warning {
                    map.insert(
                        "_warning".to_string(),
                        serde_json::Value::String(warn.clone()),
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
            println!("{}", out);
        }
    }

    Ok(())
}

fn run_stats(
    data_dir_override: &Option<PathBuf>,
    db_override: Option<PathBuf>,
    json: bool,
) -> CliResult<()> {
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

    // Get counts and statistics
    let conversation_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap_or(0);
    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);

    // Get per-agent breakdown (need to JOIN with agents table)
    let mut agent_stmt = conn
        .prepare(
            "SELECT a.slug, COUNT(*) FROM conversations c JOIN agents a ON c.agent_id = a.id GROUP BY a.slug ORDER BY COUNT(*) DESC"
        )
        .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
    let agent_rows: Vec<(String, i64)> = agent_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| CliError::unknown(format!("query: {e}")))?
        .filter_map(|r| r.ok())
        .collect();

    // Get workspace breakdown (top 10, need to JOIN with workspaces table)
    let mut ws_stmt = conn
        .prepare(
            "SELECT w.path, COUNT(*) FROM conversations c JOIN workspaces w ON c.workspace_id = w.id GROUP BY w.path ORDER BY COUNT(*) DESC LIMIT 10"
        )
        .map_err(|e| CliError::unknown(format!("query prep: {e}")))?;
    let ws_rows: Vec<(String, i64)> = ws_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| CliError::unknown(format!("query: {e}")))?
        .filter_map(|r| r.ok())
        .collect();

    // Get date range
    let oldest: Option<i64> = conn
        .query_row(
            "SELECT MIN(started_at) FROM conversations WHERE started_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .ok();
    let newest: Option<i64> = conn
        .query_row(
            "SELECT MAX(started_at) FROM conversations WHERE started_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .ok();

    if json {
        let payload = serde_json::json!({
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!("CASS Index Statistics");
        println!("=====================");
        println!("Database: {}", db_path.display());
        println!();
        println!("Totals:");
        println!("  Conversations: {}", conversation_count);
        println!("  Messages: {}", message_count);
        println!();
        println!("By Agent:");
        for (agent, count) in &agent_rows {
            println!("  {}: {}", agent, count);
        }
        println!();
        if !ws_rows.is_empty() {
            println!("Top Workspaces:");
            for (ws, count) in &ws_rows {
                println!("  {}: {}", ws, count);
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
    let index_path = data_dir.join("tantivy_index");

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

    let agent_paths: Vec<(&str, &std::path::Path, bool)> = vec![
        ("codex", &codex_path, codex_path.exists()),
        ("claude", &claude_path, claude_path.exists()),
        ("cline", &cline_path, cline_path.exists()),
        ("gemini", &gemini_path, gemini_path.exists()),
        ("opencode", &opencode_path, opencode_path.exists()),
        ("amp", &amp_path, amp_path.exists()),
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
        println!("Version: {}", version);
        println!("Platform: {} ({})", platform, arch);
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
            println!("  Conversations: {}", conversation_count);
            println!("  Messages: {}", message_count);
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
                .filter_map(|e| e.ok())
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
        format!("{} bytes", bytes)
    }
}

/// Truncate a string from the start, keeping the last `max_chars` characters.
/// UTF-8 safe. Adds "..." prefix if truncated.
fn truncate_start(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let skip = char_count - (max_chars - 3); // -3 for "..."
        format!("...{}", s.chars().skip(skip).collect::<String>())
    }
}

/// Truncate a string from the end, keeping the first `max_chars` characters.
/// UTF-8 safe. Adds "..." suffix if truncated.
fn truncate_end(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let take = max_chars - 3; // -3 for "..."
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
) -> CliResult<()> {
    use rusqlite::Connection;
    use std::time::{SystemTime, UNIX_EPOCH};

    let data_dir = data_dir_override.clone().unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    let index_path = data_dir.join("tantivy_index");
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
    let is_stale = index_age_secs
        .map(|age| age > stale_threshold)
        .unwrap_or(true);

    // Check for pending sessions from watch_state.json
    let pending_sessions = if watch_state_path.exists() {
        std::fs::read_to_string(&watch_state_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|v| v.get("pending_count").and_then(|c| c.as_u64()))
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
            format!(" ({} sessions pending)", pending_sessions)
        } else {
            String::new()
        };
        Some(format!(
            "Run 'cass index' to refresh the index{}",
            pending_msg
        ))
    } else {
        None
    };

    if json {
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
            },
            "recommended_action": recommended_action,
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

        println!("{} CASS Status: {}", status_icon, status_word);
        println!();

        // Index info
        println!("Index:");
        if index_exists {
            if let Some(age) = index_age_secs {
                let age_str = if age < 60 {
                    format!("{} seconds ago", age)
                } else if age < 3600 {
                    format!("{} minutes ago", age / 60)
                } else if age < 86400 {
                    format!("{} hours ago", age / 3600)
                } else {
                    format!("{} days ago", age / 86400)
                };
                let stale_indicator = if is_stale { " (stale)" } else { "" };
                println!("  Last indexed: {}{}", age_str, stale_indicator);
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
            println!("  Conversations: {}", conversation_count);
            println!("  Messages: {}", message_count);
        } else {
            println!("  Not found");
        }

        // Pending
        if pending_sessions > 0 {
            println!();
            println!("Pending: {} sessions awaiting indexing", pending_sessions);
        }

        // Recommended action
        if let Some(action) = &recommended_action {
            println!();
            println!("Recommended: {}", action);
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
    /// Enum values if value_type is "enum"
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
            "view_command".to_string(),
            "status_command".to_string(),
            "state_command".to_string(),
            "api_version_command".to_string(),
            "introspect_command".to_string(),
        ],
        connectors: vec![
            "codex".to_string(),
            "claude_code".to_string(),
            "gemini".to_string(),
            "opencode".to_string(),
            "amp".to_string(),
            "cline".to_string(),
            "aider".to_string(),
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
            println!("  - {}", feature);
        }
        println!();
        println!("Connectors:");
        for connector in &response.connectors {
            println!("  - {}", connector);
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
                .map(|d| format!(" [default: {}]", d))
                .unwrap_or_default();
            let enum_values = flag
                .enum_values
                .as_ref()
                .map(|vals| format!(" [values: {}]", vals.join(",")))
                .unwrap_or_default();
            let short = flag.short.map(|s| format!("-{}, ", s)).unwrap_or_default();
            let prefix = if flag.arg_type == "positional" {
                "".to_string()
            } else {
                format!("{}--", short)
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
                        .map(|d| format!(" [default: {}]", d))
                        .unwrap_or_default();
                    let short = arg.short.map(|s| format!("-{}, ", s)).unwrap_or_default();
                    let prefix = if arg.arg_type == "positional" {
                        "".to_string()
                    } else {
                        format!("{}--", short)
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
            println!("  - {}", name);
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
        println!("contract: v{}", CONTRACT_VERSION);
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
            .map(|s| s.to_string())
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
        name: arg
            .get_long()
            .map(|s| s.to_string())
            .unwrap_or_else(|| arg.get_id().as_str().to_string()),
        short: arg.get_short(),
        description: arg
            .get_help()
            .or_else(|| arg.get_long_help())
            .map(|s| s.to_string())
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
    let name = arg
        .get_long()
        .map(|s| s.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_string());

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
                            "match_type": { "type": ["string", "null"] }
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
            println!("Line: {} (context: {})", target_line, context);
        }
        println!("----------------------------------------");
        for (i, l) in lines.iter().enumerate().skip(start).take(end - start) {
            let line_num = i + 1;
            let marker = if highlight_line && line_num == target_line {
                ">"
            } else {
                " "
            };
            println!("{}{:5} | {}", marker, line_num, l);
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
) -> CliResult<()> {
    use rusqlite::Connection;
    use std::time::Instant;

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    let watch_once_paths = watch_once
        .filter(|paths| !paths.is_empty())
        .or_else(read_watch_once_paths_env);
    let opts = IndexOptions {
        full,
        force_rebuild,
        watch,
        watch_once_paths: watch_once_paths.clone(),
        db_path: db_path.clone(),
        data_dir: data_dir.clone(),
        progress: None,
    };
    let spinner = if !json {
        match progress {
            ProgressResolved::Bars => Some(indicatif::ProgressBar::new_spinner()),
            ProgressResolved::Plain => None,
            ProgressResolved::None => None,
        }
    } else {
        None
    };
    if let Some(pb) = &spinner {
        pb.set_message(if full { "index --full" } else { "index" });
        pb.enable_steady_tick(Duration::from_millis(120));
    } else if !json && matches!(progress, ProgressResolved::Plain) {
        eprintln!(
            "index starting (full={}, watch={}, watch_once={})",
            full,
            watch,
            watch_once_paths
                .as_ref()
                .map(|p| p.len())
                .unwrap_or_default()
        );
    }

    let start = Instant::now();
    // CLI index command doesn't support manual reindex triggering from TUI, so pass None
    let res = indexer::run_index(opts, None).map_err(|e| {
        let chain = e
            .chain()
            .map(|c| c.to_string())
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
        let payload = serde_json::json!({
            "success": true,
            "elapsed_ms": elapsed_ms,
            "full": full,
            "force_rebuild": force_rebuild,
            "data_dir": data_dir.display().to_string(),
            "db_path": db_path.display().to_string(),
            "conversations": conversations,
            "messages": messages,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    }

    if let Some(pb) = spinner {
        pb.finish_and_clear();
    } else if !json && matches!(progress, ProgressResolved::Plain) {
        eprintln!("index completed");
    }

    res
}

pub fn default_db_path() -> PathBuf {
    default_data_dir().join("agent_search.db")
}

pub fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "coding-agent-search", "coding-agent-search")
        .expect("project dirs available")
        .data_dir()
        .to_path_buf()
}

const OWNER: &str = "Dicklesworthstone";
const REPO: &str = "coding_agent_session_search";

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
}

async fn maybe_prompt_for_update(once: bool) -> Result<()> {
    if once
        || std::env::var("CI").is_ok()
        || std::env::var("TUI_HEADLESS").is_ok()
        || std::env::var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT").is_ok()
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
        "A newer version is available: current v{}, latest {}. Update now? (y/N): ",
        current_ver, latest_tag
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
