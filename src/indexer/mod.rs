use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{RecursiveMode, Watcher, recommended_watcher};

use crate::connectors::NormalizedConversation;
use crate::connectors::{
    Connector, amp::AmpConnector, claude_code::ClaudeCodeConnector, cline::ClineConnector,
    codex::CodexConnector, gemini::GeminiConnector, opencode::OpenCodeConnector,
};
use crate::search::tantivy::{TantivyIndex, index_dir};
use crate::storage::sqlite::SqliteStorage;

#[derive(Clone)]
pub struct IndexOptions {
    pub full: bool,
    pub watch: bool,
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
}

pub fn run_index(opts: IndexOptions) -> Result<()> {
    let mut storage = SqliteStorage::open(&opts.db_path)?;
    let mut t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir)?)?;

    if opts.full {
        reset_storage(&mut storage)?;
        t_index.delete_all()?;
    }

    let connectors: Vec<Box<dyn Connector>> = vec![
        Box::new(CodexConnector::new()),
        Box::new(ClineConnector::new()),
        Box::new(GeminiConnector::new()),
        Box::new(ClaudeCodeConnector::new()),
        Box::new(OpenCodeConnector::new()),
        Box::new(AmpConnector::new()),
    ];

    for conn in connectors {
        let detect = conn.detect();
        if !detect.detected {
            continue;
        }
        let ctx = crate::connectors::ScanContext {
            data_root: opts.data_dir.clone(),
            since_ts: None,
        };
        let convs = conn.scan(&ctx)?;
        ingest_batch(&mut storage, &mut t_index, convs)?;
    }

    t_index.commit()?;

    if opts.watch {
        let opts_clone = opts.clone();
        let state = Arc::new(Mutex::new(load_watch_state(&opts.data_dir)));
        watch_sources(move |paths| {
            let _ = reindex_paths(&opts_clone, paths, state.clone());
        })?;
    }

    Ok(())
}

fn ingest_batch(
    storage: &mut SqliteStorage,
    t_index: &mut TantivyIndex,
    convs: Vec<NormalizedConversation>,
) -> Result<()> {
    for conv in convs {
        persist::persist_conversation(storage, t_index, &conv)?;
    }
    Ok(())
}

fn watch_sources<F: Fn(Vec<PathBuf>) + Send + 'static>(callback: F) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event.paths);
        }
    })?;

    for dir in watch_roots() {
        let _ = watcher.watch(&dir, RecursiveMode::Recursive);
    }

    let debounce = Duration::from_secs(2);
    let mut last = Instant::now();
    loop {
        if let Ok(paths) = rx.recv()
            && last.elapsed() >= debounce
        {
            callback(paths);
            last = Instant::now();
        }
    }
}

fn watch_roots() -> Vec<PathBuf> {
    vec![
        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".codex/sessions")),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config/Code/User/globalStorage/saoudrizwan.claude-dev"),
        dirs::home_dir().unwrap_or_default().join(".gemini/tmp"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".claude/projects"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config/Code/User/globalStorage/sourcegraph.amp"),
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("amp"),
        std::env::current_dir()
            .unwrap_or_default()
            .join(".opencode"),
        dirs::home_dir().unwrap_or_default().join(".opencode"),
    ]
}

fn reset_storage(storage: &mut SqliteStorage) -> Result<()> {
    storage.raw().execute_batch(
        r#"
        DELETE FROM fts_messages;
        DELETE FROM snippets;
        DELETE FROM messages;
        DELETE FROM conversations;
        DELETE FROM agents;
        DELETE FROM workspaces;
        DELETE FROM tags;
        DELETE FROM conversation_tags;
    "#,
    )?;
    Ok(())
}

fn reindex_paths(
    opts: &IndexOptions,
    paths: Vec<PathBuf>,
    state: Arc<Mutex<HashMap<ConnectorKind, i64>>>,
) -> Result<()> {
    let mut storage = SqliteStorage::open(&opts.db_path)?;
    let mut t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir)?)?;

    let triggers = classify_paths(paths);
    if triggers.is_empty() {
        return Ok(());
    }

    for (kind, ts) in triggers {
        let conn: Box<dyn Connector> = match kind {
            ConnectorKind::Codex => Box::new(CodexConnector::new()),
            ConnectorKind::Cline => Box::new(ClineConnector::new()),
            ConnectorKind::Gemini => Box::new(GeminiConnector::new()),
            ConnectorKind::Claude => Box::new(ClaudeCodeConnector::new()),
            ConnectorKind::Amp => Box::new(AmpConnector::new()),
            ConnectorKind::OpenCode => Box::new(OpenCodeConnector::new()),
        };
        let detect = conn.detect();
        if !detect.detected {
            continue;
        }
        let since_ts = {
            let guard = state.lock().unwrap();
            guard
                .get(&kind)
                .cloned()
                .or_else(|| ts.map(|v| v.saturating_sub(1)))
        };
        let ctx = crate::connectors::ScanContext {
            data_root: opts.data_dir.clone(),
            since_ts,
        };
        let convs = conn.scan(&ctx)?;
        ingest_batch(&mut storage, &mut t_index, convs)?;
        if let Some(ts_val) = ts {
            let mut guard = state.lock().unwrap();
            let entry = guard.entry(kind).or_insert(ts_val);
            *entry = (*entry).max(ts_val);
            save_watch_state(&opts.data_dir, &guard)?;
        }
    }
    t_index.commit()?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConnectorKind {
    Codex,
    Cline,
    Gemini,
    Claude,
    Amp,
    OpenCode,
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("watch_state.json")
}

fn load_watch_state(data_dir: &Path) -> HashMap<ConnectorKind, i64> {
    let path = state_path(data_dir);
    if let Ok(bytes) = fs::read(&path)
        && let Ok(map) = serde_json::from_slice(&bytes)
    {
        return map;
    }
    HashMap::new()
}

fn save_watch_state(data_dir: &Path, state: &HashMap<ConnectorKind, i64>) -> Result<()> {
    let path = state_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state)?;
    fs::write(path, json)?;
    Ok(())
}

fn classify_paths(paths: Vec<PathBuf>) -> Vec<(ConnectorKind, Option<i64>)> {
    let mut map: HashMap<ConnectorKind, Option<i64>> = HashMap::new();
    for p in paths {
        if let Ok(meta) = std::fs::metadata(&p)
            && let Ok(time) = meta.modified()
            && let Ok(dur) = time.duration_since(std::time::UNIX_EPOCH)
        {
            let ts = Some(dur.as_millis() as i64);
            let s = p.to_string_lossy();
            let tag =
                if s.contains(".codex") || s.contains("codex/sessions") || s.contains("rollout-") {
                    Some(ConnectorKind::Codex)
                } else if s.contains("saoudrizwan.claude-dev") || s.contains("cline") {
                    Some(ConnectorKind::Cline)
                } else if s.contains(".gemini/tmp") {
                    Some(ConnectorKind::Gemini)
                } else if s.contains(".claude/projects")
                    || s.ends_with(".claude")
                    || s.ends_with(".claude.json")
                {
                    Some(ConnectorKind::Claude)
                } else if s.contains("sourcegraph.amp") || s.contains("/amp/") {
                    Some(ConnectorKind::Amp)
                } else if s.contains(".opencode") || s.contains("/opencode/") {
                    Some(ConnectorKind::OpenCode)
                } else {
                    None
                };

            if let Some(kind) = tag {
                let entry = map.entry(kind).or_insert(None);
                *entry = match (*entry, ts) {
                    (Some(prev), Some(cur)) => Some(prev.max(cur)),
                    (None, Some(cur)) => Some(cur),
                    _ => *entry,
                };
            }
        }
    }
    map.into_iter().collect()
}

pub mod persist {
    use anyhow::Result;

    use crate::connectors::NormalizedConversation;
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use crate::search::tantivy::TantivyIndex;
    use crate::storage::sqlite::{InsertOutcome, SqliteStorage};

    pub fn persist_conversation(
        storage: &mut SqliteStorage,
        t_index: &mut TantivyIndex,
        conv: &NormalizedConversation,
    ) -> Result<()> {
        let agent = Agent {
            id: None,
            slug: conv.agent_slug.clone(),
            name: conv.agent_slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent)?;

        let workspace_id = if let Some(ws) = &conv.workspace {
            Some(storage.ensure_workspace(ws, None)?)
        } else {
            None
        };

        let messages: Vec<Message> = conv
            .messages
            .iter()
            .map(|m| Message {
                id: None,
                idx: m.idx,
                role: map_role(&m.role),
                author: m.author.clone(),
                created_at: m.created_at,
                content: m.content.clone(),
                extra_json: m.extra.clone(),
                snippets: Vec::new(),
            })
            .collect();

        let conversation = Conversation {
            id: None,
            agent_slug: conv.agent_slug.clone(),
            workspace: conv.workspace.clone(),
            external_id: conv.external_id.clone(),
            title: conv.title.clone(),
            source_path: conv.source_path.clone(),
            started_at: conv.started_at,
            ended_at: conv.ended_at,
            approx_tokens: None,
            metadata_json: conv.metadata.clone(),
            messages,
        };

        let InsertOutcome {
            conversation_id: _,
            inserted_indices,
        } = storage.insert_conversation_tree(agent_id, workspace_id, &conversation)?;

        if !inserted_indices.is_empty() {
            let new_msgs: Vec<_> = conv
                .messages
                .iter()
                .filter(|m| inserted_indices.contains(&m.idx))
                .cloned()
                .collect();
            t_index.add_messages(conv, &new_msgs)?;
        }
        Ok(())
    }

    fn map_role(role: &str) -> MessageRole {
        match role {
            "user" => MessageRole::User,
            "assistant" | "agent" => MessageRole::Agent,
            "tool" => MessageRole::Tool,
            "system" => MessageRole::System,
            other => MessageRole::Other(other.to_string()),
        }
    }
}
