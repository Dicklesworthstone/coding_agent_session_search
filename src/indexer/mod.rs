use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use notify::{RecursiveMode, Watcher, recommended_watcher};

use crate::connectors::NormalizedConversation;
use crate::connectors::{
    Connector, aider::AiderConnector, amp::AmpConnector, claude_code::ClaudeCodeConnector,
    cline::ClineConnector, codex::CodexConnector, gemini::GeminiConnector,
    opencode::OpenCodeConnector,
};
use crate::search::tantivy::{TantivyIndex, index_dir};
use crate::storage::sqlite::SqliteStorage;

#[derive(Debug, Clone)]
pub enum ReindexCommand {
    Full,
}

#[derive(Debug)]
pub enum IndexerEvent {
    Notify(Vec<PathBuf>),
    Command(ReindexCommand),
}

#[derive(Debug, Default)]
pub struct IndexingProgress {
    pub total: AtomicUsize,
    pub current: AtomicUsize,
    // Simple phase indicator: 0=Idle, 1=Scanning, 2=Indexing
    pub phase: AtomicUsize,
    pub is_rebuilding: AtomicBool,
}

#[derive(Clone)]
pub struct IndexOptions {
    pub full: bool,
    pub force_rebuild: bool,
    pub watch: bool,
    /// One-shot watch hook: when set, `watch_sources` will bypass notify and invoke reindex for these paths once.
    pub watch_once_paths: Option<Vec<PathBuf>>,
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    pub progress: Option<Arc<IndexingProgress>>,
}

pub fn run_index(
    opts: IndexOptions,
    event_channel: Option<(Sender<IndexerEvent>, Receiver<IndexerEvent>)>,
) -> Result<()> {
    let mut storage = SqliteStorage::open(&opts.db_path)?;
    let index_path = index_dir(&opts.data_dir)?;

    // Detect if we are rebuilding due to missing meta/schema mismatch
    let needs_rebuild = opts.force_rebuild
        || !index_path.join("meta.json").exists()
        || (index_path.join("schema_hash.json").exists()
            && !std::fs::read_to_string(index_path.join("schema_hash.json"))?
                .contains(crate::search::tantivy::SCHEMA_HASH));

    if needs_rebuild && let Some(p) = &opts.progress {
        p.is_rebuilding.store(true, Ordering::Relaxed);
    }

    let mut t_index = if needs_rebuild {
        std::fs::remove_dir_all(&index_path).ok();
        TantivyIndex::open_or_create(&index_path)?
    } else {
        TantivyIndex::open_or_create(&index_path)?
    };

    if opts.full {
        reset_storage(&mut storage)?;
        t_index.delete_all()?;
    }

    // Get last scan timestamp for incremental indexing.
    // If full rebuild or force_rebuild, scan everything (since_ts = None).
    // Otherwise, only scan files modified since last successful scan.
    let since_ts = if opts.full || needs_rebuild {
        None
    } else {
        storage.get_last_scan_ts().unwrap_or(None)
    };

    if since_ts.is_some() {
        tracing::info!(since_ts = ?since_ts, "incremental_scan: using last_scan_ts");
    } else {
        tracing::info!("full_scan: no last_scan_ts or rebuild requested");
    }

    // Record scan start time before scanning
    let scan_start_ts = SqliteStorage::now_millis();

    let connectors: Vec<(&'static str, Box<dyn Connector>)> = vec![
        ("codex", Box::new(CodexConnector::new())),
        ("cline", Box::new(ClineConnector::new())),
        ("gemini", Box::new(GeminiConnector::new())),
        ("claude", Box::new(ClaudeCodeConnector::new())),
        ("opencode", Box::new(OpenCodeConnector::new())),
        ("amp", Box::new(AmpConnector::new())),
        ("aider", Box::new(AiderConnector::new())),
    ];

    // First pass: Scan all to get counts if we have progress tracker
    let mut pending_batches = Vec::new();
    if let Some(p) = &opts.progress {
        p.phase.store(1, Ordering::Relaxed); // Scanning
    }

    for (name, conn) in &connectors {
        let detect = conn.detect();
        if !detect.detected {
            continue;
        }
        let ctx = crate::connectors::ScanContext {
            data_root: opts.data_dir.clone(),
            since_ts,
        };
        // We scan here. For optimization in non-progress mode, we could stream.
        // But to show accurate "X/Y", we need to collect.
        match conn.scan(&ctx) {
            Ok(convs) => {
                if let Some(p) = &opts.progress {
                    p.total.fetch_add(convs.len(), Ordering::Relaxed);
                }
                pending_batches.push((name, convs));
            }
            Err(e) => {
                tracing::warn!("scan failed for {}: {}", name, e);
            }
        }
    }

    if let Some(p) = &opts.progress {
        p.phase.store(2, Ordering::Relaxed); // Indexing
    }

    for (name, convs) in pending_batches {
        ingest_batch(&mut storage, &mut t_index, &convs, &opts.progress)?;
        tracing::info!(
            connector = name,
            conversations = convs.len(),
            "connector_ingest"
        );
    }

    t_index.commit()?;

    // Update last_scan_ts after successful scan and commit
    storage.set_last_scan_ts(scan_start_ts)?;
    tracing::info!(
        scan_start_ts,
        "updated last_scan_ts for incremental indexing"
    );

    if let Some(p) = &opts.progress {
        p.phase.store(0, Ordering::Relaxed); // Idle
        p.is_rebuilding.store(false, Ordering::Relaxed);
    }

    if opts.watch || opts.watch_once_paths.is_some() {
        let opts_clone = opts.clone();
        let state = Arc::new(Mutex::new(load_watch_state(&opts.data_dir)));
        let storage = Arc::new(Mutex::new(storage));
        let t_index = Arc::new(Mutex::new(t_index));

        watch_sources(
            opts.watch_once_paths.clone(),
            event_channel,
            move |paths, is_rebuild| {
                if is_rebuild {
                    // For full rebuild, we effectively restart the index process
                    // But here we just trigger a re-scan of all roots
                    // For simplicity, we can't easily recurse into run_index (lock issues)
                    // So we emulate a re-scan by passing all watch roots and clearing since_ts logic
                    // Or we can just call reindex_paths with all roots and a flag to ignore ts?
                    // reindex_paths uses classify_paths which uses mtime.
                    // To force reindex, we might need to clear watch state.
                    if let Ok(mut g) = state.lock() {
                        g.clear();
                        let _ = save_watch_state(&opts_clone.data_dir, &g);
                    }
                    // Pass all watch roots
                    let roots = watch_roots();
                    let _ = reindex_paths(
                        &opts_clone,
                        roots,
                        state.clone(),
                        storage.clone(),
                        t_index.clone(),
                        true,
                    );
                } else {
                    let _ = reindex_paths(
                        &opts_clone,
                        paths,
                        state.clone(),
                        storage.clone(),
                        t_index.clone(),
                        false,
                    );
                }
            },
        )?;
    }

    Ok(())
}

fn ingest_batch(
    storage: &mut SqliteStorage,
    t_index: &mut TantivyIndex,
    convs: &[NormalizedConversation],
    progress: &Option<Arc<IndexingProgress>>,
) -> Result<()> {
    for conv in convs {
        persist::persist_conversation(storage, t_index, conv)?;
        if let Some(p) = progress {
            p.current.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn watch_sources<F: Fn(Vec<PathBuf>, bool) + Send + 'static>(
    watch_once_paths: Option<Vec<PathBuf>>,
    event_channel: Option<(Sender<IndexerEvent>, Receiver<IndexerEvent>)>,
    callback: F,
) -> Result<()> {
    if let Some(paths) = watch_once_paths {
        if !paths.is_empty() {
            callback(paths, false);
        }
        return Ok(());
    }

    let (tx, rx) = event_channel.unwrap_or_else(crossbeam_channel::unbounded);
    let tx_clone = tx.clone();

    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx_clone.send(IndexerEvent::Notify(event.paths));
        }
    })?;

    for dir in watch_roots() {
        let _ = watcher.watch(&dir, RecursiveMode::Recursive);
    }

    let debounce = Duration::from_secs(2);
    let max_wait = Duration::from_secs(5);
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut first_event: Option<std::time::Instant> = None;

    loop {
        if pending.is_empty() {
            match rx.recv() {
                Ok(event) => match event {
                    IndexerEvent::Notify(paths) => {
                        pending.extend(paths);
                        first_event = Some(std::time::Instant::now());
                    }
                    IndexerEvent::Command(cmd) => match cmd {
                        ReindexCommand::Full => {
                            callback(vec![], true);
                        }
                    },
                },
                Err(_) => break, // Channel closed
            }
        } else {
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(first_event.unwrap_or(now));
            if elapsed >= max_wait {
                callback(std::mem::take(&mut pending), false);
                first_event = None;
                continue;
            }

            let remaining = max_wait.checked_sub(elapsed).unwrap();
            let wait = debounce.min(remaining);

            match rx.recv_timeout(wait) {
                Ok(event) => match event {
                    IndexerEvent::Notify(paths) => pending.extend(paths),
                    IndexerEvent::Command(cmd) => match cmd {
                        ReindexCommand::Full => {
                            // Flush pending first? Or discard?
                            // Let's flush pending then do full.
                            if !pending.is_empty() {
                                callback(std::mem::take(&mut pending), false);
                            }
                            callback(vec![], true);
                            first_event = None; // Reset debounce
                        }
                    },
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    callback(std::mem::take(&mut pending), false);
                    first_event = None;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    Ok(())
}

fn watch_roots() -> Vec<PathBuf> {
    vec![
        std::env::var("CODEX_HOME").map_or_else(
            |_| dirs::home_dir().unwrap_or_default().join(".codex/sessions"),
            PathBuf::from,
        ),
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
        "DELETE FROM fts_messages;
         DELETE FROM snippets;
         DELETE FROM messages;
         DELETE FROM conversations;
         DELETE FROM agents;
         DELETE FROM workspaces;
         DELETE FROM tags;
         DELETE FROM conversation_tags;",
    )?;
    Ok(())
}

fn reindex_paths(
    opts: &IndexOptions,
    paths: Vec<PathBuf>,
    state: Arc<Mutex<HashMap<ConnectorKind, i64>>>,
    storage: Arc<Mutex<SqliteStorage>>,
    t_index: Arc<Mutex<TantivyIndex>>,
    force_full: bool,
) -> Result<()> {
    let mut storage = storage
        .lock()
        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
    let mut t_index = t_index
        .lock()
        .map_err(|_| anyhow::anyhow!("index lock poisoned"))?;

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
            ConnectorKind::Aider => Box::new(AiderConnector::new()),
        };
        let detect = conn.detect();
        if !detect.detected {
            continue;
        }

        // Update phase to scanning
        if let Some(p) = &opts.progress {
            p.phase.store(1, Ordering::Relaxed);
        }

        let since_ts = if force_full {
            None
        } else {
            let guard = state
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            guard
                .get(&kind)
                .copied()
                .or_else(|| ts.map(|v| v.saturating_sub(1)))
        };
        let ctx = crate::connectors::ScanContext {
            data_root: opts.data_dir.clone(),
            since_ts,
        };
        let convs = conn.scan(&ctx)?;

        // Update total and phase to indexing
        if let Some(p) = &opts.progress {
            p.total.fetch_add(convs.len(), Ordering::Relaxed);
            p.phase.store(2, Ordering::Relaxed);
        }

        tracing::info!(?kind, conversations = convs.len(), since_ts, "watch_scan");
        ingest_batch(&mut storage, &mut t_index, &convs, &opts.progress)?;

        // Commit to Tantivy immediately to ensure index consistency before advancing watch state.
        // This prevents a state where we think we've indexed up to T, but the index is stale.
        t_index.commit()?;

        if let Some(ts_val) = ts {
            let mut guard = state
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let entry = guard.entry(kind).or_insert(ts_val);
            *entry = (*entry).max(ts_val);
            save_watch_state(&opts.data_dir, &guard)?;
        }
    }

    // Reset phase to idle if progress exists
    if let Some(p) = &opts.progress {
        p.phase.store(0, Ordering::Relaxed);
    }

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
    Aider,
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
                } else if s.contains(".aider.chat.history.md") {
                    Some(ConnectorKind::Aider)
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
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
    use crate::search::tantivy::TantivyIndex;
    use crate::storage::sqlite::{InsertOutcome, SqliteStorage};

    /// Convert a NormalizedConversation to the internal Conversation type for SQLite storage.
    pub fn map_to_internal(conv: &NormalizedConversation) -> Conversation {
        Conversation {
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
            messages: conv
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
                    snippets: m
                        .snippets
                        .iter()
                        .map(|s| Snippet {
                            id: None,
                            file_path: s.file_path.clone(),
                            start_line: s.start_line,
                            end_line: s.end_line,
                            language: s.language.clone(),
                            snippet_text: s.snippet_text.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn persist_conversation(
        storage: &mut SqliteStorage,
        t_index: &mut TantivyIndex,
        conv: &NormalizedConversation,
    ) -> Result<()> {
        tracing::info!(agent = %conv.agent_slug, messages = conv.messages.len(), "persist_conversation");
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

        let internal_conv = map_to_internal(conv);

        let InsertOutcome {
            conversation_id: _,
            inserted_indices,
        } = storage.insert_conversation_tree(agent_id, workspace_id, &internal_conv)?;

        // Only add newly inserted messages to the Tantivy index (incremental)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{NormalizedConversation, NormalizedMessage};
    use rusqlite::Connection;
    use serial_test::serial;
    use tempfile::TempDir;

    fn norm_msg(idx: i64, created_at: i64) -> NormalizedMessage {
        NormalizedMessage {
            idx,
            role: "user".into(),
            author: Some("u".into()),
            created_at: Some(created_at),
            content: format!("msg-{idx}"),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
        }
    }

    fn norm_conv(
        external_id: Option<&str>,
        msgs: Vec<NormalizedMessage>,
    ) -> NormalizedConversation {
        NormalizedConversation {
            agent_slug: "tester".into(),
            external_id: external_id.map(std::borrow::ToOwned::to_owned),
            title: Some("Demo".into()),
            workspace: Some(PathBuf::from("/workspace/demo")),
            source_path: PathBuf::from("/logs/demo.jsonl"),
            started_at: msgs.first().and_then(|m| m.created_at),
            ended_at: msgs.last().and_then(|m| m.created_at),
            metadata: serde_json::json!({}),
            messages: msgs,
        }
    }

    #[test]
    fn reset_storage_clears_data_but_leaves_meta() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("db.sqlite");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        ensure_fts_schema(storage.raw());

        let agent = crate::model::types::Agent {
            id: None,
            slug: "tester".into(),
            name: "Tester".into(),
            version: None,
            kind: crate::model::types::AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conv = norm_conv(Some("c1"), vec![norm_msg(0, 10)]);
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &crate::model::types::Conversation {
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
                    messages: conv
                        .messages
                        .iter()
                        .map(|m| crate::model::types::Message {
                            id: None,
                            idx: m.idx,
                            role: crate::model::types::MessageRole::User,
                            author: m.author.clone(),
                            created_at: m.created_at,
                            content: m.content.clone(),
                            extra_json: m.extra.clone(),
                            snippets: Vec::new(),
                        })
                        .collect(),
                },
            )
            .unwrap();

        let msg_count: i64 = storage
            .raw()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg_count, 1);

        reset_storage(&mut storage).unwrap();

        let msg_count: i64 = storage
            .raw()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg_count, 0);
        assert_eq!(storage.schema_version().unwrap(), 3);
    }

    #[test]
    fn persist_append_only_adds_new_messages_to_index() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        ensure_fts_schema(storage.raw());
        let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir).unwrap()).unwrap();

        let conv1 = norm_conv(Some("ext"), vec![norm_msg(0, 100), norm_msg(1, 200)]);
        persist::persist_conversation(&mut storage, &mut index, &conv1).unwrap();
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        assert_eq!(reader.searcher().num_docs(), 2);

        let conv2 = norm_conv(
            Some("ext"),
            vec![norm_msg(0, 100), norm_msg(1, 200), norm_msg(2, 300)],
        );
        persist::persist_conversation(&mut storage, &mut index, &conv2).unwrap();
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        assert_eq!(reader.searcher().num_docs(), 3);
    }

    #[test]
    fn classify_paths_uses_latest_mtime_per_connector() {
        let tmp = TempDir::new().unwrap();
        let codex = tmp.path().join(".codex/sessions/rollout-1.jsonl");
        std::fs::create_dir_all(codex.parent().unwrap()).unwrap();
        std::fs::write(&codex, "{{}}\n{{}}").unwrap();

        let claude = tmp.path().join("project/.claude.json");
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(&claude, "{{}}").unwrap();

        let paths = vec![codex.clone(), claude.clone()];
        let classified = classify_paths(paths);

        let kinds: std::collections::HashSet<_> = classified.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&ConnectorKind::Codex));
        assert!(kinds.contains(&ConnectorKind::Claude));

        for (_, ts) in classified {
            assert!(ts.is_some(), "mtime should be captured");
        }
    }

    #[test]
    fn watch_state_round_trips_to_disk() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut state = HashMap::new();
        state.insert(ConnectorKind::Codex, 123);
        state.insert(ConnectorKind::Gemini, 456);

        save_watch_state(&data_dir, &state).unwrap();

        let loaded = load_watch_state(&data_dir);
        assert_eq!(loaded.get(&ConnectorKind::Codex), Some(&123));
        assert_eq!(loaded.get(&ConnectorKind::Gemini), Some(&456));
    }

    #[test]
    #[serial]
    fn watch_state_updates_after_reindex_paths() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Use dirs::data_dir() to align with connector detection roots.
        let data_dir = dirs::data_dir().unwrap().join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Prepare amp fixture under data dir so detection + scan succeed.
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-002.json");
        std::fs::write(
            &amp_file,
            r#"{{
  "id": "thread-002",
  "title": "Amp test",
  "messages": [
    {{"role":"user","text":"hi","createdAt":1700000000100}},
    {{"role":"assistant","text":"hello","createdAt":1700000000200}}
  ]
}} "#,
        )
        .unwrap();

        let opts = super::IndexOptions {
            full: false,
            watch: false,
            force_rebuild: false,
            db_path: data_dir.join("agent_search.db"),
            data_dir: data_dir.clone(),
            progress: None,
            watch_once_paths: None,
        };

        // Manually set up dependencies for reindex_paths
        let storage = SqliteStorage::open(&opts.db_path).unwrap();
        let t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir).unwrap()).unwrap();

        let state = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let storage = std::sync::Arc::new(std::sync::Mutex::new(storage));
        let t_index = std::sync::Arc::new(std::sync::Mutex::new(t_index));

        reindex_paths(
            &opts,
            vec![amp_file.clone()],
            state.clone(),
            storage,
            t_index,
            false,
        )
        .unwrap();

        let loaded = load_watch_state(&data_dir);
        assert!(loaded.contains_key(&ConnectorKind::Amp));
        let ts = loaded.get(&ConnectorKind::Amp).copied().unwrap();
        assert!(ts > 0);

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }

    fn ensure_fts_schema(conn: &Connection) {
        let mut stmt = conn
            .prepare("PRAGMA table_info(fts_messages)")
            .expect("prepare table_info");
        let cols: Vec<String> = stmt
            .query_map([], |row: &rusqlite::Row| row.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        if !cols.iter().any(|c| c == "created_at") {
            conn.execute_batch(
                r#""
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
""#,
            )
            .unwrap();
        }
    }

    #[test]
    #[serial]
    fn reindex_paths_updates_progress() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Prepare amp fixture
        let data_dir = dirs::data_dir().unwrap().join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-progress.json");
        // Use a timestamp well in the future to avoid race with file mtime.
        // The since_ts filter compares message.createdAt > file_mtime - 1, so if
        // there's any delay between capturing 'now' and writing the file, the message
        // could be filtered out. Adding 10s buffer ensures the message is always included.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + 10_000;
        std::fs::write(
            &amp_file,
            format!(r#"{{"id":"tp","messages":[{{"role":"user","text":"p","createdAt":{now}}}]}}"#),
        )
        .unwrap();

        let progress = Arc::new(super::IndexingProgress::default());
        let opts = super::IndexOptions {
            full: false,
            watch: false,
            force_rebuild: false,
            watch_once_paths: None,
            db_path: data_dir.join("db.sqlite"),
            data_dir: data_dir.clone(),
            progress: Some(progress.clone()),
        };

        let storage = SqliteStorage::open(&opts.db_path).unwrap();
        let t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir).unwrap()).unwrap();
        let state = Arc::new(Mutex::new(HashMap::new()));
        let storage = Arc::new(Mutex::new(storage));
        let t_index = Arc::new(Mutex::new(t_index));

        reindex_paths(&opts, vec![amp_file], state, storage, t_index, false).unwrap();

        // Progress should reflect the indexed conversation
        assert_eq!(progress.total.load(Ordering::Relaxed), 1);
        assert_eq!(progress.current.load(Ordering::Relaxed), 1);
        // Phase resets to 0 (idle) at the end
        assert_eq!(progress.phase.load(Ordering::Relaxed), 0);

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }
}
