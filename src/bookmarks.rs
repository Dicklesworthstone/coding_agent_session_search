//! Bookmarks system for saving and annotating search results.
//!
//! Provides persistent storage for bookmarked search results with user notes
//! and tags. Uses a separate `SQLite` database file to avoid schema conflicts.

use crate::franken_sync::Connection;
use crate::franken_sync::compat::{ConnectionExt, OptionalExtension, RowExt, TransactionExt};
use crate::franken_sync::params;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A bookmarked search result with optional note and tags
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    /// Unique bookmark ID
    pub id: i64,
    /// Title/summary of the bookmarked result
    pub title: String,
    /// Path to the source file
    pub source_path: String,
    /// Line number in the source (if applicable)
    pub line_number: Option<usize>,
    /// Agent that produced this result
    pub agent: String,
    /// Workspace path
    pub workspace: String,
    /// User's note/annotation
    pub note: String,
    /// Comma-separated tags
    pub tags: String,
    /// When the bookmark was created (unix millis)
    pub created_at: i64,
    /// When the bookmark was last updated (unix millis)
    pub updated_at: i64,
    /// Original search snippet (for context)
    pub snippet: String,
}

impl Bookmark {
    /// Create a new bookmark from search result data
    pub fn new(
        title: impl Into<String>,
        source_path: impl Into<String>,
        agent: impl Into<String>,
        workspace: impl Into<String>,
    ) -> Self {
        let now = current_timestamp();

        Self {
            id: 0, // Set by database on insert
            title: title.into(),
            source_path: source_path.into(),
            line_number: None,
            agent: agent.into(),
            workspace: workspace.into(),
            note: String::new(),
            tags: String::new(),
            created_at: now,
            updated_at: now,
            snippet: String::new(),
        }
    }

    /// Add a note to the bookmark
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    /// Add tags to the bookmark
    pub fn with_tags(mut self, tags: impl Into<String>) -> Self {
        self.tags = tags.into();
        self
    }

    /// Set line number
    pub fn with_line(mut self, line: usize) -> Self {
        self.line_number = Some(line);
        self
    }

    /// Set snippet
    pub fn with_snippet(mut self, snippet: impl Into<String>) -> Self {
        self.snippet = snippet.into();
        self
    }

    /// Get tags as a vector
    pub fn tag_list(&self) -> Vec<&str> {
        self.tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Check if bookmark has a specific tag
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tag_list().iter().any(|t| t.eq_ignore_ascii_case(tag))
    }
}

/// Storage backend for bookmarks using `SQLite`
pub struct BookmarkStore {
    conn: Connection,
}

impl BookmarkStore {
    /// Open or create a bookmark store at the given path
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating bookmarks directory {}", parent.display()))?;
        }

        let conn = Connection::open(path.to_string_lossy().as_ref())
            .with_context(|| format!("opening bookmarks db at {}", path.display()))?;

        // Apply pragmas for performance and concurrency safety
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;",
        )?;

        // Create schema if needed
        conn.execute_batch(SCHEMA)?;

        Ok(Self { conn })
    }

    /// Open bookmark store at the default location (`data_dir/bookmarks.db`)
    pub fn open_default() -> Result<Self> {
        let path = default_bookmarks_path();
        Self::open(&path)
    }

    /// Add a new bookmark
    pub fn add(&self, bookmark: &Bookmark) -> Result<i64> {
        let line_number = line_number_to_db(bookmark.line_number)?;

        self.conn.execute_compat(
            "INSERT INTO bookmarks (title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                bookmark.title.as_str(),
                bookmark.source_path.as_str(),
                line_number,
                bookmark.agent.as_str(),
                bookmark.workspace.as_str(),
                bookmark.note.as_str(),
                bookmark.tags.as_str(),
                bookmark.created_at,
                bookmark.updated_at,
                bookmark.snippet.as_str(),
            ],
        )?;

        let rowid = self.conn.last_insert_rowid();
        Ok(rowid)
    }

    /// Update an existing bookmark
    pub fn update(&self, bookmark: &Bookmark) -> Result<bool> {
        let now = current_timestamp();

        let rows = self.conn.execute_compat(
            "UPDATE bookmarks SET title = ?1, note = ?2, tags = ?3, updated_at = ?4 WHERE id = ?5",
            params![
                bookmark.title.as_str(),
                bookmark.note.as_str(),
                bookmark.tags.as_str(),
                now,
                bookmark.id
            ],
        )?;

        Ok(rows > 0)
    }

    /// Remove a bookmark by ID
    pub fn remove(&self, id: i64) -> Result<bool> {
        let rows = self
            .conn
            .execute_compat("DELETE FROM bookmarks WHERE id = ?1", params![id])?;
        Ok(rows > 0)
    }

    /// Get a bookmark by ID
    pub fn get(&self, id: i64) -> Result<Option<Bookmark>> {
        self.conn
            .query_row_map(
                "SELECT id, title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet
                 FROM bookmarks WHERE id = ?1",
                params![id],
                row_to_bookmark,
            )
            .optional()
            .context("querying bookmark by id")
    }

    /// List all bookmarks, optionally filtered by tag
    pub fn list(&self, tag_filter: Option<&str>) -> Result<Vec<Bookmark>> {
        let sql = "SELECT id, title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet
                   FROM bookmarks ORDER BY created_at DESC";

        let all_bookmarks: Vec<Bookmark> =
            self.conn.query_map_collect(sql, &[], row_to_bookmark)?;

        if let Some(tag) = tag_filter {
            Ok(all_bookmarks
                .into_iter()
                .filter(|b| b.has_tag(tag))
                .collect())
        } else {
            Ok(all_bookmarks)
        }
    }

    /// Search bookmarks by text (title, note, snippet)
    pub fn search(&self, query: &str) -> Result<Vec<Bookmark>> {
        // Escape SQL LIKE wildcards so they are matched literally
        let escaped = query
            .to_lowercase()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");

        let results = self.conn.query_map_collect(
            "SELECT id, title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet
             FROM bookmarks
             WHERE LOWER(title) LIKE ?1 ESCAPE '\\' OR LOWER(note) LIKE ?1 ESCAPE '\\' OR LOWER(snippet) LIKE ?1 ESCAPE '\\'
             ORDER BY created_at DESC",
            params![pattern],
            row_to_bookmark,
        ).context("searching bookmarks")?;
        Ok(results)
    }

    /// Get all unique tags
    pub fn all_tags(&self) -> Result<Vec<String>> {
        let bookmarks = self.list(None)?;
        let mut tags: Vec<String> = bookmarks
            .iter()
            .flat_map(|b| b.tag_list())
            .map(std::string::ToString::to_string)
            .collect();

        tags.sort();
        tags.dedup();
        Ok(tags)
    }

    /// Count total bookmarks
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row_map(
            "SELECT COUNT(*) FROM bookmarks",
            &[],
            |row: &crate::franken_sync::Row| row.get_typed(0),
        )?;
        usize::try_from(count).context("bookmark count is out of range")
    }

    /// Check if a `source_path` + line is already bookmarked
    pub fn is_bookmarked(&self, source_path: &str, line_number: Option<usize>) -> Result<bool> {
        let line_number = line_number_to_db(line_number)?;
        let exists: i64 = self.conn.query_row_map(
            "SELECT EXISTS(SELECT 1 FROM bookmarks WHERE source_path = ?1 AND line_number IS ?2)",
            params![source_path, line_number],
            |row: &crate::franken_sync::Row| row.get_typed(0),
        )?;
        Ok(exists != 0)
    }

    /// Export all bookmarks to JSON
    pub fn export_json(&self) -> Result<String> {
        let bookmarks = self.list(None)?;
        serde_json::to_string_pretty(&bookmarks).context("serializing bookmarks to JSON")
    }

    /// Import bookmarks from JSON (merges, doesn't overwrite)
    pub fn import_json(&self, json: &str) -> Result<usize> {
        let bookmarks: Vec<Bookmark> =
            serde_json::from_str(json).context("parsing bookmark JSON")?;
        let mut imported = 0;

        let mut tx = self.conn.transaction()?;

        for mut bookmark in bookmarks {
            let line_number = line_number_to_db(bookmark.line_number)?;

            // Check for duplicates
            let check_params = params![bookmark.source_path.as_str(), line_number];
            let check_values = crate::franken_sync::compat::param_slice_to_values(check_params);
            let exists_row = tx.query_with_params(
                "SELECT EXISTS(SELECT 1 FROM bookmarks WHERE source_path = ?1 AND line_number IS ?2)",
                &check_values,
            )?;
            let exists: i64 = exists_row
                .first()
                .ok_or_else(|| {
                    crate::franken_sync::FrankenError::Internal(
                        "bookmark schema-incompatible: duplicate probe returned no row".to_string(),
                    )
                })?
                .get_typed(0)
                .map_err(|error| {
                    crate::franken_sync::FrankenError::Internal(format!(
                        "bookmark schema-incompatible: duplicate probe decode failed: {error}"
                    ))
                })?;

            if exists == 0 {
                bookmark.id = 0; // Reset ID for new insert
                tx.execute_compat(
                    "INSERT INTO bookmarks (title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        bookmark.title.as_str(),
                        bookmark.source_path.as_str(),
                        line_number,
                        bookmark.agent.as_str(),
                        bookmark.workspace.as_str(),
                        bookmark.note.as_str(),
                        bookmark.tags.as_str(),
                        bookmark.created_at,
                        bookmark.updated_at,
                        bookmark.snippet.as_str(),
                    ],
                )?;
                imported += 1;
            }
        }

        tx.commit()?;

        Ok(imported)
    }
}

/// Convert a database row to a Bookmark
fn row_to_bookmark(
    row: &crate::franken_sync::Row,
) -> Result<Bookmark, crate::franken_sync::FrankenError> {
    Ok(Bookmark {
        id: row.get_typed(0)?,
        title: row.get_typed(1)?,
        source_path: row.get_typed(2)?,
        line_number: line_number_from_db(row.get_typed::<Option<i64>>(3)?)?,
        agent: row.get_typed(4)?,
        workspace: row.get_typed(5)?,
        note: row.get_typed(6)?,
        tags: row.get_typed(7)?,
        created_at: row.get_typed(8)?,
        updated_at: row.get_typed(9)?,
        snippet: row.get_typed(10)?,
    })
}

/// File name of the bookmarks database inside the data directory.
pub const BOOKMARKS_DB_FILE: &str = "bookmarks.db";

/// Location of the bookmarks database inside a specific data directory.
pub fn bookmarks_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(BOOKMARKS_DB_FILE)
}

/// Get the default bookmarks database path
pub fn default_bookmarks_path() -> PathBuf {
    bookmarks_path_in(&crate::default_data_dir())
}

// ---------------------------------------------------------------------------
// CLI surface: `cass bookmarks <add|list|search|remove|export|import>`
// ---------------------------------------------------------------------------

/// Exit code for a bookmark command that completed successfully.
pub const BOOKMARKS_EXIT_OK: i32 = 0;
/// Exit code for usage errors (bad arguments, unparsable import file).
pub const BOOKMARKS_EXIT_USAGE: i32 = 2;
/// Exit code when the requested bookmark id does not exist. Uses the crate's
/// documented "mapping / not-found" class (13); 3 is reserved for a missing
/// index/archive and would misroute agents to `cass index --full`.
pub const BOOKMARKS_EXIT_NOT_FOUND: i32 = 13;
/// Exit code for filesystem or database failures: the crate's documented
/// I/O class (14); 4 is reserved for network errors.
pub const BOOKMARKS_EXIT_IO: i32 = 14;

/// Agent name recorded when `add` is invoked without `--agent`.
const DEFAULT_BOOKMARK_AGENT: &str = "unknown";

/// Arguments for the `cass bookmarks` command family.
///
/// The dispatcher only needs to hand this struct to
/// [`run_bookmarks_command`]; every subcommand resolves its own data
/// directory and output mode.
#[derive(Debug, Clone, Args)]
pub struct BookmarksArgs {
    /// Bookmark operation to run.
    #[command(subcommand)]
    pub command: BookmarksCommand,
}

/// Operations available under `cass bookmarks`.
///
/// Every variant accepts `--json` (alias `--robot`) for a single machine
/// readable document on stdout, and `--data-dir` to override where
/// `bookmarks.db` lives (default: the crate's data directory).
#[derive(Debug, Clone, Subcommand)]
pub enum BookmarksCommand {
    /// Save a bookmark pointing at a source file (optionally a specific line).
    Add {
        /// Path of the session/file being bookmarked (stored verbatim).
        source_path: PathBuf,
        /// Line number inside the source, as shown by `cass search`.
        #[arg(long, short = 'n')]
        line: Option<u64>,
        /// Human title; defaults to the source file name.
        #[arg(long)]
        title: Option<String>,
        /// Free-form note or annotation.
        #[arg(long)]
        note: Option<String>,
        /// Comma-separated tags (e.g. `rust,important`).
        #[arg(long)]
        tags: Option<String>,
        /// Agent that produced the bookmarked content.
        #[arg(long)]
        agent: Option<String>,
        /// Workspace/project path the content belongs to.
        #[arg(long)]
        workspace: Option<String>,
        /// Original search snippet kept for context.
        #[arg(long)]
        snippet: Option<String>,
        /// Emit a single JSON document instead of human-readable lines.
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override the data directory holding `bookmarks.db`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// List saved bookmarks, newest first.
    List {
        /// Maximum number of bookmarks to print.
        #[arg(long)]
        limit: Option<usize>,
        /// Only show bookmarks carrying this tag (case-insensitive).
        #[arg(long)]
        tag: Option<String>,
        /// Emit a single JSON document instead of human-readable lines.
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override the data directory holding `bookmarks.db`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Search bookmark titles, notes, and snippets (case-insensitive substring).
    Search {
        /// Text to look for.
        query: String,
        /// Maximum number of matches to print.
        #[arg(long)]
        limit: Option<usize>,
        /// Emit a single JSON document instead of human-readable lines.
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override the data directory holding `bookmarks.db`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Delete a bookmark by id.
    Remove {
        /// Bookmark id as shown by `list`.
        id: i64,
        /// Emit a single JSON document instead of human-readable lines.
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override the data directory holding `bookmarks.db`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Export all bookmarks as JSON (to a file, or stdout when no `--output`).
    Export {
        /// Destination file; parent directories are created as needed.
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        /// Emit a single JSON document instead of human-readable lines.
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override the data directory holding `bookmarks.db`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Import bookmarks from a JSON file produced by `export` (duplicates skipped).
    Import {
        /// JSON file: either an array of bookmarks or `{"bookmarks": [...]}`.
        input: PathBuf,
        /// Emit a single JSON document instead of human-readable lines.
        #[arg(long, visible_alias = "robot")]
        json: bool,
        /// Override the data directory holding `bookmarks.db`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

impl BookmarksCommand {
    /// Whether the subcommand was invoked with `--json`/`--robot`.
    pub fn json(&self) -> bool {
        match self {
            Self::Add { json, .. }
            | Self::List { json, .. }
            | Self::Search { json, .. }
            | Self::Remove { json, .. }
            | Self::Export { json, .. }
            | Self::Import { json, .. } => *json,
        }
    }

    /// Explicit `--data-dir` override, if any.
    pub fn data_dir(&self) -> Option<&Path> {
        match self {
            Self::Add { data_dir, .. }
            | Self::List { data_dir, .. }
            | Self::Search { data_dir, .. }
            | Self::Remove { data_dir, .. }
            | Self::Export { data_dir, .. }
            | Self::Import { data_dir, .. } => data_dir.as_deref(),
        }
    }

    /// Data directory this invocation operates on (override or crate default).
    pub fn resolved_data_dir(&self) -> PathBuf {
        self.data_dir()
            .map_or_else(crate::default_data_dir, Path::to_path_buf)
    }
}

/// Failure raised by a bookmark CLI command.
///
/// Carries the process exit code plus the machine-readable `kind` that the
/// `--json` error envelope exposes (`usage`, `bookmark-not-found`, `io`).
#[derive(Debug, Clone)]
struct BookmarkCliError {
    code: i32,
    kind: &'static str,
    message: String,
    hint: Option<String>,
}

impl BookmarkCliError {
    fn usage(message: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            code: BOOKMARKS_EXIT_USAGE,
            kind: "usage",
            message: message.into(),
            hint: hint.map(str::to_string),
        }
    }

    fn not_found(id: i64) -> Self {
        Self {
            code: BOOKMARKS_EXIT_NOT_FOUND,
            kind: "bookmark-not-found",
            message: format!("bookmark {id} not found"),
            hint: Some("Run `cass bookmarks list` to see the ids that exist.".to_string()),
        }
    }

    fn io(context: &str, detail: impl std::fmt::Display) -> Self {
        Self {
            code: BOOKMARKS_EXIT_IO,
            kind: "io",
            message: format!("{context}: {detail}"),
            hint: None,
        }
    }

    /// Print the failure to stderr, as a JSON envelope when `json` is set.
    ///
    /// The envelope mirrors the crate's structured CLI error shape:
    /// `{"success": false, "error": {code, kind, message, hint, retryable}}`.
    fn report(&self, json: bool) {
        if json {
            eprintln!(
                "{}",
                serde_json::json!({
                    "success": false,
                    "error": {
                        "code": self.code,
                        "kind": self.kind,
                        "message": self.message,
                        "hint": self.hint,
                        "retryable": false
                    }
                })
            );
        } else {
            eprintln!("error: {}", self.message);
            if let Some(hint) = &self.hint {
                eprintln!("hint: {hint}");
            }
        }
    }
}

type BookmarkCliResult<T> = std::result::Result<T, BookmarkCliError>;

/// Run a `cass bookmarks` subcommand, writing its output to stdout.
///
/// Returns the process exit code: `0` on success, `2` for usage errors,
/// `13` (the crate's not-found class) when a bookmark id does not exist,
/// `14` (the crate's I/O class) for filesystem/database failures. Command-level failures are reported on stderr (as a JSON
/// envelope under `--json`) and surfaced through the exit code rather than
/// as `Err`, so callers can dispatch this uniformly with the other command
/// entry points.
pub fn run_bookmarks_command(args: BookmarksArgs) -> Result<i32> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    run_bookmarks_command_to(args, &mut out)
}

/// Run a `cass bookmarks` subcommand, writing its stdout payload to `out`.
///
/// Same contract as [`run_bookmarks_command`]; diagnostics still go to the
/// real stderr. Exposed so tests and embedders can capture the output.
pub fn run_bookmarks_command_to(args: BookmarksArgs, out: &mut dyn Write) -> Result<i32> {
    let json = args.command.json();
    match execute(&args.command, out) {
        Ok(code) => Ok(code),
        Err(err) => {
            err.report(json);
            Ok(err.code)
        }
    }
}

fn execute(command: &BookmarksCommand, out: &mut dyn Write) -> BookmarkCliResult<i32> {
    let json = command.json();
    let db_path = bookmarks_path_in(&command.resolved_data_dir());

    match command {
        BookmarksCommand::Add {
            source_path,
            line,
            title,
            note,
            tags,
            agent,
            workspace,
            snippet,
            ..
        } => {
            let source = source_path.to_string_lossy().into_owned();
            if source.trim().is_empty() {
                return Err(BookmarkCliError::usage(
                    "source path must not be empty",
                    None,
                ));
            }
            let line = line
                .map(|n| {
                    usize::try_from(n).map_err(|_| {
                        BookmarkCliError::usage(
                            format!("line number {n} is too large for this platform"),
                            None,
                        )
                    })
                })
                .transpose()?;
            let title = title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map_or_else(|| default_title(source_path), str::to_string);

            let mut bookmark = Bookmark::new(
                title,
                source,
                agent.as_deref().unwrap_or(DEFAULT_BOOKMARK_AGENT),
                workspace.as_deref().unwrap_or_default(),
            );
            if let Some(line) = line {
                bookmark = bookmark.with_line(line);
            }
            if let Some(note) = note {
                bookmark = bookmark.with_note(note.as_str());
            }
            if let Some(tags) = tags {
                bookmark = bookmark.with_tags(tags.as_str());
            }
            if let Some(snippet) = snippet {
                bookmark = bookmark.with_snippet(snippet.as_str());
            }

            let store = open_store(&db_path)?;
            let id = store
                .add(&bookmark)
                .map_err(|e| BookmarkCliError::io("adding bookmark", format!("{e:#}")))?;
            let saved = store
                .get(id)
                .map_err(|e| BookmarkCliError::io("reading back bookmark", format!("{e:#}")))?
                .ok_or_else(|| {
                    BookmarkCliError::io(
                        "reading back bookmark",
                        format!("id {id} is missing right after insert"),
                    )
                })?;

            if json {
                write_json_line(
                    out,
                    &serde_json::json!({ "success": true, "bookmark": saved }),
                )?;
            } else {
                write_line(
                    out,
                    &format!(
                        "Saved bookmark #{id}: {} ({})",
                        saved.title,
                        describe_location(&saved)
                    ),
                )?;
            }
            Ok(BOOKMARKS_EXIT_OK)
        }

        BookmarksCommand::List { limit, tag, .. } => {
            let store = open_store(&db_path)?;
            let mut bookmarks = store
                .list(tag.as_deref())
                .map_err(|e| BookmarkCliError::io("listing bookmarks", format!("{e:#}")))?;
            if let Some(limit) = limit {
                bookmarks.truncate(*limit);
            }
            emit_bookmarks(out, json, &bookmarks)?;
            Ok(BOOKMARKS_EXIT_OK)
        }

        BookmarksCommand::Search { query, limit, .. } => {
            let query = query.trim();
            if query.is_empty() {
                return Err(BookmarkCliError::usage(
                    "search query must not be empty",
                    Some("Pass the text to look for, e.g. `cass bookmarks search auth`."),
                ));
            }
            let store = open_store(&db_path)?;
            let mut bookmarks = store
                .search(query)
                .map_err(|e| BookmarkCliError::io("searching bookmarks", format!("{e:#}")))?;
            if let Some(limit) = limit {
                bookmarks.truncate(*limit);
            }
            emit_bookmarks(out, json, &bookmarks)?;
            Ok(BOOKMARKS_EXIT_OK)
        }

        BookmarksCommand::Remove { id, .. } => {
            let store = open_store(&db_path)?;
            let removed = store
                .remove(*id)
                .map_err(|e| BookmarkCliError::io("removing bookmark", format!("{e:#}")))?;
            if !removed {
                return Err(BookmarkCliError::not_found(*id));
            }
            if json {
                write_json_line(out, &serde_json::json!({ "success": true, "removed": id }))?;
            } else {
                write_line(out, &format!("Removed bookmark #{id}"))?;
            }
            Ok(BOOKMARKS_EXIT_OK)
        }

        BookmarksCommand::Export { output, .. } => {
            let store = open_store(&db_path)?;
            let bookmarks = store
                .list(None)
                .map_err(|e| BookmarkCliError::io("listing bookmarks", format!("{e:#}")))?;
            let count = bookmarks.len();

            match output {
                Some(path) => {
                    let payload = serde_json::to_string_pretty(&bookmarks).map_err(|e| {
                        BookmarkCliError::io("serializing bookmarks", e.to_string())
                    })?;
                    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            BookmarkCliError::io(
                                &format!("creating export directory {}", parent.display()),
                                e,
                            )
                        })?;
                    }
                    std::fs::write(path, payload).map_err(|e| {
                        BookmarkCliError::io(&format!("writing {}", path.display()), e)
                    })?;
                    let shown = path.display().to_string();
                    if json {
                        write_json_line(
                            out,
                            &serde_json::json!({
                                "success": true,
                                "exported": shown,
                                "count": count
                            }),
                        )?;
                    } else {
                        write_line(out, &format!("Exported {count} bookmark(s) to {shown}"))?;
                    }
                }
                None => {
                    if json {
                        // One document only: the envelope carries the payload.
                        write_json_line(
                            out,
                            &serde_json::json!({
                                "success": true,
                                "exported": "stdout",
                                "count": count,
                                "bookmarks": bookmarks
                            }),
                        )?;
                    } else {
                        // Raw export payload, directly re-importable.
                        let payload = serde_json::to_string_pretty(&bookmarks).map_err(|e| {
                            BookmarkCliError::io("serializing bookmarks", e.to_string())
                        })?;
                        write_line(out, &payload)?;
                    }
                }
            }
            Ok(BOOKMARKS_EXIT_OK)
        }

        BookmarksCommand::Import { input, .. } => {
            let raw = std::fs::read_to_string(input)
                .map_err(|e| BookmarkCliError::io(&format!("reading {}", input.display()), e))?;
            let bookmarks = parse_import_payload(&raw)?;
            let total = bookmarks.len();
            let payload = serde_json::to_string(&bookmarks)
                .map_err(|e| BookmarkCliError::io("serializing import payload", e.to_string()))?;

            let store = open_store(&db_path)?;
            let imported = store
                .import_json(&payload)
                .map_err(|e| BookmarkCliError::io("importing bookmarks", format!("{e:#}")))?;
            let skipped = total.saturating_sub(imported);

            if json {
                write_json_line(
                    out,
                    &serde_json::json!({
                        "success": true,
                        "imported": imported,
                        "skipped": skipped
                    }),
                )?;
            } else {
                write_line(
                    out,
                    &format!(
                        "Imported {imported} bookmark(s) from {} ({skipped} duplicate(s) skipped)",
                        input.display()
                    ),
                )?;
            }
            Ok(BOOKMARKS_EXIT_OK)
        }
    }
}

fn open_store(db_path: &Path) -> BookmarkCliResult<BookmarkStore> {
    BookmarkStore::open(db_path)
        .map_err(|e| BookmarkCliError::io("opening bookmarks database", format!("{e:#}")))
}

/// Title used by `add` when none is supplied: the source file name.
fn default_title(source_path: &Path) -> String {
    source_path.file_name().map_or_else(
        || source_path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `path` or `path:line` for human-readable output.
fn describe_location(bookmark: &Bookmark) -> String {
    match bookmark.line_number {
        Some(line) => format!("{}:{line}", bookmark.source_path),
        None => bookmark.source_path.clone(),
    }
}

/// Accept either a bare bookmark array or the `{"bookmarks": [...]}` envelope
/// that `export --json` prints, and validate every entry before touching the DB.
fn parse_import_payload(raw: &str) -> BookmarkCliResult<Vec<Bookmark>> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
        BookmarkCliError::usage(format!("import file is not valid JSON: {e}"), None)
    })?;
    let entries = match value {
        serde_json::Value::Array(entries) => entries,
        serde_json::Value::Object(mut map) => match map.remove("bookmarks") {
            Some(serde_json::Value::Array(entries)) => entries,
            _ => {
                return Err(BookmarkCliError::usage(
                    "import file must be a JSON array of bookmarks or an object with a \"bookmarks\" array",
                    Some("Use the output of `cass bookmarks export`."),
                ));
            }
        },
        _ => {
            return Err(BookmarkCliError::usage(
                "import file must be a JSON array of bookmarks or an object with a \"bookmarks\" array",
                Some("Use the output of `cass bookmarks export`."),
            ));
        }
    };
    serde_json::from_value(serde_json::Value::Array(entries)).map_err(|e| {
        BookmarkCliError::usage(
            format!("import file contains an invalid bookmark: {e}"),
            None,
        )
    })
}

/// Print a bookmark collection: a JSON envelope, or one block per bookmark.
fn emit_bookmarks(
    out: &mut dyn Write,
    json: bool,
    bookmarks: &[Bookmark],
) -> BookmarkCliResult<()> {
    if json {
        return write_json_line(
            out,
            &serde_json::json!({
                "success": true,
                "count": bookmarks.len(),
                "bookmarks": bookmarks
            }),
        );
    }
    if bookmarks.is_empty() {
        eprintln!("No bookmarks found.");
        return Ok(());
    }
    for bookmark in bookmarks {
        write_line(
            out,
            &format!(
                "#{}\t{}\t{}",
                bookmark.id,
                describe_location(bookmark),
                bookmark.title
            ),
        )?;
        let tags = bookmark.tags.trim();
        if !tags.is_empty() {
            write_line(out, &format!("\ttags: {tags}"))?;
        }
        let note = bookmark.note.trim();
        if !note.is_empty() {
            write_line(out, &format!("\tnote: {note}"))?;
        }
    }
    Ok(())
}

fn write_json_line(out: &mut dyn Write, value: &serde_json::Value) -> BookmarkCliResult<()> {
    let text = serde_json::to_string(value)
        .map_err(|e| BookmarkCliError::io("serializing output", e.to_string()))?;
    write_line(out, &text)
}

fn write_line(out: &mut dyn Write, text: &str) -> BookmarkCliResult<()> {
    writeln!(out, "{text}").map_err(|e| BookmarkCliError::io("writing output", e))?;
    out.flush()
        .map_err(|e| BookmarkCliError::io("flushing output", e))
}

/// SQL schema for bookmarks database
const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS bookmarks (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    source_path TEXT NOT NULL,
    line_number INTEGER,
    agent TEXT NOT NULL,
    workspace TEXT NOT NULL,
    note TEXT DEFAULT '',
    tags TEXT DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    snippet TEXT DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_bookmarks_source ON bookmarks(source_path, line_number);
CREATE INDEX IF NOT EXISTS idx_bookmarks_created ON bookmarks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_bookmarks_agent ON bookmarks(agent);
";

fn line_number_to_db(line_number: Option<usize>) -> Result<Option<i64>> {
    line_number
        .map(|n| i64::try_from(n).context("line number exceeds i64 range"))
        .transpose()
}

fn line_number_from_db(
    line_number: Option<i64>,
) -> Result<Option<usize>, crate::franken_sync::FrankenError> {
    line_number
        .map(|number| {
            usize::try_from(number).map_err(|_| {
                crate::franken_sync::FrankenError::Internal(format!(
                    "bookmark schema-incompatible: line_number {number} is outside the supported non-negative range"
                ))
            })
        })
        .transpose()
}

fn current_timestamp() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_store() -> (BookmarkStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test_bookmarks.db");
        let store = BookmarkStore::open(&path).unwrap();
        (store, dir)
    }

    fn assert_single_search_path(store: &BookmarkStore, query: &str, expected_path: &str) {
        let results = store.search(query).unwrap();
        let paths = results
            .iter()
            .map(|bookmark| bookmark.source_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![expected_path],
            "query {query:?} should match exactly one source path"
        );
    }

    #[test]
    fn test_create_bookmark() {
        let bookmark = Bookmark::new("Test", "/path/file.rs", "claude_code", "/workspace")
            .with_note("Important finding")
            .with_tags("rust, important")
            .with_line(42);

        assert_eq!(bookmark.title, "Test");
        assert_eq!(bookmark.line_number, Some(42));
        assert!(bookmark.has_tag("rust"));
        assert!(bookmark.has_tag("important"));
        assert!(!bookmark.has_tag("python"));
    }

    #[test]
    fn test_add_and_get() {
        let (store, _dir) = test_store();
        let bookmark = Bookmark::new("Test Result", "/path/to/file.jsonl", "codex", "/my/project")
            .with_note("Found the bug here");

        let id = store.add(&bookmark).unwrap();
        assert!(id > 0);

        let retrieved = store.get(id).unwrap().unwrap();
        assert_eq!(retrieved.title, "Test Result");
        assert_eq!(retrieved.note, "Found the bug here");
    }

    #[test]
    fn test_list_and_count() {
        let (store, _dir) = test_store();

        store
            .add(&Bookmark::new("First", "/a.rs", "claude", "/ws"))
            .unwrap();
        store
            .add(&Bookmark::new("Second", "/b.rs", "codex", "/ws"))
            .unwrap();
        store
            .add(&Bookmark::new("Third", "/c.rs", "claude", "/ws"))
            .unwrap();

        assert_eq!(store.count().unwrap(), 3);
        assert_eq!(store.list(None).unwrap().len(), 3);
    }

    #[test]
    fn test_remove() {
        let (store, _dir) = test_store();
        let id = store
            .add(&Bookmark::new("ToDelete", "/x.rs", "agent", "/ws"))
            .unwrap();

        assert_eq!(store.count().unwrap(), 1);
        assert!(store.remove(id).unwrap());
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn test_tag_filter() {
        let (store, _dir) = test_store();

        store
            .add(&Bookmark::new("A", "/a.rs", "a", "/w").with_tags("rust"))
            .unwrap();
        store
            .add(&Bookmark::new("B", "/b.rs", "b", "/w").with_tags("python"))
            .unwrap();
        store
            .add(&Bookmark::new("C", "/c.rs", "c", "/w").with_tags("rust, important"))
            .unwrap();

        let rust_bookmarks = store.list(Some("rust")).unwrap();
        assert_eq!(rust_bookmarks.len(), 2);
    }

    #[test]
    fn test_search() {
        let (store, _dir) = test_store();

        store
            .add(&Bookmark::new("Bug fix for auth", "/auth.rs", "a", "/w"))
            .unwrap();
        store
            .add(
                &Bookmark::new("Feature", "/feat.rs", "a", "/w")
                    .with_note("authentication related"),
            )
            .unwrap();
        store
            .add(&Bookmark::new("Other", "/other.rs", "a", "/w"))
            .unwrap();

        let results = store.search("auth").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_treats_like_metacharacters_literally() {
        let (store, _dir) = test_store();

        store
            .add(&Bookmark::new(
                "Percent 100% complete",
                "/percent.rs",
                "a",
                "/w",
            ))
            .unwrap();
        store
            .add(&Bookmark::new(
                "Underscore auth_token",
                "/underscore.rs",
                "a",
                "/w",
            ))
            .unwrap();
        store
            .add(&Bookmark::new(
                "Backslash path C:\\tmp",
                "/backslash.rs",
                "a",
                "/w",
            ))
            .unwrap();
        store
            .add(&Bookmark::new("Plain row", "/plain.rs", "a", "/w"))
            .unwrap();

        assert_single_search_path(&store, "%", "/percent.rs");
        assert_single_search_path(&store, "_", "/underscore.rs");
        assert_single_search_path(&store, "\\", "/backslash.rs");
    }

    #[test]
    fn test_is_bookmarked() {
        let (store, _dir) = test_store();

        store
            .add(&Bookmark::new("X", "/file.rs", "a", "/w").with_line(10))
            .unwrap();

        assert!(store.is_bookmarked("/file.rs", Some(10)).unwrap());
        assert!(!store.is_bookmarked("/file.rs", Some(20)).unwrap());
        assert!(!store.is_bookmarked("/other.rs", Some(10)).unwrap());
    }

    #[test]
    fn test_negative_line_number_from_db_reports_schema_incompatibility() {
        let (store, _dir) = test_store();
        let now = current_timestamp();
        store
            .conn
            .execute_compat(
                "INSERT INTO bookmarks (title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    "NegLine",
                    "/neg.rs",
                    -12_i64,
                    "agent",
                    "/ws",
                    "",
                    "",
                    now,
                    now,
                    ""
                ],
            )
            .unwrap();

        let error = store
            .list(None)
            .expect_err("incompatible bookmark rows must not decode through a default");
        assert!(error.to_string().contains("bookmark schema-incompatible"));
        assert!(error.to_string().contains("line_number -12"));
    }

    #[test]
    fn test_add_rejects_line_number_above_i64_max() {
        if usize::BITS <= 63 {
            return;
        }

        let (store, _dir) = test_store();
        let too_large_line = (i64::MAX as usize).saturating_add(1);
        let bookmark =
            Bookmark::new("HugeLine", "/huge.rs", "agent", "/ws").with_line(too_large_line);
        let err = store
            .add(&bookmark)
            .expect_err("line overflow must be rejected");
        assert!(err.to_string().contains("line number exceeds i64 range"));
    }

    #[test]
    fn test_export_import() {
        let (store1, _dir1) = test_store();
        store1
            .add(&Bookmark::new("A", "/a.rs", "agent", "/w").with_tags("tag1"))
            .unwrap();
        store1
            .add(&Bookmark::new("B", "/b.rs", "agent", "/w").with_tags("tag2"))
            .unwrap();

        let json = store1.export_json().unwrap();

        let (store2, _dir2) = test_store();
        let imported = store2.import_json(&json).unwrap();
        assert_eq!(imported, 2);
        assert_eq!(store2.count().unwrap(), 2);
    }

    #[test]
    fn test_import_deduplicates_null_and_specific_line_numbers_separately() {
        let (store, _dir) = test_store();
        let bookmarks = vec![
            Bookmark::new("Whole file", "/same.rs", "agent", "/w"),
            Bookmark::new("Specific line", "/same.rs", "agent", "/w").with_line(10),
        ];
        let json = serde_json::to_string(&bookmarks).unwrap();

        assert_eq!(store.import_json(&json).unwrap(), 2);
        assert_eq!(store.import_json(&json).unwrap(), 0);
        assert_eq!(store.count().unwrap(), 2);
        assert!(store.is_bookmarked("/same.rs", None).unwrap());
        assert!(store.is_bookmarked("/same.rs", Some(10)).unwrap());
    }

    #[test]
    fn test_import_rolls_back_all_rows_when_late_row_is_invalid() -> anyhow::Result<()> {
        if usize::BITS <= 63 {
            return Ok(());
        }

        let (store, _dir) = test_store();
        let bookmarks = vec![
            Bookmark::new("Valid first row", "/valid.rs", "agent", "/w"),
            Bookmark::new("Invalid second row", "/invalid.rs", "agent", "/w")
                .with_line((i64::MAX as usize).saturating_add(1)),
        ];
        let json = serde_json::to_string(&bookmarks)?;

        let Err(error) = store.import_json(&json) else {
            anyhow::bail!("invalid imports must abort the whole transaction");
        };
        anyhow::ensure!(
            error.to_string().contains("line number exceeds i64 range"),
            "unexpected import error: {error}"
        );
        anyhow::ensure!(store.count()? == 0, "the first insert must roll back");
        Ok(())
    }
}
