//! Live monitoring of active Claude Code instances.
//!
//! Discovers running `claude` processes via the process table,
//! tails their JSONL session files, derives agent state, and
//! renders a dashboard (ftui TUI or streaming JSON).

pub mod discovery;
pub mod session;
pub mod state;
pub mod tui;

use crate::CliError;
use state::AgentInstance;

/// Collect a single snapshot of all active Claude Code agents.
pub fn collect_snapshot() -> Vec<AgentInstance> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    let claude_projects_dir = home.join(".claude").join("projects");

    let own_pid = std::process::id();
    let mut agents_with_paths = discovery::discover_agents(&claude_projects_dir, own_pid);

    // Discover team configurations
    let teams = discovery::discover_teams(&home);

    // Derive state from session files for each agent
    for (agent, session_path) in &mut agents_with_paths {
        if let Some(path) = session_path {
            match session::derive_state(path) {
                Ok((derived_state, mut ctx)) => {
                    // Attach telemetry from full JSONL scan
                    if let Some(ref mut c) = ctx {
                        c.telemetry = session::derive_telemetry(path);
                    }
                    agent.state = derived_state;
                    agent.session_context = ctx;
                    agent.last_activity_secs = session::file_staleness_secs(path);

                    // Promote to Idle if session file is stale (>30s)
                    if agent.last_activity_secs > 30
                        && !matches!(
                            agent.state,
                            state::AgentState::WaitingInput
                                | state::AgentState::WaitingPermission
                                | state::AgentState::Queued
                        )
                    {
                        agent.state = state::AgentState::Idle;
                    }
                }
                Err(_) => {
                    // Can't read session file — leave as Starting
                }
            }
        }

        // Annotate with team info by matching CWD to team member CWDs
        let cwd_str = agent.cwd.to_string_lossy().to_string();
        for team in teams.values() {
            for member in &team.members {
                if let Some(ref member_cwd) = member.cwd {
                    if cwd_str == *member_cwd {
                        agent.team_name = Some(team.name.clone());
                        agent.agent_role = Some(member.agent_type.clone());
                        agent.agent_slug = Some(member.name.clone());
                        break;
                    }
                }
            }
            if agent.team_name.is_some() {
                break;
            }
        }
    }

    // Collect parent agents and discover subagents
    let mut agents: Vec<AgentInstance> = Vec::new();

    for (agent, session_path) in agents_with_paths {
        // For each parent agent with a session_id, try to discover subagents
        let mut subagent_instances = Vec::new();
        if let Some(ref ctx) = agent.session_context {
            if let Some(ref session_id) = ctx.session_id {
                let project_key =
                    discovery::cwd_to_project_key(agent.cwd.to_str().unwrap_or(""));
                let subagent_files = discovery::discover_subagents(
                    &claude_projects_dir,
                    &project_key,
                    session_id,
                    60, // 60s staleness threshold
                );

                for sub in subagent_files {
                    let sub_cwd = sub.cwd.unwrap_or_else(|| agent.cwd.clone());
                    let sub_project = discovery::extract_project_name(
                        sub_cwd.to_str().unwrap_or(""),
                    );

                    // Try to derive state from the subagent's JSONL
                    let (sub_state, sub_ctx, sub_staleness) =
                        match session::derive_state(&sub.session_path) {
                            Ok((s, mut c)) => {
                                // Attach telemetry for subagents too
                                if let Some(ref mut ctx) = c {
                                    ctx.telemetry =
                                        session::derive_telemetry(&sub.session_path);
                                }
                                let staleness =
                                    session::file_staleness_secs(&sub.session_path);
                                let final_state = if staleness > 30
                                    && !matches!(
                                        s,
                                        state::AgentState::WaitingInput
                                            | state::AgentState::WaitingPermission
                                            | state::AgentState::Queued
                                    ) {
                                    state::AgentState::Idle
                                } else {
                                    s
                                };
                                (final_state, c, staleness)
                            }
                            Err(_) => (state::AgentState::Starting, None, 0),
                        };

                    // Check if this subagent is a team member
                    let sub_cwd_str = sub_cwd.to_string_lossy().to_string();
                    let mut sub_team_name = None;
                    let mut sub_agent_role = None;
                    let mut sub_agent_slug_from_team = None;
                    for team in teams.values() {
                        for member in &team.members {
                            if let Some(ref member_cwd) = member.cwd {
                                if sub_cwd_str == *member_cwd {
                                    sub_team_name = Some(team.name.clone());
                                    sub_agent_role = Some(member.agent_type.clone());
                                    sub_agent_slug_from_team = Some(member.name.clone());
                                    break;
                                }
                            }
                        }
                        if sub_team_name.is_some() {
                            break;
                        }
                    }

                    let sub_instance = AgentInstance {
                        pid: 0, // Subagents may not have a discoverable PID
                        tty: String::new(),
                        cwd: sub_cwd,
                        project_name: sub_project,
                        state: sub_state,
                        permission_mode: state::PermissionMode::Default,
                        age_secs: 0,
                        last_activity_secs: sub_staleness,
                        session_context: sub_ctx,
                        is_subagent: true,
                        parent_session_id: Some(session_id.clone()),
                        team_name: sub_team_name,
                        agent_role: sub_agent_role,
                        agent_slug: sub_agent_slug_from_team.or(sub.slug),
                    };

                    subagent_instances.push(sub_instance);
                }
            }
        }

        // Also check if the session path points inside a subagents/ dir
        // (should not happen for parent agents, but defensive)
        let _ = session_path; // consumed

        agents.push(agent);
        // Group subagents immediately after their parent
        agents.extend(subagent_instances);
    }

    // Stable sort: parents by priority, subagents stay after their parent
    // We partition into groups (parent + its subagents) then sort groups
    let mut groups: Vec<Vec<AgentInstance>> = Vec::new();
    let mut current_group: Vec<AgentInstance> = Vec::new();

    for agent in agents {
        if !agent.is_subagent {
            if !current_group.is_empty() {
                groups.push(std::mem::take(&mut current_group));
            }
            current_group.push(agent);
        } else {
            current_group.push(agent);
        }
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    // Sort groups by the parent's priority
    groups.sort_by_key(|g| g.first().map(|a| a.state.priority()).unwrap_or(255));

    groups.into_iter().flatten().collect()
}

/// Format a duration in seconds as a human-readable age string.
fn format_age(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    } else if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

/// Entry point for `cass monitor`.
pub fn run_monitor(json: bool, interval: u64, once: bool) -> Result<(), CliError> {
    if json {
        run_monitor_json(interval, once)
    } else if once {
        run_monitor_table(interval, once)
    } else {
        tui::run_monitor_tui(interval)
    }
}

/// Streaming JSON output mode: one JSON snapshot per line.
fn run_monitor_json(interval: u64, once: bool) -> Result<(), CliError> {
    loop {
        let agents = collect_snapshot();
        let json_str = serde_json::to_string(&agents).map_err(|e| CliError {
            code: 9,
            kind: "monitor",
            message: format!("JSON serialization failed: {e}"),
            hint: None,
            retryable: false,
        })?;
        println!("{json_str}");

        if once {
            return Ok(());
        }

        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

/// Table output mode: prints a formatted table to the terminal.
fn run_monitor_table(interval: u64, once: bool) -> Result<(), CliError> {
    loop {
        let agents = collect_snapshot();

        // Clear screen (ANSI escape) unless --once
        if !once {
            print!("\x1b[2J\x1b[H");
        }

        // Header
        println!(
            "\x1b[1;36m╔══════════════════════════════════════════════════════════════╗\x1b[0m"
        );
        println!(
            "\x1b[1;36m║\x1b[0m  \x1b[1;37m⚡ CASS MONITOR\x1b[0m — {} agent(s) active{}\x1b[1;36m║\x1b[0m",
            agents.len(),
            " ".repeat(62usize.saturating_sub(30 + count_digits(agents.len())))
        );
        println!(
            "\x1b[1;36m╚══════════════════════════════════════════════════════════════╝\x1b[0m"
        );
        println!();

        if agents.is_empty() {
            println!("  No active Claude Code instances found.");
            println!();
            println!("  \x1b[2mLooking for `claude` processes with JSONL session files.\x1b[0m");
        } else {
            // Column headers
            println!(
                "  \x1b[1;37m{:<20} {:<16} {:<8} {:<8} {:<10}\x1b[0m",
                "PROJECT", "STATE", "AGE", "MODE", "PID"
            );
            println!("  {}", "─".repeat(58));

            for agent in &agents {
                let state_str = format_state_colored(&agent.state);
                let mode_str = match agent.permission_mode {
                    state::PermissionMode::DangerouslySkip => "\x1b[31myolo\x1b[0m",
                    state::PermissionMode::AllowDangerouslySkip => "\x1b[33mallow\x1b[0m",
                    state::PermissionMode::Default => "default",
                };

                println!(
                    "  {:<20} {:<26} {:<8} {:<18} {}",
                    truncate(&agent.project_name, 20),
                    state_str,
                    format_age(agent.age_secs),
                    mode_str,
                    agent.pid,
                );
            }
        }

        println!();

        if once {
            return Ok(());
        }

        println!(
            "  \x1b[2mRefreshing every {}s · Ctrl-C to quit\x1b[0m",
            interval
        );
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn format_state_colored(state: &state::AgentState) -> String {
    match state {
        state::AgentState::WaitingInput => "\x1b[1;33;5m⚠ NEEDS INPUT\x1b[0m".to_string(),
        state::AgentState::WaitingPermission => "\x1b[1;31;5m🔒 PERMISSION\x1b[0m".to_string(),
        state::AgentState::Working => "\x1b[1;32m⚙ WORKING\x1b[0m".to_string(),
        state::AgentState::ToolRunning => "\x1b[1;32m🔧 TOOL RUNNING\x1b[0m".to_string(),
        state::AgentState::Queued => "\x1b[1;35m⏳ QUEUED\x1b[0m".to_string(),
        state::AgentState::Idle => "\x1b[2m💤 IDLE\x1b[0m".to_string(),
        state::AgentState::Starting => "\x1b[2m🚀 STARTING\x1b[0m".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max.saturating_sub(2))
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}..", &s[..end])
    }
}

fn count_digits(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        (n as f64).log10().floor() as usize + 1
    }
}
