use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use anyhow::Result;
use rusqlite::{Connection, Row};
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct OpenCodeConnector;
impl Default for OpenCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeConnector {
    pub fn new() -> Self {
        Self
    }

    fn dir_candidates() -> Vec<PathBuf> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut dirs = vec![cwd.join(".opencode")];

        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".opencode"));
        }

        if let Some(data) = dirs::data_dir() {
            dirs.push(data.join("opencode"));
            dirs.push(data.join("opencode/project"));
        }

        dirs
    }

    fn find_dbs() -> Vec<PathBuf> {
        let mut out = Vec::new();
        for root in Self::dir_candidates() {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(root).into_iter().flatten() {
                if entry.file_type().is_file() {
                    let path = entry.path();
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.ends_with(".db")
                        || name.ends_with(".sqlite")
                        || name.eq_ignore_ascii_case("database")
                        || name.eq_ignore_ascii_case("storage")
                    {
                        out.push(path.to_path_buf());
                    }
                }
            }
        }
        out
    }
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        for d in Self::dir_candidates() {
            if d.exists() {
                return DetectionResult {
                    detected: true,
                    evidence: vec![format!("found {}", d.display())],
                };
            }
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // Use ctx.data_root for tests/custom paths, but filter out CASS internal databases
        let dbs = if ctx.data_root.exists() {
            WalkDir::new(&ctx.data_root)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| e.path().to_path_buf())
                .filter(|p| {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // Exclude CASS's own internal databases to prevent self-indexing
                    if name == "agent_search.db" || name == "conversations.db" {
                        return false;
                    }
                    name.ends_with(".db") || name.ends_with(".sqlite")
                })
                .collect()
        } else {
            Self::find_dbs()
        };

        for db_path in dbs {
            let conn = match Connection::open(&db_path) {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!("opencode: failed to open {}: {err}", db_path.display());
                    continue;
                }
            };

            match load_db(&conn, &db_path, ctx.since_ts, &mut seen_ids) {
                Ok(mut found) => convs.append(&mut found),
                Err(err) => tracing::warn!("opencode: failed to read {}: {err}", db_path.display()),
            }
        }

        Ok(convs)
    }
}

fn load_db(
    conn: &Connection,
    db_path: &PathBuf,
    since_ts: Option<i64>,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<Vec<NormalizedConversation>> {
    let sessions_present = has_table(conn, "sessions")?;
    let messages_present = has_table(conn, "messages")?;

    if !messages_present {
        return Ok(Vec::new());
    }

    // Build session metadata map if available.
    let session_meta: HashMap<i64, SessionRow> = if sessions_present {
        read_sessions(conn)?
    } else {
        HashMap::new()
    };

    let mut by_session: HashMap<i64, Vec<NormalizedMessage>> = HashMap::new();
    let mut fallback_messages: Vec<NormalizedMessage> = Vec::new();

    let msg_cols = table_columns(conn, "messages")?;
    let order_col = msg_cols
        .iter()
        .find(|c| c.as_str() == "created_at" || c.as_str() == "timestamp" || c.as_str() == "ts")
        .cloned();
    let sql = match order_col {
        Some(col) => format!("SELECT * FROM messages ORDER BY {col}"),
        None => "SELECT * FROM messages".to_string(),
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| message_from_row(row, &msg_cols))?;
    for msg in rows {
        let msg = msg?;
        if let (Some(since), Some(ts)) = (since_ts, msg.created_at)
            && ts <= since
        {
            continue;
        }
        if let Some(id) = msg.extra.get("session_id").and_then(|v| v.as_i64()) {
            by_session.entry(id).or_default().push(msg);
        } else if let Some(id) = msg.extra.get("task_id").and_then(|v| v.as_i64()) {
            by_session.entry(id).or_default().push(msg);
        } else {
            fallback_messages.push(msg);
        }
    }

    let mut convs = Vec::new();

    // Calculate stable hash of db path to disambiguate session IDs between projects
    let mut hasher = DefaultHasher::new();
    db_path.hash(&mut hasher);
    let db_hash = hasher.finish();

    for (session_id, mut messages) in by_session {
        if messages.is_empty() {
            continue;
        }
        messages.sort_by_key(|m| m.created_at.unwrap_or(i64::MAX));

        // Assign stable indices based on full history order before filtering
        for (i, msg) in messages.iter_mut().enumerate() {
            msg.idx = i as i64;
        }

        if let Some(since) = since_ts {
            messages.retain(|m| m.created_at.is_some_and(|ts| ts > since));
            if messages.is_empty() {
                continue;
            }
        }

        let meta = session_meta.get(&session_id);
        let title = meta.and_then(|m| m.title.clone()).or_else(|| {
            messages
                .first()
                .and_then(|m| m.content.lines().next())
                .map(|s| s.to_string())
        });
        let started_at = meta
            .and_then(|m| m.started_at)
            .or_else(|| messages.first().and_then(|m| m.created_at));
        let ended_at = messages.last().and_then(|m| m.created_at);

        convs.push(NormalizedConversation {
            agent_slug: "opencode".into(),
            external_id: Some(format!("session-{session_id}-{:x}", db_hash)),
            title,
            workspace: meta.and_then(|m| m.workspace.clone()),
            source_path: db_path.clone(),
            started_at,
            ended_at,
            metadata: serde_json::json!({
                "db_path": db_path,
                "session_id": session_id,
            }),
            messages,
        });
    }

    if !fallback_messages.is_empty() {
        for (i, msg) in fallback_messages.iter_mut().enumerate() {
            msg.idx = i as i64;
        }
        convs.push(NormalizedConversation {
            agent_slug: "opencode".into(),
            external_id: Some(format!("db:{}", db_path.display())),
            title: fallback_messages
                .first()
                .and_then(|m| m.content.lines().next())
                .map(|s| s.to_string()),
            workspace: None,
            source_path: db_path.clone(),
            started_at: fallback_messages.first().and_then(|m| m.created_at),
            ended_at: fallback_messages.last().and_then(|m| m.created_at),
            metadata: serde_json::json!({"db_path": db_path}),
            messages: fallback_messages,
        });
    }

    // Apply since_ts post-filter to ensure late-binding still respects high-water mark.
    if let Some(since) = since_ts {
        let mut filtered = Vec::new();
        for mut conv in convs {
            let mut msgs: Vec<_> = conv
                .messages
                .into_iter()
                .filter(|m| m.created_at.is_some_and(|ts| ts > since))
                .collect();
            if msgs.is_empty() {
                continue;
            }
            for (i, m) in msgs.iter_mut().enumerate() {
                m.idx = i as i64;
            }
            conv.messages = msgs;
            conv.started_at = conv.messages.first().and_then(|m| m.created_at);
            conv.ended_at = conv.messages.last().and_then(|m| m.created_at);
            filtered.push(conv);
        }
        convs = filtered;
    }

    // Deduplicate external IDs in case multiple DBs share identifiers.
    let mut unique = Vec::new();
    for conv in convs {
        if let Some(ext) = &conv.external_id {
            let key = format!("opencode:{ext}");
            if seen_ids.insert(key) {
                unique.push(conv);
            }
        } else {
            unique.push(conv);
        }
    }

    Ok(unique)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name = ?1 LIMIT 1")?;
    Ok(stmt.exists([name])?)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?; // 1 = name
    let mut cols = Vec::new();
    for c in rows {
        cols.push(c?);
    }
    Ok(cols)
}

#[derive(Debug, Clone)]
struct SessionRow {
    id: i64,
    title: Option<String>,
    workspace: Option<PathBuf>,
    started_at: Option<i64>,
}

fn read_sessions(conn: &Connection) -> Result<HashMap<i64, SessionRow>> {
    let cols = table_columns(conn, "sessions")?;
    let mut stmt = conn.prepare("SELECT * FROM sessions")?;
    let rows = stmt.query_map([], |row| session_from_row(row, &cols))?;
    let mut map = HashMap::new();
    for r in rows {
        let r = r?;
        map.insert(r.id, r);
    }
    Ok(map)
}

fn session_from_row(row: &Row<'_>, cols: &[String]) -> rusqlite::Result<SessionRow> {
    let id = get_opt_i64(row, cols, "id")?.unwrap_or_else(|| row.get::<_, i64>(0).unwrap_or(0));

    let mut title = get_opt_string(row, cols, "title")?;
    if title.is_none() {
        title = get_opt_string(row, cols, "name")?;
    }

    let mut workspace = get_opt_string(row, cols, "workspace")?;
    if workspace.is_none() {
        workspace = get_opt_string(row, cols, "root_path")?;
    }
    let workspace = workspace.map(PathBuf::from);

    let mut started_at = get_opt_i64(row, cols, "created_at")?;
    if started_at.is_none() {
        started_at = get_opt_i64(row, cols, "started_at")?;
    }
    if started_at.is_none() {
        started_at = get_opt_i64(row, cols, "timestamp")?;
    }

    Ok(SessionRow {
        id,
        title,
        workspace,
        started_at,
    })
}

fn message_from_row(row: &Row<'_>, cols: &[String]) -> rusqlite::Result<NormalizedMessage> {
    let mut role = get_opt_string(row, cols, "role")?;
    if role.is_none() {
        role = get_opt_string(row, cols, "sender")?;
    }
    let role = role.unwrap_or_else(|| "agent".to_string());

    let mut author = get_opt_string(row, cols, "author")?;
    if author.is_none() {
        author = get_opt_string(row, cols, "sender")?;
    }

    let mut created_at = get_opt_i64(row, cols, "created_at")?;
    if created_at.is_none() {
        created_at = get_opt_i64(row, cols, "timestamp")?;
    }
    if created_at.is_none() {
        created_at = get_opt_i64(row, cols, "ts")?;
    }

    let mut content = get_opt_string(row, cols, "content")?;
    if content.is_none() {
        content = get_opt_string(row, cols, "text")?;
    }
    if content.is_none() {
        content = get_opt_string(row, cols, "message")?;
    }
    let content = content.unwrap_or_default();

    // Capture the entire row as best-effort metadata for debugging.
    let mut extra = serde_json::Map::new();
    for (idx, c) in cols.iter().enumerate() {
        if let Ok(val) = row.get::<_, rusqlite::types::Value>(idx) {
            extra.insert(c.clone(), sqlite_value_to_json(val));
        }
    }

    Ok(NormalizedMessage {
        idx: 0,
        role,
        author,
        created_at,
        content,
        extra: serde_json::Value::Object(extra),
        snippets: Vec::new(),
    })
}

fn sqlite_value_to_json(v: rusqlite::types::Value) -> serde_json::Value {
    use base64::Engine;
    use rusqlite::types::Value as V;
    match v {
        V::Null => serde_json::Value::Null,
        V::Integer(i) => serde_json::Value::from(i),
        V::Real(f) => serde_json::Value::from(f),
        V::Text(t) => serde_json::Value::from(t),
        V::Blob(b) => serde_json::Value::from(base64::engine::general_purpose::STANDARD.encode(b)),
    }
}

fn get_opt_string(row: &Row<'_>, cols: &[String], name: &str) -> rusqlite::Result<Option<String>> {
    if let Some(idx) = cols.iter().position(|c| c == name) {
        return row
            .get::<_, Option<String>>(idx)
            .map(|s| s.map(|v| v.to_string()));
    }
    Ok(None)
}

fn get_opt_i64(row: &Row<'_>, cols: &[String], name: &str) -> rusqlite::Result<Option<i64>> {
    if let Some(idx) = cols.iter().position(|c| c == name) {
        return row.get::<_, Option<i64>>(idx);
    }
    Ok(None)
}
