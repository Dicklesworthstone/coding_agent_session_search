//! Connector for Grok Build (xAI's official `grok` coding CLI) sessions.
//!
//! Implementation lives in `franken_agent_detection::connectors::grok`.
//! Layout: `$GROK_HOME/sessions/<percent-encoded-cwd>/<session-uuid>/` with
//! `updates.jsonl` (authoritative ACP session-update stream), `summary.json`
//! (metadata), and `chat_history.jsonl` (raw model history, fallback).

pub use franken_agent_detection::GrokConnector;
