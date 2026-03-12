// src/monitor/state.rs
//! Agent state types for live monitoring.

use serde::Serialize;
use std::fmt;
use std::path::PathBuf;

/// State of an active Claude Code agent, derived from JSONL session tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentState {
    /// Last entry is assistant with stop_reason: null (still streaming)
    Working,
    /// Progress entry — hook or tool executing
    ToolRunning,
    /// tool_use emitted, no tool_result yet, stale >2s
    WaitingPermission,
    /// Complete assistant turn, no subsequent user message, stale >5s
    WaitingInput,
    /// queue-operation: enqueue present
    Queued,
    /// Process alive but no JSONL writes >30s
    Idle,
    /// Process running but no JSONL file found yet
    Starting,
}

impl AgentState {
    /// Lower number = higher urgency. Used for sorting the dashboard table.
    pub fn priority(&self) -> u8 {
        match self {
            Self::WaitingInput => 0,
            Self::WaitingPermission => 1,
            Self::Queued => 2,
            Self::Working => 3,
            Self::ToolRunning => 4,
            Self::Starting => 5,
            Self::Idle => 6,
        }
    }

    /// Whether this state requires user attention (flashing in TUI).
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::WaitingInput | Self::WaitingPermission)
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Working => write!(f, "WORKING"),
            Self::ToolRunning => write!(f, "TOOL RUNNING"),
            Self::WaitingPermission => write!(f, "PERMISSION"),
            Self::WaitingInput => write!(f, "NEEDS INPUT"),
            Self::Queued => write!(f, "QUEUED"),
            Self::Idle => write!(f, "IDLE"),
            Self::Starting => write!(f, "STARTING"),
        }
    }
}

/// How the Claude Code instance was launched re: permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    DangerouslySkip,
    AllowDangerouslySkip,
}

impl PermissionMode {
    /// Parse from process command-line arguments.
    pub fn from_args(args: &[&str]) -> Self {
        if args.iter().any(|a| *a == "--dangerously-skip-permissions") {
            Self::DangerouslySkip
        } else if args
            .iter()
            .any(|a| *a == "--allow-dangerously-skip-permissions")
        {
            Self::AllowDangerouslySkip
        } else {
            Self::Default
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::DangerouslySkip => write!(f, "yolo"),
            Self::AllowDangerouslySkip => write!(f, "allow-yolo"),
        }
    }
}

/// Session telemetry derived from full JSONL scan of usage data.
#[derive(Debug, Clone, Serialize)]
pub struct SessionTelemetry {
    /// Total context window tokens (input + cache_creation + cache_read)
    pub context_tokens: u64,
    /// Context pressure as percentage (context_tokens / context_max * 100)
    pub context_pressure_pct: u8,
    /// Model's max context window
    pub context_max: u64,
    /// Total output tokens generated this session
    pub total_output_tokens: u64,
    /// Number of assistant turns (conversation depth)
    pub turn_count: u32,
    /// Context growth rate: tokens gained per minute (last 5 turns)
    pub burn_rate_per_min: u64,
    /// Tool usage counts (top tools, sorted by frequency descending)
    pub tool_mix: Vec<(String, u32)>,
    /// Session start timestamp (ISO8601)
    pub session_start: Option<String>,
    /// Whether messages are queued (queue-operation: enqueue seen)
    pub has_queued_messages: bool,
}

/// Context extracted from the session JSONL for display in the detail pane.
#[derive(Debug, Clone, Serialize)]
pub struct SessionContext {
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
    pub session_id: Option<String>,
    /// Recent activity entries (most recent first), summarized for display.
    pub recent_activity: Vec<ActivityEntry>,
    /// Telemetry data from full JSONL scan (usage, burn rate, tool mix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<SessionTelemetry>,
}

/// A single summarized activity entry from the JSONL log.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub timestamp: String,
    pub kind: String,
    pub summary: String,
}

/// A discovered Claude Code agent instance with derived state.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInstance {
    pub pid: u32,
    pub tty: String,
    pub cwd: PathBuf,
    pub project_name: String,
    pub state: AgentState,
    pub permission_mode: PermissionMode,
    pub age_secs: u64,
    pub last_activity_secs: u64,
    pub session_context: Option<SessionContext>,
    /// Whether this is a subagent spawned by a parent session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_subagent: bool,
    /// Session ID of the parent agent (if this is a subagent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Team name this agent belongs to (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    /// Role within the team (e.g., "researcher", "test-runner").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    /// Short slug identifying this subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_slug: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_display() {
        assert_eq!(AgentState::Working.to_string(), "WORKING");
        assert_eq!(AgentState::WaitingInput.to_string(), "NEEDS INPUT");
        assert_eq!(AgentState::WaitingPermission.to_string(), "PERMISSION");
    }

    #[test]
    fn agent_state_priority_order() {
        assert!(AgentState::WaitingInput.priority() < AgentState::WaitingPermission.priority());
        assert!(AgentState::WaitingPermission.priority() < AgentState::Working.priority());
        assert!(AgentState::Working.priority() < AgentState::Idle.priority());
    }

    #[test]
    fn agent_state_needs_attention() {
        assert!(AgentState::WaitingInput.needs_attention());
        assert!(AgentState::WaitingPermission.needs_attention());
        assert!(!AgentState::Working.needs_attention());
        assert!(!AgentState::Idle.needs_attention());
    }

    #[test]
    fn permission_mode_from_args() {
        assert_eq!(
            PermissionMode::from_args(&["claude", "--dangerously-skip-permissions"]),
            PermissionMode::DangerouslySkip
        );
        assert_eq!(
            PermissionMode::from_args(&["claude", "--allow-dangerously-skip-permissions"]),
            PermissionMode::AllowDangerouslySkip
        );
        assert_eq!(PermissionMode::from_args(&["claude"]), PermissionMode::Default);
    }
}
