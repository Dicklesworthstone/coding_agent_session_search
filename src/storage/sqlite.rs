//! `SQLite` backend: schema, pragmas, and migrations.

use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
use crate::sources::provenance::{LOCAL_SOURCE_ID, Source, SourceKind};
use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 4;

const MIGRATION_V1: &str = r"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    version TEXT,
    kind TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS workspaces (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    display_name TEXT
);

CREATE TABLE IF NOT EXISTS conversations (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    workspace_id INTEGER REFERENCES workspaces(id),
    external_id TEXT,
    title TEXT,
    source_path TEXT NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    approx_tokens INTEGER,
    metadata_json TEXT,
    UNIQUE(agent_id, external_id)
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    role TEXT NOT NULL,
    author TEXT,
    created_at INTEGER,
    content TEXT NOT NULL,
    extra_json TEXT,
    UNIQUE(conversation_id, idx)
);

CREATE TABLE IF NOT EXISTS snippets (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    file_path TEXT,
    start_line INTEGER,
    end_line INTEGER,
    language TEXT,
    snippet_text TEXT
);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS conversation_tags (
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (conversation_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_conversations_agent_started
    ON conversations(agent_id, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_messages_conv_idx
    ON messages(conversation_id, idx);

CREATE INDEX IF NOT EXISTS idx_messages_created
    ON messages(created_at);
";

const MIGRATION_V2: &str = r"
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
SELECT
    m.content,
    c.title,
    a.slug,
    w.path,
    c.source_path,
    m.created_at,
    m.id
FROM messages m
JOIN conversations c ON m.conversation_id = c.id
JOIN agents a ON c.agent_id = a.id
LEFT JOIN workspaces w ON c.workspace_id = w.id;
";

const MIGRATION_V3: &str = r"
DROP TABLE IF EXISTS fts_messages;
CREATE VIRTUAL TABLE fts_messages USING fts5(
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
SELECT
    m.content,
    c.title,
    a.slug,
    w.path,
    c.source_path,
    m.created_at,
    m.id
FROM messages m
JOIN conversations c ON m.conversation_id = c.id
JOIN agents a ON c.agent_id = a.id
LEFT JOIN workspaces w ON c.workspace_id = w.id;
";

const MIGRATION_V4: &str = r"
-- Sources table for tracking where conversations come from
CREATE TABLE IF NOT EXISTS sources (
    id TEXT PRIMARY KEY,           -- source_id (e.g., 'local', 'work-laptop')
    kind TEXT NOT NULL,            -- 'local', 'ssh', etc.
    host_label TEXT,               -- display label
    machine_id TEXT,               -- optional stable machine id
    platform TEXT,                 -- 'macos', 'linux', 'windows'
    config_json TEXT,              -- JSON blob for extra config (SSH params, path rewrites)
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Bootstrap: Insert the default 'local' source
INSERT OR IGNORE INTO sources (id, kind, host_label, created_at, updated_at)
VALUES ('local', 'local', NULL, strftime('%s','now')*1000, strftime('%s','now')*1000);
";

pub struct SqliteStorage {
    conn: Connection,
}

pub struct InsertOutcome {
    pub conversation_id: i64,
    pub inserted_indices: Vec<i64>,
}

impl SqliteStorage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let mut conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;

        apply_pragmas(&mut conn)?;
        init_meta(&mut conn)?;
        migrate(&mut conn)?;

        Ok(Self { conn })
    }

    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening sqlite db readonly at {}", path.display()))?;

        apply_common_pragmas(&conn)?;

        Ok(Self { conn })
    }

    pub fn raw(&self) -> &Connection {
        &self.conn
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0).map(|s| s.parse().unwrap_or(0)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("schema_version missing"))
    }

    pub fn ensure_agent(&self, agent: &Agent) -> Result<i64> {
        let now = Self::now_millis();
        self.conn.execute(
            "INSERT INTO agents(slug, name, version, kind, created_at, updated_at) VALUES(?,?,?,?,?,?)
             ON CONFLICT(slug) DO UPDATE SET name=excluded.name, version=excluded.version, kind=excluded.kind, updated_at=excluded.updated_at",
            params![
                &agent.slug,
                &agent.name,
                &agent.version,
                agent_kind_str(agent.kind.clone()),
                now,
                now
            ],
        )?;

        self.conn
            .query_row(
                "SELECT id FROM agents WHERE slug = ?",
                params![&agent.slug],
                |row| row.get(0),
            )
            .with_context(|| format!("fetching agent id for {}", agent.slug))
    }

    pub fn ensure_workspace(&self, path: &Path, display_name: Option<&str>) -> Result<i64> {
        let path_str = path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO workspaces(path, display_name) VALUES(?,?)
             ON CONFLICT(path) DO UPDATE SET display_name=COALESCE(excluded.display_name, workspaces.display_name)",
            params![path_str, display_name],
        )?;

        self.conn
            .query_row(
                "SELECT id FROM workspaces WHERE path = ?",
                params![path_str],
                |row| row.get(0),
            )
            .with_context(|| format!("fetching workspace id for {path_str}"))
    }

    pub fn insert_conversation_tree(
        &mut self,
        agent_id: i64,
        workspace_id: Option<i64>,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        if let Some(ext) = &conv.external_id
            && let Some(existing) = self
                .conn
                .query_row(
                    "SELECT id FROM conversations WHERE agent_id = ? AND external_id = ?",
                    params![agent_id, ext],
                    |row| row.get(0),
                )
                .optional()?
        {
            return self.append_messages(existing, conv);
        }

        let tx = self.conn.transaction()?;

        let conv_id = insert_conversation(&tx, agent_id, workspace_id, conv)?;
        for msg in &conv.messages {
            let msg_id = insert_message(&tx, conv_id, msg)?;
            insert_snippets(&tx, msg_id, &msg.snippets)?;
            insert_fts_message(&tx, msg_id, msg, conv)?;
        }
        tx.commit()?;
        Ok(InsertOutcome {
            conversation_id: conv_id,
            inserted_indices: conv.messages.iter().map(|m| m.idx).collect(),
        })
    }

    fn append_messages(
        &mut self,
        conversation_id: i64,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        let tx = self.conn.transaction()?;

        let max_idx: Option<i64> = tx.query_row(
            "SELECT MAX(idx) FROM messages WHERE conversation_id = ?",
            params![conversation_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let cutoff = max_idx.unwrap_or(-1);

        let mut inserted_indices = Vec::new();
        for msg in &conv.messages {
            if msg.idx <= cutoff {
                continue;
            }
            let msg_id = insert_message(&tx, conversation_id, msg)?;
            insert_snippets(&tx, msg_id, &msg.snippets)?;
            insert_fts_message(&tx, msg_id, msg, conv)?;
            inserted_indices.push(msg.idx);
        }

        if let Some(last_ts) = conv.messages.iter().filter_map(|m| m.created_at).max() {
            // Use IFNULL to handle NULL ended_at values correctly.
            // SQLite's scalar MAX(NULL, x) returns NULL, so we need to coalesce first.
            tx.execute(
                "UPDATE conversations SET ended_at = MAX(IFNULL(ended_at, 0), ?) WHERE id = ?",
                params![last_ts, conversation_id],
            )?;
        }

        tx.commit()?;
        Ok(InsertOutcome {
            conversation_id,
            inserted_indices,
        })
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, name, version, kind FROM agents ORDER BY slug")?;
        let rows = stmt.query_map([], |row| {
            let kind: String = row.get(4)?;
            Ok(Agent {
                id: Some(row.get(0)?),
                slug: row.get(1)?,
                name: row.get(2)?,
                version: row.get(3)?,
                kind: match kind.as_str() {
                    "cli" => AgentKind::Cli,
                    "vscode" => AgentKind::VsCode,
                    _ => AgentKind::Hybrid,
                },
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_workspaces(&self) -> Result<Vec<crate::model::types::Workspace>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, display_name FROM workspaces ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::model::types::Workspace {
                id: Some(row.get(0)?),
                path: Path::new(&row.get::<_, String>(1)?).to_path_buf(),
                display_name: row.get::<_, Option<String>>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_conversations(&self, limit: i64, offset: i64) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            r"SELECT c.id, a.slug, w.path, c.external_id, c.title, c.source_path,
                       c.started_at, c.ended_at, c.approx_tokens, c.metadata_json
                FROM conversations c
                JOIN agents a ON c.agent_id = a.id
                LEFT JOIN workspaces w ON c.workspace_id = w.id
                ORDER BY c.started_at IS NULL, c.started_at DESC, c.id DESC
                LIMIT ? OFFSET ?",
        )?;

        let rows = stmt.query_map(params![limit, offset], |row| {
            Ok(Conversation {
                id: Some(row.get(0)?),
                agent_slug: row.get(1)?,
                workspace: row
                    .get::<_, Option<String>>(2)?
                    .map(|p| Path::new(&p).to_path_buf()),
                external_id: row.get(3)?,
                title: row.get(4)?,
                source_path: Path::new(&row.get::<_, String>(5)?).to_path_buf(),
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                approx_tokens: row.get(8)?,
                metadata_json: row
                    .get::<_, Option<String>>(9)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
                messages: Vec::new(),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn fetch_messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, idx, role, author, created_at, content, extra_json FROM messages WHERE conversation_id = ? ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            let role: String = row.get(2)?;
            Ok(Message {
                id: Some(row.get(0)?),
                idx: row.get(1)?,
                role: match role.as_str() {
                    "user" => MessageRole::User,
                    "agent" | "assistant" => MessageRole::Agent,
                    "tool" => MessageRole::Tool,
                    "system" => MessageRole::System,
                    other => MessageRole::Other(other.to_string()),
                },
                author: row.get::<_, Option<String>>(3)?,
                created_at: row.get::<_, Option<i64>>(4)?,
                content: row.get(5)?,
                extra_json: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
                snippets: Vec::new(),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn rebuild_fts(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM fts_messages", [])?;
        self.conn.execute_batch(
            r"INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
               SELECT m.content, c.title, a.slug, w.path, c.source_path, m.created_at, m.id
               FROM messages m
               JOIN conversations c ON m.conversation_id = c.id
               JOIN agents a ON c.agent_id = a.id
               LEFT JOIN workspaces w ON c.workspace_id = w.id;",
        )?;
        Ok(())
    }

    /// Get the timestamp of the last successful scan (milliseconds since epoch).
    /// Returns None if no scan has been recorded yet.
    pub fn get_last_scan_ts(&self) -> Result<Option<i64>> {
        let ts: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_scan_ts'",
                [],
                |row| {
                    let s: String = row.get(0)?;
                    Ok(s.parse().ok())
                },
            )
            .optional()?
            .flatten();
        Ok(ts)
    }

    /// Set the timestamp of the last successful scan (milliseconds since epoch).
    pub fn set_last_scan_ts(&mut self, ts: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_scan_ts', ?)",
            params![ts.to_string()],
        )?;
        Ok(())
    }

    /// Get current time as milliseconds since epoch.
    pub fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // -------------------------------------------------------------------------
    // Source CRUD operations
    // -------------------------------------------------------------------------

    /// Get a source by ID.
    pub fn get_source(&self, id: &str) -> Result<Option<Source>> {
        self.conn
            .query_row(
                "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at
                 FROM sources WHERE id = ?",
                params![id],
                |row| {
                    let kind_str: String = row.get(1)?;
                    let config_json_str: Option<String> = row.get(5)?;
                    Ok(Source {
                        id: row.get(0)?,
                        kind: SourceKind::parse(&kind_str).unwrap_or_default(),
                        host_label: row.get(2)?,
                        machine_id: row.get(3)?,
                        platform: row.get(4)?,
                        config_json: config_json_str
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .with_context(|| format!("fetching source with id '{id}'"))
    }

    /// List all sources.
    pub fn list_sources(&self) -> Result<Vec<Source>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at
             FROM sources ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(1)?;
            let config_json_str: Option<String> = row.get(5)?;
            Ok(Source {
                id: row.get(0)?,
                kind: SourceKind::parse(&kind_str).unwrap_or_default(),
                host_label: row.get(2)?,
                machine_id: row.get(3)?,
                platform: row.get(4)?,
                config_json: config_json_str.and_then(|s| serde_json::from_str(&s).ok()),
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Create or update a source.
    pub fn upsert_source(&self, source: &Source) -> Result<()> {
        let now = Self::now_millis();
        let config_json_str = source
            .config_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            "INSERT INTO sources(id, kind, host_label, machine_id, platform, config_json, created_at, updated_at)
             VALUES(?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                host_label = excluded.host_label,
                machine_id = excluded.machine_id,
                platform = excluded.platform,
                config_json = excluded.config_json,
                updated_at = excluded.updated_at",
            params![
                source.id,
                source.kind.as_str(),
                source.host_label,
                source.machine_id,
                source.platform,
                config_json_str,
                source.created_at.unwrap_or(now),
                now
            ],
        )?;
        Ok(())
    }

    /// Delete a source by ID.
    ///
    /// If `cascade` is true, also deletes all conversations from this source.
    /// Note: Currently conversations don't have a source_id column, so cascade
    /// is a no-op until P1.3 is implemented.
    pub fn delete_source(&self, id: &str, _cascade: bool) -> Result<bool> {
        // Prevent deletion of the local source
        if id == LOCAL_SOURCE_ID {
            return Err(anyhow!("cannot delete the local source"));
        }

        let rows_affected = self
            .conn
            .execute("DELETE FROM sources WHERE id = ?", params![id])?;

        Ok(rows_affected > 0)
    }
}

fn apply_pragmas(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        r"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        ",
    )?;
    apply_common_pragmas(conn)
}

fn apply_common_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r"
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -65536; -- 64MB
        PRAGMA mmap_size = 268435456; -- 256MB
        PRAGMA foreign_keys = ON;
        ",
    )?;
    Ok(())
}

fn init_meta(conn: &mut Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )?;

    let existing: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0).map(|s| s.parse().unwrap_or(0)),
        )
        .optional()?;

    if existing.is_none() {
        // Start at version 0 so migrate() applies full schema on first open.
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', 0)",
            [],
        )?;
    }

    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0).map(|s| s.parse().unwrap_or(0)),
        )
        .optional()?
        .unwrap_or(0);

    if current == SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn.transaction()?;

    match current {
        0 => {
            tx.execute_batch(MIGRATION_V1)?;
            tx.execute_batch(MIGRATION_V2)?;
            tx.execute_batch(MIGRATION_V3)?;
            tx.execute_batch(MIGRATION_V4)?;
        }
        1 => {
            tx.execute_batch(MIGRATION_V2)?;
            tx.execute_batch(MIGRATION_V3)?;
            tx.execute_batch(MIGRATION_V4)?;
        }
        2 => {
            tx.execute_batch(MIGRATION_V3)?;
            tx.execute_batch(MIGRATION_V4)?;
        }
        3 => {
            tx.execute_batch(MIGRATION_V4)?;
        }
        v => return Err(anyhow!("unsupported schema version {v}")),
    }

    tx.execute(
        "UPDATE meta SET value = ? WHERE key = 'schema_version'",
        params![SCHEMA_VERSION.to_string()],
    )?;

    tx.commit()?;
    Ok(())
}

fn insert_conversation(
    tx: &Transaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO conversations(
            agent_id, workspace_id, external_id, title, source_path, started_at, ended_at, approx_tokens, metadata_json
        ) VALUES(?,?,?,?,?,?,?,?,?)",
        params![
            agent_id,
            workspace_id,
            conv.external_id,
            conv.title,
            path_to_string(&conv.source_path),
            conv.started_at,
            conv.ended_at,
            conv.approx_tokens,
            serde_json::to_string(&conv.metadata_json)?
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn insert_message(tx: &Transaction<'_>, conversation_id: i64, msg: &Message) -> Result<i64> {
    tx.execute(
        "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json)
         VALUES(?,?,?,?,?,?,?)",
        params![
            conversation_id,
            msg.idx,
            role_str(&msg.role),
            msg.author,
            msg.created_at,
            msg.content,
            serde_json::to_string(&msg.extra_json)?
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn insert_snippets(tx: &Transaction<'_>, message_id: i64, snippets: &[Snippet]) -> Result<()> {
    for snip in snippets {
        tx.execute(
            "INSERT INTO snippets(message_id, file_path, start_line, end_line, language, snippet_text)
             VALUES(?,?,?,?,?,?)",
            params![
                message_id,
                snip.file_path.as_ref().map(path_to_string),
                snip.start_line,
                snip.end_line,
                snip.language,
                snip.snippet_text,
            ],
        )?;
    }
    Ok(())
}

fn insert_fts_message(
    tx: &Transaction<'_>,
    message_id: i64,
    msg: &Message,
    conv: &Conversation,
) -> Result<()> {
    let _ = tx.execute(
        "INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
         VALUES(?,?,?,?,?,?,?)",
        params![
            msg.content,
            conv.title.clone().unwrap_or_default(),
            conv.agent_slug.clone(),
            conv.workspace
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path_to_string(&conv.source_path),
            msg.created_at.or(conv.started_at),
            message_id
        ],
    );
    // FTS mirror is best-effort; skip errors (Tantivy remains source of truth).
    Ok(())
}

fn path_to_string<P: AsRef<Path>>(p: P) -> String {
    p.as_ref().to_string_lossy().into_owned()
}

fn role_str(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".to_owned(),
        MessageRole::Agent => "agent".to_owned(),
        MessageRole::Tool => "tool".to_owned(),
        MessageRole::System => "system".to_owned(),
        MessageRole::Other(v) => v.clone(),
    }
}

fn agent_kind_str(kind: AgentKind) -> String {
    match kind {
        AgentKind::Cli => "cli".into(),
        AgentKind::VsCode => "vscode".into(),
        AgentKind::Hybrid => "hybrid".into(),
    }
}
