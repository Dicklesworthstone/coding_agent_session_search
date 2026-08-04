//! Connector for Goose session storage.
//!
//! Goose (Block's open-source agent) stores sessions in a SQLite database at
//! `~/.local/share/goose/sessions/sessions.db` from v1.20 onward, with a
//! per-session `*.jsonl` layout (including the legacy `~/.goose/sessions/`
//! location) used by earlier releases. Both layouts are handled upstream.
//!
//! Implementation lives in `franken_agent_detection::connectors::goose`.

pub use franken_agent_detection::connectors::goose::GooseConnector;
