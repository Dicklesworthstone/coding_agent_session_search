//! Connectors for agent histories.
//!
//! All connector implementations live in `franken_agent_detection`.
//! This module provides re-export stubs for backward-compatible import paths.

// Re-export normalized types and connector infrastructure from franken_agent_detection.
pub use franken_agent_detection::{
    Connector,
    DetectionResult,
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
