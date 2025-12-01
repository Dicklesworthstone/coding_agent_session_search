//! Bookmarks system for saving and annotating search results.
//!
//! Provides persistent storage for bookmarked search results with user notes
//! and tags. Uses a separate SQLite database file to avoid schema conflicts.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

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
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Check if bookmark has a specific tag
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tag_list().iter().any(|t| t.eq_ignore_ascii_case(tag))
    }
}

/// Storage backend for bookmarks using SQLite
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

        let conn = Connection::open(path)
            .with_context(|| format!("opening bookmarks db at {}", path.display()))?;

        // Apply pragmas for performance
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;

        // Create schema if needed
        conn.execute_batch(SCHEMA)?;

        Ok(Self { conn })
    }

    /// Open bookmark store at the default location (data_dir/bookmarks.db)
    pub fn open_default() -> Result<Self> {
        let path = default_bookmarks_path();
        Self::open(&path)
    }

    /// Add a new bookmark
    pub fn add(&self, bookmark: &Bookmark) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO bookmarks (title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                bookmark.title,
                bookmark.source_path,
                bookmark.line_number.map(|n| n as i64),
                bookmark.agent,
                bookmark.workspace,
                bookmark.note,
                bookmark.tags,
                bookmark.created_at,
                bookmark.updated_at,
                bookmark.snippet,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Update an existing bookmark
    pub fn update(&self, bookmark: &Bookmark) -> Result<bool> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let rows = self.conn.execute(
            "UPDATE bookmarks SET title = ?1, note = ?2, tags = ?3, updated_at = ?4 WHERE id = ?5",
            params![
                bookmark.title,
                bookmark.note,
                bookmark.tags,
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
            .execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
        Ok(rows > 0)
    }

    /// Get a bookmark by ID
    pub fn get(&self, id: i64) -> Result<Option<Bookmark>> {
        self.conn
            .query_row(
                "SELECT id, title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet
                 FROM bookmarks WHERE id = ?1",
                [id],
                |row| Ok(row_to_bookmark(row)),
            )
            .optional()
            .context("querying bookmark by id")
    }

    /// List all bookmarks, optionally filtered by tag
    pub fn list(&self, tag_filter: Option<&str>) -> Result<Vec<Bookmark>> {
        let mut bookmarks = Vec::new();

        let sql = "SELECT id, title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet
                   FROM bookmarks ORDER BY created_at DESC";

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| Ok(row_to_bookmark(row)))?;

        for bookmark in rows {
            let bookmark = bookmark?;
            if let Some(tag) = tag_filter {
                if bookmark.has_tag(tag) {
                    bookmarks.push(bookmark);
                }
            } else {
                bookmarks.push(bookmark);
            }
        }

        Ok(bookmarks)
    }

    /// Search bookmarks by text (title, note, snippet)
    pub fn search(&self, query: &str) -> Result<Vec<Bookmark>> {
        let pattern = format!("%{}%", query.to_lowercase());

        let mut stmt = self.conn.prepare(
            "SELECT id, title, source_path, line_number, agent, workspace, note, tags, created_at, updated_at, snippet
             FROM bookmarks
             WHERE LOWER(title) LIKE ?1 OR LOWER(note) LIKE ?1 OR LOWER(snippet) LIKE ?1
             ORDER BY created_at DESC",
        )?;

        let rows = stmt.query_map([&pattern], |row| Ok(row_to_bookmark(row)))?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("searching bookmarks")
    }

    /// Get all unique tags
    pub fn all_tags(&self) -> Result<Vec<String>> {
        let bookmarks = self.list(None)?;
        let mut tags: Vec<String> = bookmarks
            .iter()
            .flat_map(|b| b.tag_list())
            .map(|s| s.to_string())
            .collect();

        tags.sort();
        tags.dedup();
        Ok(tags)
    }

    /// Count total bookmarks
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM bookmarks", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Check if a source_path + line is already bookmarked
    pub fn is_bookmarked(&self, source_path: &str, line_number: Option<usize>) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM bookmarks WHERE source_path = ?1 AND line_number IS ?2)",
            params![source_path, line_number.map(|n| n as i64)],
            |row| row.get(0),
        )?;
        Ok(exists)
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

        for mut bookmark in bookmarks {
            // Check for duplicates
            if !self.is_bookmarked(&bookmark.source_path, bookmark.line_number)? {
                bookmark.id = 0; // Reset ID for new insert
                self.add(&bookmark)?;
                imported += 1;
            }
        }

        Ok(imported)
    }
}

/// Convert a database row to a Bookmark
fn row_to_bookmark(row: &rusqlite::Row) -> Bookmark {
    Bookmark {
        id: row.get(0).unwrap_or(0),
        title: row.get(1).unwrap_or_default(),
        source_path: row.get(2).unwrap_or_default(),
        line_number: row
            .get::<_, Option<i64>>(3)
            .ok()
            .flatten()
            .map(|n| n as usize),
        agent: row.get(4).unwrap_or_default(),
        workspace: row.get(5).unwrap_or_default(),
        note: row.get(6).unwrap_or_default(),
        tags: row.get(7).unwrap_or_default(),
        created_at: row.get(8).unwrap_or(0),
        updated_at: row.get(9).unwrap_or(0),
        snippet: row.get(10).unwrap_or_default(),
    }
}

/// Get the default bookmarks database path
pub fn default_bookmarks_path() -> PathBuf {
    directories::ProjectDirs::from("com", "coding-agent-search", "coding-agent-search")
        .map(|dirs| dirs.data_dir().join("bookmarks.db"))
        .unwrap_or_else(|| PathBuf::from("bookmarks.db"))
}

/// SQL schema for bookmarks database
const SCHEMA: &str = r#"
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
"#;

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
}
