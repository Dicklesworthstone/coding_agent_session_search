//! Connectors for agent histories.
//!
//! All connector implementations live in `franken_agent_detection`.
//! This module provides re-export stubs for backward-compatible import paths.

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
    get_connector_factories,
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
pub mod hermes;
pub mod kimi;
pub mod openclaw;
pub mod opencode;
pub mod pi_agent;
pub mod qwen;
pub mod vibe;
