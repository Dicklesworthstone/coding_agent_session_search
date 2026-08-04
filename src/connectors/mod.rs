//! Connectors for agent histories.
//!
//! Most connector implementations live in `franken_agent_detection`.
//! This module provides re-export stubs plus the CASS-specific Codex enrichment
//! wrapper used by every connector factory exposed to the indexer.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// Re-export normalized types and connector infrastructure from franken_agent_detection.
pub use franken_agent_detection::{
    Connector,
    DetectionResult,
    DiscoveredSourceFile,
    DiscoveredSourceRole,
    ExtractedTokenUsage,
    LOCAL_SOURCE_ID,
    ModelInfo,
    // Scan & provenance types
    NormalizedConversation,
    NormalizedMessage,
    NormalizedSnippet,
    Origin,
    PathMapping,
    // Connector infrastructure
    PathTrie,
    Platform,
    ScanContext,
    ScanRoot,
    SourceKind,
    TokenDataSource,
    WorkspaceCache,
    estimate_tokens_from_content,
    extract_claude_code_tokens,
    extract_codex_tokens,
    extract_tokens_for_agent,
    file_modified_since,
    flatten_content,
    franken_detection_for_connector,
    normalize_model,
    parse_timestamp,
    reindex_messages,
};

/// Result of a Codex scan-root preflight. The preflight replaces directory
/// roots with explicit rollout files while preserving each root's provenance
/// and workspace rewrite metadata.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct CodexScanPreflight {
    pub scan_roots: Vec<ScanRoot>,
    pub original_roots: usize,
    pub explicit_file_roots: usize,
    pub fallback_roots: usize,
}

/// Expand Codex directory roots into explicit rollout-file roots where doing so
/// preserves Codex's session-relative external IDs.
///
/// Parent directories that contain a `.codex` child fall back to the original
/// directory root: `franken_agent_detection` includes `.codex/sessions/...` in
/// the external ID from that shape, while explicit file roots make the ID
/// relative to `sessions/`. Unreadable or ambiguous roots similarly fall back
/// so the connector's existing behavior remains the source of truth.
#[doc(hidden)]
#[must_use]
pub fn preflight_codex_explicit_file_roots(
    roots: &[ScanRoot],
    since_ts: Option<i64>,
) -> CodexScanPreflight {
    let mut scan_roots = Vec::new();
    let mut explicit_file_roots = 0usize;
    let mut fallback_roots = 0usize;

    for root in roots {
        if root.path.is_file() {
            if is_codex_rollout_file(&root.path) {
                explicit_file_roots = explicit_file_roots.saturating_add(1);
            }
            scan_roots.push(root.clone());
            continue;
        }

        match codex_explicit_file_roots_for_root(root, since_ts) {
            Ok(expanded) => {
                explicit_file_roots = explicit_file_roots.saturating_add(expanded.len());
                scan_roots.extend(expanded);
            }
            Err(_) => {
                fallback_roots = fallback_roots.saturating_add(1);
                scan_roots.push(root.clone());
            }
        }
    }

    CodexScanPreflight {
        scan_roots,
        original_roots: roots.len(),
        explicit_file_roots,
        fallback_roots,
    }
}

fn codex_explicit_file_roots_for_root(
    root: &ScanRoot,
    since_ts: Option<i64>,
) -> io::Result<Vec<ScanRoot>> {
    if !is_under_codex_dir(&root.path) && root.path.join(".codex").exists() {
        return Err(io::Error::other(
            "parent codex roots keep directory scan to preserve external IDs",
        ));
    }

    let sessions = codex_sessions_dir(&root.path);
    if sessions == root.path
        && root
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name != "sessions")
    {
        return Err(io::Error::other(
            "roots without a sessions directory keep directory scan to preserve external IDs",
        ));
    }

    let files = collect_codex_rollout_files(&sessions, since_ts)?;

    Ok(files
        .into_iter()
        .map(|path| {
            let mut file_root = root.clone();
            file_root.path = path;
            file_root
        })
        .collect())
}

fn is_under_codex_dir(path: &Path) -> bool {
    path.ancestors().any(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == ".codex")
    })
}

fn codex_sessions_dir(home: &Path) -> PathBuf {
    let sessions = home.join("sessions");
    if sessions.exists() {
        sessions
    } else {
        home.to_path_buf()
    }
}

fn collect_codex_rollout_files(sessions: &Path, since_ts: Option<i64>) -> io::Result<Vec<PathBuf>> {
    if !sessions.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let mut pending_dirs = vec![sessions.to_path_buf()];
    while let Some(dir) = pending_dirs.pop() {
        let mut entries = fs::read_dir(&dir)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                pending_dirs.push(path);
            } else if file_type.is_file()
                && is_codex_rollout_file(&path)
                && file_modified_since(&path, since_ts)
            {
                files.push(path);
            }
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

fn is_codex_rollout_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with("rollout-")
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("json")
            })
}

// Connector re-export stubs — each module file re-exports from FAD.
pub mod aider;
pub mod amp;
pub mod antigravity;
pub mod chatgpt;
pub mod claude_code;
pub mod clawdbot;
pub mod cline;
pub mod codex;
pub mod copilot;
pub mod copilot_cli;
pub mod crush;
pub mod cursor;
pub mod factory;
pub mod gemini;
pub mod goose;
pub mod grok;
pub mod hermes;
pub mod kimi;
pub mod openclaw;
pub mod opencode;
pub mod openhands;
pub mod pi_agent;
pub mod qwen;
pub mod vibe;

/// Constructor function used by the runtime connector registry.
pub type ConnectorFactory = fn() -> Box<dyn Connector + Send>;

fn codex_connector_factory() -> Box<dyn Connector + Send> {
    Box::new(codex::CodexConnector::new())
}

/// Return connector factories with CASS-specific wrappers applied.
///
/// Non-Codex factories remain exactly the upstream FAD factories. Codex must
/// pass through CASS's enrichment wrapper so modern `function_call` arguments
/// reach the production indexer instead of remaining placeholder-only content.
#[must_use]
pub fn get_connector_factories() -> Vec<(&'static str, ConnectorFactory)> {
    franken_agent_detection::get_connector_factories()
        .into_iter()
        .map(|(name, factory)| {
            if name == "codex" {
                (name, codex_connector_factory as ConnectorFactory)
            } else {
                (name, factory)
            }
        })
        .collect()
}

/// gh373 third variant (bead oeu5a): ambient work-liveness sink for
/// connector-internal parse loops.
///
/// The #332 activity tick fires only once per COMPLETED conversation, so a
/// connector spending minutes inside the line loop of one giant source file
/// (observed: 60-90MB codex rollouts, ~40k lines) starves the stall
/// watchdog's counters and draws a spurious exit(70). Connector code has no
/// `IndexingProgress` handle — `ScanContext` lives in the pinned external
/// `franken_agent_detection` crate — so the indexer registers a tick closure
/// here for the duration of a run and deep parse loops call
/// [`scan_activity::tick`] every N lines.
///
/// Multiple concurrent registrations are supported (in-process tests):
/// `tick` fans out to every live sink — an extra tick can only delay a false
/// stall report, never corrupt state. Registration is cheap; `tick` takes a
/// mutex only once per stride (default: caller ticks every ~1024 lines).
pub mod scan_activity {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Registered liveness sinks: `(registration id, tick callback)`.
    type SinkRegistry = Vec<(u64, Box<dyn Fn() + Send + Sync>)>;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    static SINKS: Mutex<SinkRegistry> = Mutex::new(Vec::new());

    /// Unregisters its sink on drop. Hold for the duration of an index run.
    pub struct ScanActivityGuard {
        id: u64,
    }

    pub fn register(tick: Box<dyn Fn() + Send + Sync>) -> ScanActivityGuard {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut sinks) = SINKS.lock() {
            sinks.push((id, tick));
        }
        ScanActivityGuard { id }
    }

    impl Drop for ScanActivityGuard {
        fn drop(&mut self) {
            if let Ok(mut sinks) = SINKS.lock() {
                sinks.retain(|(id, _)| *id != self.id);
            }
        }
    }

    /// Record connector-internal parse liveness. No-op when no index run has
    /// registered a sink (e.g. connector unit tests).
    pub fn tick() {
        if let Ok(sinks) = SINKS.lock() {
            for (_, sink) in sinks.iter() {
                sink();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        #[test]
        fn tick_reaches_registered_sink_and_stops_after_guard_drop() {
            let count = Arc::new(AtomicUsize::new(0));
            let sink_count = Arc::clone(&count);
            let guard = register(Box::new(move || {
                sink_count.fetch_add(1, Ordering::Relaxed);
            }));
            tick();
            tick();
            assert_eq!(count.load(Ordering::Relaxed), 2);
            drop(guard);
            tick();
            assert_eq!(
                count.load(Ordering::Relaxed),
                2,
                "a dropped guard's sink must not receive further ticks"
            );
        }
    }
}
