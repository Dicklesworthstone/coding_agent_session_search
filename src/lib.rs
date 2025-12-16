pub mod bookmarks;
pub mod connectors;
pub mod export;
pub mod indexer;
pub mod model;
pub mod search;
pub mod sources;
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
        Some(Commands::Context { .. }) => "context".to_string(),
        Some(Commands::Export { .. }) => "export".to_string(),
        Some(Commands::Expand { .. }) => "expand".to_string(),
        Some(Commands::Timeline { .. }) => "timeline".to_string(),
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
        // Note: We lowercase both and find matches in the lowercased version,
        // but the matched substring length in the original might differ from term.len()
        // for certain Unicode characters. We use the actual matched length from lower_result.
        let lower_result = result.to_lowercase();
        let lower_term = term.to_lowercase();
        let mut new_result = String::new();
        let mut last_end = 0;

        for (idx, matched_str) in lower_result.match_indices(&lower_term) {
            // Skip if this overlaps with a previous highlight (from a longer term)
            if idx < last_end {
                continue;
            }
            // Append text before this match
            new_result.push_str(&result[last_end..idx]);
            // Append highlighted match (preserve original case)
            // Use matched_str.len() which is the actual byte length in the lowercased string
            new_result.push_str(start_mark);
            new_result.push_str(&result[idx..idx + matched_str.len()]);
            new_result.push_str(end_mark);
            last_end = idx + matched_str.len();
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
        "Topics: commands | env | paths | schemas | guide | exit-codes | examples | contracts | wrap",
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
    robot_auto: bool,
    time_filter: TimeFilter,
    aggregate: Option<Vec<String>>,
    explain: bool,
    dry_run: bool,
    timeout_ms: Option<u64>,
    highlight: bool,
    source: Option<String>,
) -> CliResult<()> {
    use crate::search::query::{QueryExplanation, SearchClient, SearchFilters};
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
) -> CliResult<()> {
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
        .filter_map(std::result::Result::ok)
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
        .filter_map(std::result::Result::ok)
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
    let opts = IndexOptions {
        full,
        force_rebuild,
        watch,
        watch_once_paths: watch_once_paths.clone(),
        db_path: db_path.clone(),
        data_dir: data_dir.clone(),
        progress: None,
    };
    let spinner = if json {
        None
    } else {
        match progress {
            ProgressResolved::Bars => Some(indicatif::ProgressBar::new_spinner()),
            ProgressResolved::Plain => None,
            ProgressResolved::None => None,
        }
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
                .map(std::vec::Vec::len)
                .unwrap_or_default()
        );
    }

    let start = Instant::now();
    // CLI index command doesn't support manual reindex triggering from TUI, so pass None
    let res = indexer::run_index(opts, None).map_err(|e| {
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
        .map(|p| p.data_dir().to_path_buf())
        .or_else(|| dirs::home_dir().map(|h| h.join(".coding-agent-search")))
        .unwrap_or_else(|| PathBuf::from("./data"))
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

    let file = File::open(path).map_err(|e| CliError {
        code: 9,
        kind: "file-open",
        message: format!("Failed to open file: {e}"),
        hint: None,
        retryable: false,
    })?;

    let reader = BufReader::new(file);
    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut session_title: Option<String> = None;
    let mut session_start: Option<i64> = None;
    let mut session_end: Option<i64> = None;

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(ts) = msg.get("timestamp").and_then(|t| t.as_i64()) {
                if session_start.is_none() || ts < session_start.unwrap() {
                    session_start = Some(ts);
                }
                if session_end.is_none() || ts > session_end.unwrap() {
                    session_end = Some(ts);
                }
            }
            messages.push(msg);
        }
    }

    if messages.is_empty() {
        return Err(CliError {
            code: 9,
            kind: "empty-session",
            message: format!("No messages found in: {}", path.display()),
            hint: None,
            retryable: false,
        });
    }

    // Find title from first user message
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
        && let Some(dt) = Utc.timestamp_opt(ts, 0).single()
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
        .and_then(|ts| Utc.timestamp_opt(ts, 0).single())
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
    // Try direct content first (standard format)
    if let Some(content) = msg.get("content") {
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        if let Some(arr) = content.as_array() {
            let mut result = String::new();
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = block.get("text").and_then(|t| t.as_str())
                {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(text);
                }
            }
            if !result.is_empty() {
                return result;
            }
        }
    }
    // Try nested message.content (Claude Code format)
    if let Some(inner) = msg.get("message")
        && let Some(content) = inner.get("content")
    {
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        if let Some(arr) = content.as_array() {
            let mut result = String::new();
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = block.get("text").and_then(|t| t.as_str())
                {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(text);
                }
            }
            return result;
        }
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
    use chrono::{Local, TimeZone, Utc};
    use crate::sources::provenance::SourceFilter;
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
        let local_start = Local.from_local_datetime(&start_of_day).single().unwrap();
        (local_start.timestamp(), now.timestamp())
    } else {
        let start = since
            .and_then(parse_datetime_flexible)
            .unwrap_or_else(|| (now - chrono::Duration::days(7)).timestamp());
        let end = until
            .and_then(parse_datetime_flexible)
            .unwrap_or_else(|| now.timestamp());
        (start, end)
    };

    let mut sql = String::from(
        "SELECT c.id, a.slug as agent, c.title, c.started_at, c.ended_at, c.source_path,
                COUNT(m.id) as message_count, c.source_id
         FROM conversations c
         JOIN agents a ON c.agent_id = a.id
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
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?, // source_id (P3.2)
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
    let mut sessions: Vec<(i64, String, Option<String>, i64, Option<i64>, String, i64, String)> =
        Vec::new();
    for r in rows.flatten() {
        sessions.push(r);
    }

    if json {
        let output = match group_by {
            TimelineGrouping::None => {
                let items: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|(id, agent, title, started, ended, path, msg_count, source_id)| {
                        let duration = ended.map(|e| e - started);
                        serde_json::json!({
                            "id": id, "agent": agent, "title": title,
                            "started_at": started, "ended_at": ended,
                            "duration_seconds": duration, "source_path": path,
                            "message_count": msg_count, "source_id": source_id,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "range": { "start": start_ts, "end": end_ts },
                    "total_sessions": sessions.len(),
                    "sessions": items,
                })
            }
            TimelineGrouping::Hour | TimelineGrouping::Day => {
                let mut groups: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
                for (id, agent, title, started, ended, path, msg_count, source_id) in &sessions {
                    let dt = Utc
                        .timestamp_opt(*started, 0)
                        .single()
                        .unwrap_or_else(Utc::now);
                    let key = match group_by {
                        TimelineGrouping::Hour => dt.format("%Y-%m-%d %H:00").to_string(),
                        TimelineGrouping::Day => dt.format("%Y-%m-%d").to_string(),
                        _ => unreachable!(),
                    };
                    groups.entry(key).or_default().push(serde_json::json!({
                        "id": id, "agent": agent, "title": title,
                        "started_at": started, "ended_at": ended,
                        "source_path": path, "message_count": msg_count,
                        "source_id": source_id,
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
            .timestamp_opt(start_ts, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let end_dt = Utc
            .timestamp_opt(end_ts, 0)
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
        for (_id, agent, title, started, ended, _path, msg_count, source_id) in &sessions {
            let dt = Utc
                .timestamp_opt(*started, 0)
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
                let mins = (e - started) / 60;
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

            // Source badge for remote sessions (P3.2)
            let source_badge = if source_id != "local" {
                format!(" [{}]", source_id)
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

fn parse_datetime_flexible(s: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, TimeZone};

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }

    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        && let Some(dt) = date.and_hms_opt(0, 0, 0)
        && let Some(local) = Local.from_local_datetime(&dt).single()
    {
        return Some(local.timestamp());
    }

    let now = Local::now();
    match s.to_lowercase().as_str() {
        "today" => {
            let start = now.date_naive().and_hms_opt(0, 0, 0)?;
            Local
                .from_local_datetime(&start)
                .single()
                .map(|d| d.timestamp())
        }
        "yesterday" => {
            let yesterday = (now - chrono::Duration::days(1)).date_naive();
            let start = yesterday.and_hms_opt(0, 0, 0)?;
            Local
                .from_local_datetime(&start)
                .single()
                .map(|d| d.timestamp())
        }
        _ => {
            if let Some(days_str) = s.strip_suffix('d')
                && let Ok(days) = days_str.parse::<i64>()
            {
                return Some((now - chrono::Duration::days(days)).timestamp());
            }
            if let Some(hours_str) = s.strip_suffix('h')
                && let Ok(hours) = hours_str.parse::<i64>()
            {
                return Some((now - chrono::Duration::hours(hours)).timestamp());
            }
            None
        }
    }
}
