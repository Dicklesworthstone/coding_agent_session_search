//! Live monitoring of active Claude Code instances.
//!
//! Discovers running `claude` processes via the process table,
//! tails their JSONL session files, derives agent state, and
//! renders a dashboard (ftui TUI or streaming JSON).

pub mod discovery;
pub mod session;
pub mod state;
pub mod tui;
