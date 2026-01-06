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
    Connector, ScanRoot, aider::AiderConnector, amp::AmpConnector, chatgpt::ChatGptConnector,
    claude_code::ClaudeCodeConnector, cline::ClineConnector, codex::CodexConnector,
    cursor::CursorConnector, factory::FactoryConnector, gemini::GeminiConnector,
    opencode::OpenCodeConnector, pi_agent::PiAgentConnector,
};
use crate::search::tantivy::{TantivyIndex, index_dir};
use crate::sources::config::{Platform, SourcesConfig};
use crate::sources::provenance::{Origin, Source};
use crate::sources::sync::path_to_safe_dirname;
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
    /// Number of coding agents discovered so far during scanning
    pub discovered_agents: AtomicUsize,
    /// Names of discovered agents (protected by mutex for concurrent access)
    pub discovered_agent_names: Mutex<Vec<String>>,
    /// Last error message from background indexer, if any
    pub last_error: Mutex<Option<String>>,
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

    // Detect if we are rebuilding due to missing meta/schema mismatch/index corruption.
    // IMPORTANT: This must stay aligned with TantivyIndex::open_or_create() rebuild triggers.
    let schema_hash_path = index_path.join("schema_hash.json");
    let schema_matches = schema_hash_path.exists()
        && std::fs::read_to_string(&schema_hash_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|json| {
                json.get("schema_hash")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .as_deref()
            == Some(crate::search::tantivy::SCHEMA_HASH);

    // Treat missing schema hash as rebuild (open_or_create will wipe/recreate).
    let mut needs_rebuild =
        opts.force_rebuild || !index_path.join("meta.json").exists() || !schema_matches;

    // Preflight open: if Tantivy can't open, force a rebuild so we do a full scan and
    // reindex messages into the new Tantivy index (SQLite is incremental-only by default).
    if !needs_rebuild && let Err(e) = tantivy::Index::open_in_dir(&index_path) {
        tracing::warn!(
            error = %e,
            path = %index_path.display(),
            "tantivy open preflight failed; forcing rebuild"
        );
        needs_rebuild = true;
    }

    if needs_rebuild && let Some(p) = &opts.progress {
        p.is_rebuilding.store(true, Ordering::Relaxed);
    }

    if needs_rebuild {
        // Clean slate: avoid stale lock files and ensure a fresh Tantivy index.
        let _ = std::fs::remove_dir_all(&index_path);
    }
    let mut t_index = TantivyIndex::open_or_create(&index_path)?;

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
        storage
            .get_last_scan_ts()
            .unwrap_or(None)
            .map(|ts| ts.saturating_sub(1))
    };

    if since_ts.is_some() {
        tracing::info!(since_ts = ?since_ts, "incremental_scan: using last_scan_ts");
    } else {
        tracing::info!("full_scan: no last_scan_ts or rebuild requested");
    }

    // Record scan start time before scanning
    let scan_start_ts = SqliteStorage::now_millis();

    let connector_factories = get_connector_factories();

    // First pass: Scan all to get counts if we have progress tracker
    // Use parallel iteration for faster agent discovery
    if let Some(p) = &opts.progress {
        p.phase.store(1, Ordering::Relaxed); // Scanning
        // Track connector scan progress during discovery.
        p.total.store(connector_factories.len(), Ordering::Relaxed);
        p.current.store(0, Ordering::Relaxed);
        p.discovered_agents.store(0, Ordering::Relaxed);
        if let Ok(mut names) = p.discovered_agent_names.lock() {
            names.clear();
        }
        if let Ok(mut last_error) = p.last_error.lock() {
            *last_error = None;
        }
    }

    // Keep sources table in sync with sources.toml for provenance integrity.
    sync_sources_config_to_db(&storage);

    let scan_roots = build_scan_roots(&storage, &opts.data_dir);
    let remote_roots: Vec<ScanRoot> = scan_roots
        .iter()
        .filter(|r| r.origin.is_remote())
        .cloned()
        .collect();

    // Run connector detection and scanning in parallel using rayon
    use rayon::prelude::*;

    let progress_ref = opts.progress.as_ref();
    let data_dir = opts.data_dir.clone();

    let pending_batches: Vec<(&'static str, Vec<NormalizedConversation>)> = connector_factories
        .into_par_iter()
        .filter_map(|(name, factory)| {
            let conn = factory();
            let detect = conn.detect();
            let was_detected = detect.detected;
            let mut convs = Vec::new();

            if detect.detected {
                // Update discovered agents count immediately when detected
                // This gives fast UI feedback during the discovery phase
                if let Some(p) = progress_ref {
                    p.discovered_agents.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut names) = p.discovered_agent_names.lock() {
                        names.push(name.to_string());
                    }
                }

                let ctx = crate::connectors::ScanContext::local_default(data_dir.clone(), since_ts);
                match conn.scan(&ctx) {
                    Ok(mut local_convs) => {
                        let local_origin = Origin::local();
                        for conv in &mut local_convs {
                            inject_provenance(conv, &local_origin);
                        }
                        convs.extend(local_convs);
                    }
                    Err(e) => {
                        // Note: agent was counted as discovered but scan failed
                        // This is acceptable as detection succeeded (agent exists)
                        tracing::warn!("scan failed for {}: {}", name, e);
                    }
                }
            }

            if !remote_roots.is_empty() {
                for root in &remote_roots {
                    let ctx = crate::connectors::ScanContext::with_roots(
                        root.path.clone(),
                        vec![root.clone()],
                        None,
                    );
                    match conn.scan(&ctx) {
                        Ok(mut remote_convs) => {
                            for conv in &mut remote_convs {
                                inject_provenance(conv, &root.origin);
                                apply_workspace_rewrite(conv, &root.workspace_rewrites);
                            }
                            convs.extend(remote_convs);
                        }
                        Err(e) => {
                            tracing::warn!(
                                connector = name,
                                root = %root.path.display(),
                                "remote scan failed: {e}"
                            );
                        }
                    }
                }
            }

            if !was_detected && let Some(p) = progress_ref {
                p.discovered_agents.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut names) = p.discovered_agent_names.lock() {
                    names.push(name.to_string());
                }
            }

            // Mark this connector as scanned for discovery progress.
            if let Some(p) = progress_ref {
                p.current.fetch_add(1, Ordering::Relaxed);
            }

            if convs.is_empty() {
                return None;
            }

            tracing::info!(
                connector = name,
                conversations = convs.len(),
                "parallel_scan_complete"
            );
            Some((name, convs))
        })
        .collect();

    if let Some(p) = &opts.progress {
        let total_conversations: usize = pending_batches.iter().map(|(_, convs)| convs.len()).sum();
        p.phase.store(2, Ordering::Relaxed); // Indexing
        p.total.store(total_conversations, Ordering::Relaxed);
        p.current.store(0, Ordering::Relaxed);
    }

    for (name, convs) in pending_batches {
        ingest_batch(
            &mut storage,
            &mut t_index,
            &convs,
            &opts.progress,
            needs_rebuild,
        )?;
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

        // Detect roots once for the watcher setup
        let watch_roots = detect_watch_roots();

        watch_sources(
            opts.watch_once_paths.clone(),
            watch_roots.clone(),
            event_channel,
            move |paths, roots, is_rebuild| {
                if is_rebuild {
                    if let Ok(mut g) = state.lock() {
                        g.clear();
                        let _ = save_watch_state(&opts_clone.data_dir, &g);
                    }
                    // For rebuild, trigger reindex on all active roots
                    let all_root_paths: Vec<PathBuf> =
                        roots.iter().map(|(_, p)| p.clone()).collect();
                    let _ = reindex_paths(
                        &opts_clone,
                        all_root_paths,
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
                        roots,
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
    force_tantivy_reindex: bool,
) -> Result<()> {
    // Use batched insert for better SQLite performance (single transaction)
    persist::persist_conversations_batched(storage, t_index, convs, force_tantivy_reindex)?;

    // Update progress counter for all conversations at once
    if let Some(p) = progress {
        p.current.fetch_add(convs.len(), Ordering::Relaxed);
    }
    Ok(())
}

/// Get all available connector factories.
#[allow(clippy::type_complexity)]
pub fn get_connector_factories() -> Vec<(&'static str, fn() -> Box<dyn Connector + Send>)> {
    vec![
        ("codex", || Box::new(CodexConnector::new())),
        ("cline", || Box::new(ClineConnector::new())),
        ("gemini", || Box::new(GeminiConnector::new())),
        ("claude", || Box::new(ClaudeCodeConnector::new())),
        ("opencode", || Box::new(OpenCodeConnector::new())),
        ("amp", || Box::new(AmpConnector::new())),
        ("aider", || Box::new(AiderConnector::new())),
        ("cursor", || Box::new(CursorConnector::new())),
        ("chatgpt", || Box::new(ChatGptConnector::new())),
        ("pi_agent", || Box::new(PiAgentConnector::new())),
        ("factory", || Box::new(FactoryConnector::new())),
    ]
}

/// Detect all active roots for watching/scanning.
fn detect_watch_roots() -> Vec<(ConnectorKind, PathBuf)> {
    let factories = get_connector_factories();
    let mut roots = Vec::new();

    for (name, factory) in factories {
        if let Some(kind) = ConnectorKind::from_slug(name) {
            let conn = factory();
            let detection = conn.detect();
            if detection.detected {
                for root in detection.root_paths {
                    roots.push((kind, root));
                }
            }
        }
    }
    roots
}

impl ConnectorKind {
    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "codex" => Some(Self::Codex),
            "cline" => Some(Self::Cline),
            "gemini" => Some(Self::Gemini),
            "claude" => Some(Self::Claude),
            "amp" => Some(Self::Amp),
            "opencode" => Some(Self::OpenCode),
            "aider" => Some(Self::Aider),
            "cursor" => Some(Self::Cursor),
            "chatgpt" => Some(Self::ChatGpt),
            "pi_agent" => Some(Self::PiAgent),
            "factory" => Some(Self::Factory),
            _ => None,
        }
    }

    /// Create a boxed connector instance for this kind.
    /// Centralizes connector instantiation to avoid duplicate match arms.
    fn create_connector(&self) -> Box<dyn Connector + Send> {
        match self {
            Self::Codex => Box::new(CodexConnector::new()),
            Self::Cline => Box::new(ClineConnector::new()),
            Self::Gemini => Box::new(GeminiConnector::new()),
            Self::Claude => Box::new(ClaudeCodeConnector::new()),
            Self::Amp => Box::new(AmpConnector::new()),
            Self::OpenCode => Box::new(OpenCodeConnector::new()),
            Self::Aider => Box::new(AiderConnector::new()),
            Self::Cursor => Box::new(CursorConnector::new()),
            Self::ChatGpt => Box::new(ChatGptConnector::new()),
            Self::PiAgent => Box::new(PiAgentConnector::new()),
            Self::Factory => Box::new(FactoryConnector::new()),
        }
    }
}

fn watch_sources<F: Fn(Vec<PathBuf>, &[(ConnectorKind, PathBuf)], bool) + Send + 'static>(
    watch_once_paths: Option<Vec<PathBuf>>,
    roots: Vec<(ConnectorKind, PathBuf)>,
    event_channel: Option<(Sender<IndexerEvent>, Receiver<IndexerEvent>)>,
    callback: F,
) -> Result<()> {
    if let Some(paths) = watch_once_paths {
        if !paths.is_empty() {
            callback(paths, &roots, false);
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

    // Watch all detected roots
    for (_, dir) in &roots {
        if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
            tracing::warn!("failed to watch {}: {}", dir.display(), e);
        } else {
            tracing::info!("watching {}", dir.display());
        }
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
                            callback(vec![], &roots, true);
                        }
                    },
                },
                Err(_) => break, // Channel closed
            }
        } else {
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(first_event.unwrap_or(now));
            if elapsed >= max_wait {
                callback(std::mem::take(&mut pending), &roots, false);
                first_event = None; // Reset debounce
                continue;
            }

            let remaining = max_wait.saturating_sub(elapsed);
            let wait = debounce.min(remaining);

            match rx.recv_timeout(wait) {
                Ok(event) => match event {
                    IndexerEvent::Notify(paths) => pending.extend(paths),
                    IndexerEvent::Command(cmd) => match cmd {
                        ReindexCommand::Full => {
                            // Flush pending first? Or discard?
                            // Let's flush pending then do full.
                            if !pending.is_empty() {
                                callback(std::mem::take(&mut pending), &roots, false);
                            }
                            callback(vec![], &roots, true);
                            first_event = None; // Reset debounce
                        }
                    },
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    callback(std::mem::take(&mut pending), &roots, false);
                    first_event = None;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    Ok(())
}

fn reset_storage(storage: &mut SqliteStorage) -> Result<()> {
    // Wrap in transaction to ensure atomic reset - if any DELETE fails,
    // all changes are rolled back to prevent inconsistent state
    storage.raw().execute_batch(
        "BEGIN TRANSACTION;
         DELETE FROM fts_messages;
         DELETE FROM snippets;
         DELETE FROM messages;
         DELETE FROM conversations;
         DELETE FROM agents;
         DELETE FROM workspaces;
         DELETE FROM tags;
         DELETE FROM conversation_tags;
         COMMIT;",
    )?;
    Ok(())
}

fn reindex_paths(
    opts: &IndexOptions,
    paths: Vec<PathBuf>,
    roots: &[(ConnectorKind, PathBuf)],
    state: Arc<Mutex<HashMap<ConnectorKind, i64>>>,
    storage: Arc<Mutex<SqliteStorage>>,
    t_index: Arc<Mutex<TantivyIndex>>,
    force_full: bool,
) -> Result<()> {
    // DO NOT lock storage/index here for the whole duration.
    // We only need them for the ingest phase, not the scan phase.

    let triggers = classify_paths(paths, roots);
    if triggers.is_empty() {
        return Ok(());
    }

    for (kind, ts) in triggers {
        let conn = kind.create_connector();
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
                .map(|v| v.saturating_sub(1))
        };
        let ctx = crate::connectors::ScanContext::local_default(opts.data_dir.clone(), since_ts);

        // SCAN PHASE: IO-heavy, no locks held
        let mut convs = conn.scan(&ctx)?;

        // Inject local provenance into all conversations (P2.2)
        let local_origin = Origin::local();
        for conv in &mut convs {
            inject_provenance(conv, &local_origin);
        }

        // Update total and phase to indexing
        if let Some(p) = &opts.progress {
            p.total.fetch_add(convs.len(), Ordering::Relaxed);
            p.phase.store(2, Ordering::Relaxed);
        }

        tracing::info!(?kind, conversations = convs.len(), since_ts, "watch_scan");

        // INGEST PHASE: Acquire locks briefly
        {
            let mut storage = storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
            let mut t_index = t_index
                .lock()
                .map_err(|_| anyhow::anyhow!("index lock poisoned"))?;

            ingest_batch(&mut storage, &mut t_index, &convs, &opts.progress, false)?;

            // Commit to Tantivy immediately to ensure index consistency before advancing watch state.
            t_index.commit()?;
        }

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
    Cursor,
    ChatGpt,
    PiAgent,
    Factory,
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

fn classify_paths(
    paths: Vec<PathBuf>,
    roots: &[(ConnectorKind, PathBuf)],
) -> Vec<(ConnectorKind, Option<i64>)> {
    let mut map: HashMap<ConnectorKind, Option<i64>> = HashMap::new();
    for p in paths {
        if let Ok(meta) = std::fs::metadata(&p)
            && let Ok(time) = meta.modified()
            && let Ok(dur) = time.duration_since(std::time::UNIX_EPOCH)
        {
            let ts = Some(dur.as_millis() as i64);

            // Check against known roots (dynamic classification)
            let kind = roots.iter().find_map(
                |(k, root)| {
                    if p.starts_with(root) { Some(*k) } else { None }
                },
            );

            if let Some(kind) = kind {
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

fn sync_sources_config_to_db(storage: &SqliteStorage) {
    if dotenvy::var("CASS_IGNORE_SOURCES_CONFIG").is_ok() {
        return;
    }
    let config = match SourcesConfig::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::debug!("sources config load failed: {e}");
            return;
        }
    };

    for source in config.remote_sources() {
        let platform = source.platform.map(|p| match p {
            Platform::Macos => "macos".to_string(),
            Platform::Linux => "linux".to_string(),
            Platform::Windows => "windows".to_string(),
        });

        let config_json = serde_json::json!({
            "paths": source.paths.clone(),
            "path_mappings": source.path_mappings.clone(),
            "sync_schedule": source.sync_schedule,
        });

        let record = Source {
            id: source.name.clone(),
            kind: source.source_type,
            host_label: source.host.clone(),
            machine_id: None,
            platform,
            config_json: Some(config_json),
            created_at: None,
            updated_at: None,
        };

        if let Err(e) = storage.upsert_source(&record) {
            tracing::warn!(
                source_id = %record.id,
                "failed to upsert source into db: {e}"
            );
        }
    }
}

/// Build a list of scan roots for multi-root indexing.
///
/// This function collects both:
/// 1. Local default roots (from watch_roots() or standard locations)
/// 2. Remote mirror roots (from registered sources in the database)
///
/// Part of P2.2 - Indexer multi-root orchestration.
pub fn build_scan_roots(storage: &SqliteStorage, data_dir: &Path) -> Vec<ScanRoot> {
    let mut roots = Vec::new();

    // Add local default root with local provenance
    // We create a single "local" root that encompasses all local paths.
    // Connectors will use their own default detection logic when given an empty scan_roots.
    // For explicit multi-root support, we add the local root.
    roots.push(ScanRoot::local(data_dir.to_path_buf()));

    if dotenvy::var("CASS_IGNORE_SOURCES_CONFIG").is_err()
        && let Ok(config) = SourcesConfig::load()
    {
        let remotes: Vec<_> = config.remote_sources().collect();
        if !remotes.is_empty() {
            for source in remotes {
                let origin = Origin {
                    source_id: source.name.clone(),
                    kind: source.source_type,
                    host: source.host.clone(),
                };
                let platform = source.platform;
                let workspace_rewrites = source.path_mappings.clone();

                for path in &source.paths {
                    let expanded_path = if path.starts_with("~/") {
                        path.to_string()
                    } else if path.starts_with('~') {
                        path.replacen('~', "~/", 1)
                    } else {
                        path.to_string()
                    };
                    let safe_name = path_to_safe_dirname(&expanded_path);
                    let mirror_path = data_dir
                        .join("remotes")
                        .join(&source.name)
                        .join("mirror")
                        .join(&safe_name);
                    if !mirror_path.exists() {
                        continue;
                    }

                    let mut scan_root = ScanRoot::remote(mirror_path, origin.clone(), platform);
                    scan_root.workspace_rewrites = workspace_rewrites.clone();
                    roots.push(scan_root);
                }
            }
            return roots;
        }
    }

    // Fallback: remote mirror roots from registered sources
    if let Ok(sources) = storage.list_sources() {
        for source in sources {
            // Skip local source - already handled above
            if !source.kind.is_remote() {
                continue;
            }

            // Parse platform from source
            let platform =
                source
                    .platform
                    .as_deref()
                    .and_then(|p| match p.to_lowercase().as_str() {
                        "macos" => Some(Platform::Macos),
                        "linux" => Some(Platform::Linux),
                        "windows" => Some(Platform::Windows),
                        _ => None,
                    });

            // Parse workspace rewrites from config_json
            // Format: array of {from, to, agents?} objects
            let workspace_rewrites = source
                .config_json
                .as_ref()
                .and_then(|cfg| cfg.get("path_mappings"))
                .and_then(|arr| arr.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let from = item.get("from")?.as_str()?.to_string();
                            let to = item.get("to")?.as_str()?.to_string();
                            let agents = item.get("agents").and_then(|a| {
                                a.as_array().map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect()
                                })
                            });
                            Some(crate::sources::config::PathMapping { from, to, agents })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            if let Some(paths) = source
                .config_json
                .as_ref()
                .and_then(|cfg| cfg.get("paths"))
                .and_then(|arr| arr.as_array())
            {
                for path_val in paths {
                    let Some(path) = path_val.as_str() else {
                        continue;
                    };
                    let expanded_path = if path.starts_with("~/") {
                        path.to_string()
                    } else if path.starts_with('~') {
                        path.replacen('~', "~/", 1)
                    } else {
                        path.to_string()
                    };
                    let safe_name = path_to_safe_dirname(&expanded_path);
                    let mirror_path = data_dir
                        .join("remotes")
                        .join(&source.id)
                        .join("mirror")
                        .join(&safe_name);
                    if !mirror_path.exists() {
                        continue;
                    }

                    let origin = Origin {
                        source_id: source.id.clone(),
                        kind: source.kind,
                        host: source.host_label.clone(),
                    };
                    let mut scan_root = ScanRoot::remote(mirror_path, origin, platform);
                    scan_root.workspace_rewrites = workspace_rewrites.clone();
                    roots.push(scan_root);
                }
                continue;
            }

            // Remote mirror directory: data_dir/remotes/<source_id>/mirror
            let mirror_path = data_dir.join("remotes").join(&source.id).join("mirror");

            if mirror_path.exists() {
                let origin = Origin {
                    source_id: source.id.clone(),
                    kind: source.kind,
                    host: source.host_label.clone(),
                };
                let mut scan_root = ScanRoot::remote(mirror_path, origin, platform);
                scan_root.workspace_rewrites = workspace_rewrites;

                roots.push(scan_root);
            }
        }
    }

    roots
}

/// Inject provenance metadata into a conversation from a scan root's origin.
///
/// This adds the `cass.origin` field to the conversation's metadata JSON
/// so that persistence can extract and store the source_id.
///
/// Part of P2.2 - provenance injection.
fn inject_provenance(conv: &mut NormalizedConversation, origin: &Origin) {
    // Ensure metadata is an object
    if !conv.metadata.is_object() {
        conv.metadata = serde_json::json!({});
    }

    // Add cass.origin provenance
    if let Some(obj) = conv.metadata.as_object_mut() {
        let cass = obj
            .entry("cass".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(cass_obj) = cass.as_object_mut() {
            cass_obj.insert(
                "origin".to_string(),
                serde_json::json!({
                    "source_id": origin.source_id,
                    "kind": origin.kind.as_str(),
                    "host": origin.host
                }),
            );
        }
    }
}

/// Apply workspace path rewriting to a conversation.
///
/// This rewrites workspace paths from remote formats to local equivalents
/// at ingest time, ensuring that workspace filters work consistently
/// across local and remote sources.
///
/// The original workspace path is preserved in metadata.cass.workspace_original
/// for display/audit purposes.
///
/// Part of P6.2 - Apply path mappings at ingest time.
pub fn apply_workspace_rewrite(
    conv: &mut NormalizedConversation,
    workspace_rewrites: &[crate::sources::config::PathMapping],
) {
    // Only apply if we have a workspace and rewrites
    if workspace_rewrites.is_empty() {
        return;
    }

    // Clone workspace upfront to avoid borrow issues
    let original_workspace = match &conv.workspace {
        Some(ws) => ws.to_string_lossy().to_string(),
        None => return,
    };

    // Sort by prefix length descending for longest-prefix match
    let mut mappings: Vec<_> = workspace_rewrites.iter().collect();
    mappings.sort_by(|a, b| b.from.len().cmp(&a.from.len()));

    // Try to apply a mapping
    for mapping in mappings {
        // Optionally filter by agent
        if !mapping.applies_to_agent(Some(&conv.agent_slug)) {
            continue;
        }

        if let Some(rewritten) = mapping.apply(&original_workspace) {
            // Only proceed if the rewrite actually changed something
            if rewritten != original_workspace {
                // Store original in metadata
                if !conv.metadata.is_object() {
                    conv.metadata = serde_json::json!({});
                }

                if let Some(obj) = conv.metadata.as_object_mut() {
                    // Get or create cass object
                    let cass = obj
                        .entry("cass".to_string())
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(cass_obj) = cass.as_object_mut() {
                        cass_obj.insert(
                            "workspace_original".to_string(),
                            serde_json::Value::String(original_workspace.clone()),
                        );
                    }
                }

                // Update workspace to rewritten path
                conv.workspace = Some(std::path::PathBuf::from(&rewritten));

                tracing::debug!(
                    original = %original_workspace,
                    rewritten = %rewritten,
                    agent = %conv.agent_slug,
                    "workspace_rewritten"
                );
            }
            // Stop after first match (longest prefix already matched)
            return;
        }
    }
}

pub mod persist {
    use anyhow::Result;

    use crate::connectors::NormalizedConversation;
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
    use crate::search::tantivy::TantivyIndex;
    use crate::storage::sqlite::{InsertOutcome, SqliteStorage};

    /// Extract provenance (source_id, origin_host) from conversation metadata.
    ///
    /// Looks for `metadata.cass.origin` object with source_id and host fields.
    /// Returns ("local", None) if no provenance is found.
    fn extract_provenance(metadata: &serde_json::Value) -> (String, Option<String>) {
        let source_id = metadata
            .get("cass")
            .and_then(|c| c.get("origin"))
            .and_then(|o| o.get("source_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        let origin_host = metadata
            .get("cass")
            .and_then(|c| c.get("origin"))
            .and_then(|o| o.get("host"))
            .and_then(|v| v.as_str())
            .map(String::from);

        (source_id, origin_host)
    }

    /// Convert a NormalizedConversation to the internal Conversation type for SQLite storage.
    ///
    /// Extracts provenance from `metadata.cass.origin` if present, otherwise defaults to local.
    pub fn map_to_internal(conv: &NormalizedConversation) -> Conversation {
        // Extract provenance from metadata (P2.2)
        let (source_id, origin_host) = extract_provenance(&conv.metadata);

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
            source_id,
            origin_host,
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

    /// Persist multiple conversations in a single database transaction for better performance.
    /// This reduces SQLite transaction overhead when indexing many conversations at once.
    pub fn persist_conversations_batched(
        storage: &mut SqliteStorage,
        t_index: &mut TantivyIndex,
        convs: &[NormalizedConversation],
        force_tantivy_reindex: bool,
    ) -> Result<()> {
        if convs.is_empty() {
            return Ok(());
        }

        // Prepare data for batched insert: (agent_id, workspace_id, Conversation)
        let mut prepared: Vec<(i64, Option<i64>, Conversation)> = Vec::with_capacity(convs.len());

        for conv in convs {
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
            prepared.push((agent_id, workspace_id, internal_conv));
        }

        // Build references for the batched call
        let refs: Vec<(i64, Option<i64>, &Conversation)> =
            prepared.iter().map(|(a, w, c)| (*a, *w, c)).collect();

        // Execute batched insert (single transaction)
        let outcomes = storage.insert_conversations_batched(&refs)?;

        // Add newly inserted messages to Tantivy index
        for (conv, outcome) in convs.iter().zip(outcomes.iter()) {
            if force_tantivy_reindex {
                // Rebuild path: the Tantivy index is known-empty, so index all messages.
                t_index.add_messages(conv, &conv.messages)?;
            } else if !outcome.inserted_indices.is_empty() {
                let new_msgs: Vec<_> = conv
                    .messages
                    .iter()
                    .filter(|m| outcome.inserted_indices.contains(&m.idx))
                    .cloned()
                    .collect();
                t_index.add_messages(conv, &new_msgs)?;
            }
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
    use crate::sources::provenance::SourceKind;
    use rusqlite::Connection;
    use serial_test::serial;
    use tempfile::TempDir;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                // SAFETY: test helper restores prior process env for isolation.
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                // SAFETY: test helper restores prior process env for isolation.
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn ignore_sources_config() -> EnvGuard {
        let key = "CASS_IGNORE_SOURCES_CONFIG";
        let previous = dotenvy::var(key).ok();
        // SAFETY: test helper toggles a process-local env var for isolation.
        unsafe {
            std::env::set_var(key, "1");
        }
        EnvGuard { key, previous }
    }

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
                    source_id: "local".to_string(),
                    origin_host: None,
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
        assert_eq!(storage.schema_version().unwrap(), 5);
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

        let aider = tmp.path().join("repo/.aider.chat.history.md");
        std::fs::create_dir_all(aider.parent().unwrap()).unwrap();
        std::fs::write(&aider, "user\nassistant").unwrap();

        let cursor = tmp.path().join("Cursor/User/globalStorage/state.vscdb");
        std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
        std::fs::write(&cursor, b"").unwrap();

        let chatgpt = tmp
            .path()
            .join("Library/Application Support/com.openai.chat/conversations-abc/data.json");
        std::fs::create_dir_all(chatgpt.parent().unwrap()).unwrap();
        std::fs::write(&chatgpt, "{}").unwrap();

        // roots are needed for classify_paths now
        let roots = vec![
            (ConnectorKind::Codex, tmp.path().join(".codex")),
            (ConnectorKind::Claude, tmp.path().join("project")),
            (ConnectorKind::Aider, tmp.path().join("repo")),
            (ConnectorKind::Cursor, tmp.path().join("Cursor/User")),
            (
                ConnectorKind::ChatGpt,
                tmp.path()
                    .join("Library/Application Support/com.openai.chat"),
            ),
        ];

        let paths = vec![codex.clone(), claude.clone(), aider, cursor, chatgpt];
        let classified = classify_paths(paths, &roots);

        let kinds: std::collections::HashSet<_> = classified.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&ConnectorKind::Codex));
        assert!(kinds.contains(&ConnectorKind::Claude));
        assert!(kinds.contains(&ConnectorKind::Aider));
        assert!(kinds.contains(&ConnectorKind::Cursor));
        assert!(kinds.contains(&ConnectorKind::ChatGpt));

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
        // Use unique subdirectory to avoid conflicts with other tests
        let xdg = tmp.path().join("xdg_watch_state");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = dotenvy::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Use xdg directly (not dirs::data_dir() which doesn't respect XDG_DATA_HOME on macOS)
        let data_dir = xdg.join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Prepare amp fixture under data dir so detection + scan succeed.
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-002.json");
        std::fs::write(
            &amp_file,
            r#"{
  "id": "thread-002",
  "title": "Amp test",
  "messages": [
    {"role":"user","text":"hi","createdAt":1700000000100},
    {"role":"assistant","text":"hello","createdAt":1700000000200}
  ]
}"#,
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

        // Need roots for reindex_paths
        let roots = vec![(ConnectorKind::Amp, amp_dir)];

        reindex_paths(
            &opts,
            vec![amp_file.clone()],
            &roots,
            state.clone(),
            storage.clone(),
            t_index.clone(),
            false,
        )
        .unwrap();

        let loaded = load_watch_state(&data_dir);
        assert!(loaded.contains_key(&ConnectorKind::Amp));
        let ts = loaded.get(&ConnectorKind::Amp).copied().unwrap();
        assert!(ts > 0);

        // Explicitly drop resources to release locks before cleanup
        drop(t_index);
        drop(storage);
        drop(state);

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
                r#"
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
"#,
            )
            .unwrap();
        }
    }

    #[test]
    #[serial]
    fn reindex_paths_updates_progress() {
        let tmp = TempDir::new().unwrap();
        // Use unique subdirectory to avoid conflicts with other tests
        let xdg = tmp.path().join("xdg_progress");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = dotenvy::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Prepare amp fixture using temp directory directly (not dirs::data_dir()
        // which doesn't respect XDG_DATA_HOME on macOS)
        let data_dir = xdg.join("amp");
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

        reindex_paths(
            &opts,
            vec![amp_file],
            &[(ConnectorKind::Amp, amp_dir)],
            state.clone(),
            storage.clone(),
            t_index.clone(),
            false,
        )
        .unwrap();

        // Progress should reflect the indexed conversation
        assert_eq!(progress.total.load(Ordering::Relaxed), 1);
        assert_eq!(progress.current.load(Ordering::Relaxed), 1);
        // Phase resets to 0 (idle) at the end
        assert_eq!(progress.phase.load(Ordering::Relaxed), 0);

        // Explicitly drop resources to release locks before cleanup
        drop(t_index);
        drop(storage);
        drop(state);

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }

    // P2.2 Tests: Multi-root orchestration and provenance injection

    #[test]
    fn inject_provenance_adds_cass_origin_to_metadata() {
        let mut conv = norm_conv(Some("test"), vec![norm_msg(0, 100)]);
        assert!(conv.metadata.get("cass").is_none());

        let origin = Origin::local();
        inject_provenance(&mut conv, &origin);

        let cass = conv.metadata.get("cass").expect("cass field should exist");
        let origin_obj = cass.get("origin").expect("origin should exist");
        assert_eq!(origin_obj.get("source_id").unwrap().as_str(), Some("local"));
        assert_eq!(origin_obj.get("kind").unwrap().as_str(), Some("local"));
    }

    #[test]
    fn inject_provenance_handles_remote_origin() {
        let mut conv = norm_conv(Some("test"), vec![norm_msg(0, 100)]);

        let origin = Origin::remote_with_host("laptop", "user@laptop.local");
        inject_provenance(&mut conv, &origin);

        let cass = conv.metadata.get("cass").expect("cass field should exist");
        let origin_obj = cass.get("origin").expect("origin should exist");
        assert_eq!(
            origin_obj.get("source_id").unwrap().as_str(),
            Some("laptop")
        );
        assert_eq!(origin_obj.get("kind").unwrap().as_str(), Some("ssh"));
        assert_eq!(
            origin_obj.get("host").unwrap().as_str(),
            Some("user@laptop.local")
        );
    }

    #[test]
    fn extract_provenance_returns_local_for_empty_metadata() {
        let conv = persist::map_to_internal(&NormalizedConversation {
            agent_slug: "test".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test"),
            started_at: None,
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![],
        });
        assert_eq!(conv.source_id, "local");
        assert!(conv.origin_host.is_none());
    }

    #[test]
    fn extract_provenance_extracts_remote_origin() {
        let metadata = serde_json::json!({
            "cass": {
                "origin": {
                    "source_id": "laptop",
                    "kind": "ssh",
                    "host": "user@laptop.local"
                }
            }
        });
        let conv = persist::map_to_internal(&NormalizedConversation {
            agent_slug: "test".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test"),
            started_at: None,
            ended_at: None,
            metadata,
            messages: vec![],
        });
        assert_eq!(conv.source_id, "laptop");
        assert_eq!(conv.origin_host, Some("user@laptop.local".to_string()));
    }

    #[test]
    #[serial]
    fn build_scan_roots_creates_local_root() {
        let _guard = ignore_sources_config();
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let roots = build_scan_roots(&storage, &data_dir);

        // Should have at least the local root
        assert!(!roots.is_empty());
        assert_eq!(roots[0].origin.source_id, "local");
        assert!(!roots[0].origin.is_remote());
    }

    #[test]
    #[serial]
    fn build_scan_roots_includes_remote_mirror_if_exists() {
        let _guard = ignore_sources_config();
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Create a remote source in the database
        let db_path = data_dir.join("db.sqlite");
        let storage = SqliteStorage::open(&db_path).unwrap();

        // Register a remote source
        storage
            .upsert_source(&crate::sources::provenance::Source {
                id: "laptop".to_string(),
                kind: SourceKind::Ssh,
                host_label: Some("user@laptop.local".to_string()),
                machine_id: None,
                platform: Some("linux".to_string()),
                config_json: None,
                created_at: None,
                updated_at: None,
            })
            .unwrap();

        // Create the mirror directory
        let mirror_dir = data_dir.join("remotes").join("laptop").join("mirror");
        std::fs::create_dir_all(&mirror_dir).unwrap();

        let roots = build_scan_roots(&storage, &data_dir);

        // Should have local root + remote root
        assert_eq!(roots.len(), 2);

        // Find the remote root
        let remote_root = roots.iter().find(|r| r.origin.source_id == "laptop");
        assert!(remote_root.is_some());
        let remote_root = remote_root.unwrap();
        assert!(remote_root.origin.is_remote());
        assert_eq!(
            remote_root.origin.host,
            Some("user@laptop.local".to_string())
        );
        assert_eq!(remote_root.platform, Some(Platform::Linux));
    }

    #[test]
    #[serial]
    fn build_scan_roots_skips_nonexistent_mirror() {
        let _guard = ignore_sources_config();
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = SqliteStorage::open(&db_path).unwrap();

        // Register a remote source but don't create mirror directory
        storage
            .upsert_source(&crate::sources::provenance::Source {
                id: "nonexistent".to_string(),
                kind: SourceKind::Ssh,
                host_label: Some("user@host".to_string()),
                machine_id: None,
                platform: None,
                config_json: None,
                created_at: None,
                updated_at: None,
            })
            .unwrap();

        let roots = build_scan_roots(&storage, &data_dir);

        // Should only have local root (remote skipped because mirror doesn't exist)
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].origin.source_id, "local");
    }

    #[test]
    fn apply_workspace_rewrite_no_rewrites() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/projects/app"));

        apply_workspace_rewrite(&mut conv, &[]);

        // Workspace unchanged when no rewrites
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/home/user/projects/app"))
        );
        // No workspace_original in metadata
        assert!(
            conv.metadata
                .get("cass")
                .and_then(|c| c.get("workspace_original"))
                .is_none()
        );
    }

    #[test]
    fn apply_workspace_rewrite_no_workspace() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = None;

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        apply_workspace_rewrite(&mut conv, &mappings);

        // Still None
        assert!(conv.workspace.is_none());
    }

    #[test]
    fn apply_workspace_rewrite_applies_mapping() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/projects/app"));

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        apply_workspace_rewrite(&mut conv, &mappings);

        // Workspace rewritten
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/Users/me/projects/app"))
        );

        // Original stored in metadata
        let workspace_original = conv
            .metadata
            .get("cass")
            .and_then(|c| c.get("workspace_original"))
            .and_then(|v| v.as_str());
        assert_eq!(workspace_original, Some("/home/user/projects/app"));
    }

    #[test]
    fn apply_workspace_rewrite_longest_prefix_match() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/projects/special/app"));

        let mappings = vec![
            crate::sources::config::PathMapping::new("/home/user", "/Users/me"),
            crate::sources::config::PathMapping::new(
                "/home/user/projects/special",
                "/Volumes/Special",
            ),
        ];

        apply_workspace_rewrite(&mut conv, &mappings);

        // Should use longer prefix match
        assert_eq!(conv.workspace, Some(PathBuf::from("/Volumes/Special/app")));
    }

    #[test]
    fn apply_workspace_rewrite_no_match() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/opt/other/path"));

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        apply_workspace_rewrite(&mut conv, &mappings);

        // Workspace unchanged - no matching prefix
        assert_eq!(conv.workspace, Some(PathBuf::from("/opt/other/path")));
        // No workspace_original since nothing was rewritten
        assert!(
            conv.metadata
                .get("cass")
                .and_then(|c| c.get("workspace_original"))
                .is_none()
        );
    }

    #[test]
    fn apply_workspace_rewrite_with_agent_filter() {
        // Test with agent filter
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.agent_slug = "claude-code".to_string();
        conv.workspace = Some(PathBuf::from("/home/user/projects/app"));

        let mappings = vec![
            crate::sources::config::PathMapping::new("/home/user", "/Users/me"),
            crate::sources::config::PathMapping::with_agents(
                "/home/user/projects",
                "/Volumes/Work",
                vec!["cursor".to_string()], // Only for cursor, not claude-code
            ),
        ];

        apply_workspace_rewrite(&mut conv, &mappings);

        // Should NOT use cursor-specific mapping, falls back to general
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/Users/me/projects/app"))
        );
    }

    #[test]
    fn apply_workspace_rewrite_preserves_existing_metadata() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/app"));
        conv.metadata = serde_json::json!({
            "cass": {
                "origin": {
                    "source_id": "laptop",
                    "kind": "ssh",
                    "host": "user@laptop.local"
                }
            }
        });

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        apply_workspace_rewrite(&mut conv, &mappings);

        // Origin preserved
        assert_eq!(
            conv.metadata["cass"]["origin"]["source_id"].as_str(),
            Some("laptop")
        );
        // workspace_original added
        assert_eq!(
            conv.metadata["cass"]["workspace_original"].as_str(),
            Some("/home/user/app")
        );
    }
}
