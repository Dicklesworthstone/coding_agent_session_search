//! ftui-based live monitoring dashboard for active Claude Code instances.
//!
//! Renders a split-pane TUI: agent table on the left, detail pane on the right.
//! Refreshes every N seconds via background task. Keyboard-navigable.

use std::collections::HashSet;
use std::time::Duration;

use ftui::layout::{Constraint, Flex};
use ftui::render::cell::{CellContent, PackedRgba};
use ftui::{Cmd, Event, Frame, KeyCode, KeyEvent, Model, Program, ProgramConfig, Style, TaskSpec};

use ftui::core::geometry::Rect;

use crate::monitor::state::{AgentInstance, AgentState, PermissionMode};

// ─── Spinner ──────────────────────────────────────────────────────────────

const BRAILLE_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// ─── Filter ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    All,
    NeedsAttention,
    Working,
    Idle,
    Teams,
}

impl FilterMode {
    fn next(self) -> Self {
        match self {
            Self::All => Self::NeedsAttention,
            Self::NeedsAttention => Self::Working,
            Self::Working => Self::Idle,
            Self::Idle => Self::Teams,
            Self::Teams => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::NeedsAttention => "Attention",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Teams => "Teams",
        }
    }

    fn matches_agent(self, agent: &AgentInstance) -> bool {
        match self {
            Self::All => true,
            Self::NeedsAttention => agent.state.needs_attention(),
            Self::Working => {
                matches!(agent.state, AgentState::Working | AgentState::ToolRunning)
            }
            Self::Idle => matches!(agent.state, AgentState::Idle | AgentState::Starting),
            Self::Teams => agent.team_name.is_some() || agent.is_subagent,
        }
    }
}

// ─── Colors ────────────────────────────────────────────────────────────────

const CYAN: PackedRgba = PackedRgba::rgb(0, 215, 255);
const GREEN: PackedRgba = PackedRgba::rgb(0, 255, 128);
const YELLOW: PackedRgba = PackedRgba::rgb(255, 215, 0);
const RED: PackedRgba = PackedRgba::rgb(255, 80, 80);
const MAGENTA: PackedRgba = PackedRgba::rgb(200, 100, 255);
const DIM: PackedRgba = PackedRgba::rgb(100, 100, 100);
const WHITE: PackedRgba = PackedRgba::rgb(230, 230, 230);
const BRIGHT_WHITE: PackedRgba = PackedRgba::rgb(255, 255, 255);
const DARK_BG: PackedRgba = PackedRgba::rgb(20, 20, 30);
const SELECTED_BG: PackedRgba = PackedRgba::rgb(40, 40, 70);
const HEADER_BG: PackedRgba = PackedRgba::rgb(15, 15, 25);

// ─── Header ──────────────────────────────────────────────────────────────

const LOGO_LINE: &str = " ▄▀▀ ▄▀▄ ▄▀▀ ▄▀▀";
const LOGO_LINE2: &str = " ▀▄▄ █▀█ ▄██ ▄██";
const SUBTITLE: &str = "  M O N I T O R";

// ─── Model ─────────────────────────────────────────────────────────────────

pub struct MonitorApp {
    agents: Vec<AgentInstance>,
    selected: usize,
    tick_count: u64,
    interval_secs: u64,
    prev_attention_pids: HashSet<u32>,
    filter: FilterMode,
    detail_scroll: usize,
}

impl MonitorApp {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            agents: Vec::new(),
            selected: 0,
            tick_count: 0,
            interval_secs,
            prev_attention_pids: HashSet::new(),
            filter: FilterMode::All,
            detail_scroll: 0,
        }
    }

    fn filtered_agents(&self) -> Vec<&AgentInstance> {
        self.agents
            .iter()
            .filter(|a| self.filter.matches_agent(a))
            .collect()
    }
}

// ─── Messages ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum MonitorMsg {
    Tick,
    Refreshed(Vec<AgentInstance>),
    SelectNext,
    SelectPrev,
    SelectFirst,
    SelectLast,
    CycleFilter,
    DetailScrollUp,
    DetailScrollDown,
    OpenEditor,
    Quit,
    Noop,
}

impl From<Event> for MonitorMsg {
    fn from(event: Event) -> Self {
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Escape,
                ..
            }) => Self::Quit,

            Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Down, ..
            }) => Self::SelectNext,

            Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) => Self::SelectPrev,

            Event::Key(KeyEvent {
                code: KeyCode::Char('g'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Home, ..
            }) => Self::SelectFirst,

            Event::Key(KeyEvent {
                code: KeyCode::Char('G'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::End, ..
            }) => Self::SelectLast,

            Event::Key(KeyEvent {
                code: KeyCode::Char('f'),
                ..
            }) => Self::CycleFilter,

            Event::Key(KeyEvent {
                code: KeyCode::PageUp,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('+'),
                ..
            }) => Self::DetailScrollUp,

            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('-'),
                ..
            }) => Self::DetailScrollDown,

            Event::Key(KeyEvent {
                code: KeyCode::Char('e'),
                ..
            }) => Self::OpenEditor,

            Event::Tick => Self::Tick,
            _ => Self::Noop,
        }
    }
}

// ─── Model impl ────────────────────────────────────────────────────────────

impl Model for MonitorApp {
    type Message = MonitorMsg;

    fn init(&mut self) -> Cmd<MonitorMsg> {
        // Immediately refresh, then schedule periodic ticks
        Cmd::Batch(vec![
            Cmd::Task(
                TaskSpec::default(),
                Box::new(|| {
                    let agents = crate::monitor::collect_snapshot();
                    MonitorMsg::Refreshed(agents)
                }),
            ),
            Cmd::Tick(Duration::from_secs(self.interval_secs)),
        ])
    }

    fn update(&mut self, msg: MonitorMsg) -> Cmd<MonitorMsg> {
        match msg {
            MonitorMsg::Tick => {
                self.tick_count += 1;
                Cmd::Batch(vec![
                    Cmd::Task(
                        TaskSpec::default(),
                        Box::new(|| {
                            let agents = crate::monitor::collect_snapshot();
                            MonitorMsg::Refreshed(agents)
                        }),
                    ),
                    Cmd::Tick(Duration::from_secs(self.interval_secs)),
                ])
            }
            MonitorMsg::Refreshed(agents) => {
                // Preserve selection by PID in filtered view
                let old_pid = self
                    .filtered_agents()
                    .get(self.selected)
                    .map(|a| a.pid);
                self.agents = agents;

                // Try to keep the same agent selected by PID in filtered list
                if let Some(pid) = old_pid {
                    let new_idx = self
                        .agents
                        .iter()
                        .filter(|a| self.filter.matches_agent(a))
                        .enumerate()
                        .find(|(_, a)| a.pid == pid)
                        .map(|(i, _)| i);
                    if let Some(idx) = new_idx {
                        self.selected = idx;
                    }
                }
                let count = self.filtered_agents().len();
                if self.selected >= count && count > 0 {
                    self.selected = count - 1;
                }

                // Detect new attention-needing PIDs and ring bell
                let current_attention: HashSet<u32> = self
                    .agents
                    .iter()
                    .filter(|a| a.state.needs_attention())
                    .map(|a| a.pid)
                    .collect();
                let has_new = current_attention
                    .iter()
                    .any(|pid| !self.prev_attention_pids.contains(pid));
                self.prev_attention_pids = current_attention;

                if has_new {
                    Cmd::Task(
                        TaskSpec::default(),
                        Box::new(|| {
                            // Write bell to /dev/tty to bypass ftui's terminal capture
                            if let Ok(mut tty) = std::fs::OpenOptions::new()
                                .write(true)
                                .open("/dev/tty")
                            {
                                use std::io::Write;
                                let _ = tty.write_all(b"\x07");
                            }
                            MonitorMsg::Noop
                        }),
                    )
                } else {
                    Cmd::none()
                }
            }
            MonitorMsg::SelectNext => {
                let count = self.filtered_agents().len();
                if count > 0 {
                    self.selected = (self.selected + 1).min(count - 1);
                }
                self.detail_scroll = 0;
                Cmd::none()
            }
            MonitorMsg::SelectPrev => {
                self.selected = self.selected.saturating_sub(1);
                self.detail_scroll = 0;
                Cmd::none()
            }
            MonitorMsg::SelectFirst => {
                self.selected = 0;
                self.detail_scroll = 0;
                Cmd::none()
            }
            MonitorMsg::SelectLast => {
                let count = self.filtered_agents().len();
                if count > 0 {
                    self.selected = count - 1;
                }
                self.detail_scroll = 0;
                Cmd::none()
            }
            MonitorMsg::CycleFilter => {
                self.filter = self.filter.next();
                self.selected = 0;
                self.detail_scroll = 0;
                Cmd::none()
            }
            MonitorMsg::DetailScrollUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(3);
                Cmd::none()
            }
            MonitorMsg::DetailScrollDown => {
                self.detail_scroll += 3;
                Cmd::none()
            }
            MonitorMsg::OpenEditor => {
                let cwd = self
                    .filtered_agents()
                    .get(self.selected)
                    .map(|a| a.cwd.to_string_lossy().to_string());
                if let Some(cwd) = cwd {
                    Cmd::Task(
                        TaskSpec::default(),
                        Box::new(move || {
                            let _ = std::process::Command::new("open")
                                .args(["-a", "Cursor", &cwd])
                                .spawn();
                            MonitorMsg::Noop
                        }),
                    )
                } else {
                    Cmd::none()
                }
            }
            MonitorMsg::Quit => Cmd::Quit,
            MonitorMsg::Noop => Cmd::none(),
        }
    }

    fn view(&self, frame: &mut Frame) {
        let buf = &mut frame.buffer;
        let area = Rect::new(0, 0, buf.width(), buf.height());

        // Fill background
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.get_mut(x, y) {
                    cell.content = CellContent::from_char(' ');
                    cell.bg = DARK_BG;
                }
            }
        }

        // Main vertical layout: header | content | footer
        let v_chunks = Flex::vertical()
            .constraints([
                Constraint::Fixed(4), // logo (2 lines) + subtitle + border
                Constraint::Fill,                          // main content
                Constraint::Fixed(1),                      // footer
            ])
            .split(area);

        self.render_header(buf, v_chunks[0]);
        self.render_footer(buf, v_chunks[2]);

        let filtered = self.filtered_agents();
        if filtered.is_empty() {
            self.render_empty(buf, v_chunks[1]);
        } else {
            // Horizontal split: table (60%) | detail (40%)
            let h_chunks = Flex::horizontal()
                .constraints([Constraint::Percentage(60.0), Constraint::Percentage(40.0)])
                .split(v_chunks[1]);

            self.render_table_filtered(&filtered, buf, h_chunks[0]);
            self.render_detail_filtered(&filtered, buf, h_chunks[1]);
        }
    }
}

// ─── Rendering helpers ─────────────────────────────────────────────────────

impl MonitorApp {
    fn render_header(&self, buf: &mut ftui::Buffer, area: Rect) {
        // Draw compact 2-line logo
        draw_str(buf, area.x + 2, area.y, LOGO_LINE, Style::default().fg(CYAN).bold());
        if area.y + 1 < area.y + area.height {
            draw_str(buf, area.x + 2, area.y + 1, LOGO_LINE2, Style::default().fg(CYAN).bold());
        }

        // Draw subtitle + agent count on the same line as logo line 1
        let sub_x = area.x + 2 + LOGO_LINE.len() as u16 + 3;
        draw_str(buf, sub_x, area.y, SUBTITLE, Style::default().fg(MAGENTA));

        // Agent count badge on line 2
        let parent_count = self.agents.iter().filter(|a| !a.is_subagent).count();
        let sub_count = self.agents.iter().filter(|a| a.is_subagent).count();
        let needs_attention = self.agents.iter().filter(|a| a.state.needs_attention()).count();
        let badge = if needs_attention > 0 {
            if sub_count > 0 {
                format!(
                    "{} agents + {} subagents  {} need attention",
                    parent_count, sub_count, needs_attention
                )
            } else {
                format!(
                    "{} agents  {} need attention",
                    parent_count, needs_attention
                )
            }
        } else if sub_count > 0 {
            format!("{} agents + {} subagents", parent_count, sub_count)
        } else {
            format!("{} agents active", parent_count)
        };
        let badge_x = area.x + 2 + LOGO_LINE2.len() as u16 + 3;
        let badge_style = if needs_attention > 0 {
            Style::default().fg(YELLOW).bold()
        } else {
            Style::default().fg(GREEN)
        };
        if area.y + 1 < area.y + area.height {
            draw_str(buf, badge_x, area.y + 1, &badge, badge_style);
        }

        // Bottom border of header
        let border_y = area.y + area.height - 1;
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.get_mut(x, border_y) {
                cell.content = CellContent::from_char('─');
                cell.fg = DIM;
                cell.bg = DARK_BG;
            }
        }
    }

    fn render_footer(&self, buf: &mut ftui::Buffer, area: Rect) {
        let blink = self.tick_count % 2 == 0;
        let dot = if blink { "●" } else { "○" };
        let filter_label = if self.filter == FilterMode::All {
            String::new()
        } else {
            format!("  │  Filter: {}", self.filter.label())
        };

        // Context pressure warning
        let high_ctx_count = self
            .agents
            .iter()
            .filter(|a| agent_pressure_pct(a).map_or(false, |p| p > 75))
            .count();
        let ctx_warning = if high_ctx_count > 0 {
            format!("  │  ⚠ {} agent{} >75% ctx", high_ctx_count, if high_ctx_count == 1 { "" } else { "s" })
        } else {
            String::new()
        };

        let footer = format!(
            " {} Live  │  j/k Navigate  │  f Filter  │  e Editor  │  q Quit{}{}",
            dot, filter_label, ctx_warning
        );
        draw_str(
            buf,
            area.x,
            area.y,
            &footer,
            Style::default().fg(DIM).bg(HEADER_BG),
        );
        // Fill rest of footer
        let used = footer.len() as u16;
        for x in area.x + used..area.x + area.width {
            if let Some(cell) = buf.get_mut(x, area.y) {
                cell.content = CellContent::from_char(' ');
                cell.bg = HEADER_BG;
            }
        }
    }

    fn render_empty(&self, buf: &mut ftui::Buffer, area: Rect) {
        let mid_y = area.y + area.height / 2;
        let msg1 = "No active Claude Code instances found.";
        let msg2 = "Looking for `claude` processes with JSONL session files...";
        let x1 = area.x + area.width.saturating_sub(msg1.len() as u16) / 2;
        let x2 = area.x + area.width.saturating_sub(msg2.len() as u16) / 2;
        draw_str(
            buf,
            x1,
            mid_y.saturating_sub(1),
            msg1,
            Style::default().fg(WHITE),
        );
        draw_str(buf, x2, mid_y + 1, msg2, Style::default().fg(DIM));
    }

    fn render_table_filtered(
        &self,
        filtered: &[&AgentInstance],
        buf: &mut ftui::Buffer,
        area: Rect,
    ) {
        if area.height < 3 || area.width < 20 {
            return;
        }

        // Column header
        let header_y = area.y;
        let header = format!(
            " {:<22} {:<16} {:<8} {:<8} {:<10}",
            "PROJECT", "STATE", "AGE", "MODE", "CTX"
        );
        draw_str(
            buf,
            area.x,
            header_y,
            &header,
            Style::default().fg(BRIGHT_WHITE).bold().bg(HEADER_BG),
        );
        // Fill rest of header row
        for x in area.x + header.len() as u16..area.x + area.width {
            if let Some(cell) = buf.get_mut(x, header_y) {
                cell.content = CellContent::from_char(' ');
                cell.bg = HEADER_BG;
            }
        }

        // Separator
        let sep_y = header_y + 1;
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.get_mut(x, sep_y) {
                cell.content = CellContent::from_char('─');
                cell.fg = DIM;
                cell.bg = DARK_BG;
            }
        }

        // Agent rows
        let row_start = sep_y + 1;
        let max_rows = (area.height - 2) as usize;

        for (i, agent) in filtered.iter().enumerate() {
            if i >= max_rows {
                break;
            }
            let y = row_start + i as u16;
            let is_selected = i == self.selected;

            let row_bg = if is_selected { SELECTED_BG } else { DARK_BG };

            // Fill row background
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.get_mut(x, y) {
                    cell.content = CellContent::from_char(' ');
                    cell.bg = row_bg;
                }
            }

            // Selection indicator
            if is_selected {
                if let Some(cell) = buf.get_mut(area.x, y) {
                    cell.content = CellContent::from_char('▸');
                    cell.fg = CYAN;
                }
            }

            // Project name (subagents show with tree prefix and slug)
            let (name_str, name_color) = if agent.is_subagent {
                let slug = agent
                    .agent_slug
                    .as_deref()
                    .unwrap_or(&agent.project_name);
                (format!("├─ {}", truncate_str(slug, 18)), DIM)
            } else {
                (truncate_str(&agent.project_name, 21), WHITE)
            };
            draw_str(
                buf,
                area.x + 1,
                y,
                &format!(" {:<21}", name_str),
                Style::default().fg(name_color).bg(row_bg),
            );

            // State with color
            let (state_icon, state_text, state_color) =
                state_display(&agent.state, self.tick_count);
            let state_str = format!("{} {}", state_icon, state_text);
            draw_str(
                buf,
                area.x + 24,
                y,
                &format!("{:<15}", state_str),
                Style::default().fg(state_color).bg(row_bg),
            );

            // Age
            let age = format_age(agent.age_secs);
            draw_str(
                buf,
                area.x + 40,
                y,
                &format!("{:<8}", age),
                Style::default().fg(DIM).bg(row_bg),
            );

            // Mode
            let (mode_str, mode_color) = mode_display(&agent.permission_mode);
            draw_str(
                buf,
                area.x + 48,
                y,
                &format!("{:<8}", mode_str),
                Style::default().fg(mode_color).bg(row_bg),
            );

            // CTX pressure bar
            let ctx_str = render_context_bar_compact(agent);
            let ctx_color = context_pressure_color(agent);
            draw_str(
                buf,
                area.x + 56,
                y,
                &ctx_str,
                Style::default().fg(ctx_color).bg(row_bg),
            );
        }
    }

    fn render_detail_filtered(
        &self,
        filtered: &[&AgentInstance],
        buf: &mut ftui::Buffer,
        area: Rect,
    ) {
        if area.height < 3 || area.width < 10 {
            return;
        }

        // Vertical separator on left edge
        for y in area.y..area.y + area.height {
            if let Some(cell) = buf.get_mut(area.x, y) {
                cell.content = CellContent::from_char('│');
                cell.fg = DIM;
                cell.bg = DARK_BG;
            }
        }

        let inner = Rect::new(area.x + 2, area.y, area.width.saturating_sub(3), area.height);

        if let Some(agent) = filtered.get(self.selected) {
            // Build logical lines, then render with scroll offset
            let mut lines: Vec<DetailLine> = Vec::new();

            lines.push(DetailLine::styled("AGENT DETAIL", Style::default().fg(CYAN).bold()));
            lines.push(DetailLine::Separator);
            lines.push(DetailLine::label_value("PID", &agent.pid.to_string()));
            lines.push(DetailLine::label_value("TTY", &agent.tty));
            lines.push(DetailLine::label_value("CWD", &agent.cwd.to_string_lossy()));

            let (icon, text, color) = state_display(&agent.state, self.tick_count);
            lines.push(DetailLine::styled(
                &format!("State:  {} {}", icon, text),
                Style::default().fg(color),
            ));

            let activity = format_activity_age(agent.last_activity_secs);
            lines.push(DetailLine::label_value("Activity", &activity));
            lines.push(DetailLine::Blank);

            // Team context section (if applicable)
            if agent.team_name.is_some() || agent.is_subagent {
                lines.push(DetailLine::styled(
                    "TEAM CONTEXT",
                    Style::default().fg(CYAN).bold(),
                ));
                lines.push(DetailLine::Separator);
                if let Some(ref team) = agent.team_name {
                    lines.push(DetailLine::label_value("Team", team));
                }
                if let Some(ref role) = agent.agent_role {
                    lines.push(DetailLine::label_value("Role", role));
                }
                if let Some(ref slug) = agent.agent_slug {
                    lines.push(DetailLine::label_value("Name", slug));
                }
                if agent.is_subagent {
                    lines.push(DetailLine::label_value("Type", "subagent"));
                    if let Some(ref parent_id) = agent.parent_session_id {
                        let short_id = if parent_id.len() > 12 {
                            &parent_id[..12]
                        } else {
                            parent_id
                        };
                        lines.push(DetailLine::label_value("Parent", short_id));
                    }
                }
                lines.push(DetailLine::Blank);
            }

            // Telemetry section
            if let Some(ref telem) = agent
                .session_context
                .as_ref()
                .and_then(|c| c.telemetry.as_ref())
            {
                lines.push(DetailLine::styled(
                    "TELEMETRY",
                    Style::default().fg(CYAN).bold(),
                ));
                lines.push(DetailLine::Separator);

                // Context gauge: 10-char bar + percentage + absolute
                let ctx_bar = render_context_gauge(telem.context_pressure_pct);
                let ctx_abs = format_token_count(telem.context_tokens);
                let ctx_max = format_token_count(telem.context_max);
                let gauge_color = if telem.context_pressure_pct > 75 {
                    RED
                } else if telem.context_pressure_pct > 50 {
                    YELLOW
                } else {
                    GREEN
                };
                lines.push(DetailLine::styled(
                    &format!(
                        "Context:  {} {}% ({} / {})",
                        ctx_bar, telem.context_pressure_pct, ctx_abs, ctx_max
                    ),
                    Style::default().fg(gauge_color),
                ));

                // Burn rate
                if telem.burn_rate_per_min > 0 {
                    lines.push(DetailLine::label_value(
                        "Burn",
                        &format!("{} tok/min", format_number(telem.burn_rate_per_min)),
                    ));
                }

                // Output tokens
                lines.push(DetailLine::label_value(
                    "Output",
                    &format!("{} tokens (total)", format_number(telem.total_output_tokens)),
                ));

                // Turn count
                lines.push(DetailLine::label_value(
                    "Turns",
                    &telem.turn_count.to_string(),
                ));

                // Session duration (use agent process age)
                if agent.age_secs > 0 {
                    lines.push(DetailLine::label_value(
                        "Session",
                        &format_age(agent.age_secs),
                    ));
                }

                // Queue status
                if telem.has_queued_messages {
                    lines.push(DetailLine::styled(
                        "Queued:   yes",
                        Style::default().fg(YELLOW),
                    ));
                }

                // Tool mix (top 4)
                if !telem.tool_mix.is_empty() {
                    lines.push(DetailLine::Blank);
                    lines.push(DetailLine::styled(
                        "TOOL MIX",
                        Style::default().fg(CYAN).bold(),
                    ));
                    lines.push(DetailLine::Separator);

                    let max_count = telem.tool_mix.first().map(|(_, c)| *c).unwrap_or(1);
                    for (name, count) in telem.tool_mix.iter().take(4) {
                        let bar = render_tool_bar(*count, max_count);
                        lines.push(DetailLine::styled(
                            &format!("{:<8} {} {:>4}", truncate_str(name, 8), bar, count),
                            Style::default().fg(MAGENTA),
                        ));
                    }
                }

                lines.push(DetailLine::Blank);
            }

            if let Some(ref ctx) = agent.session_context {
                lines.push(DetailLine::styled(
                    "SESSION CONTEXT",
                    Style::default().fg(CYAN).bold(),
                ));
                lines.push(DetailLine::Separator);

                if let Some(ref model) = ctx.model {
                    lines.push(DetailLine::label_value("Model", model));
                }
                if let Some(ref branch) = ctx.git_branch {
                    lines.push(DetailLine::label_value("Branch", branch));
                }
                if let Some(ref msg) = ctx.last_user_message {
                    lines.push(DetailLine::Blank);
                    lines.push(DetailLine::styled(
                        "Last User Message:",
                        Style::default().fg(DIM),
                    ));
                    let wrapped =
                        truncate_str(msg, (inner.width as usize).saturating_sub(2));
                    lines.push(DetailLine::IndentedStyled(
                        wrapped,
                        Style::default().fg(WHITE),
                    ));
                }
                if let Some(ref msg) = ctx.last_assistant_message {
                    lines.push(DetailLine::Blank);
                    lines.push(DetailLine::styled(
                        "Last Assistant:",
                        Style::default().fg(DIM),
                    ));
                    let wrapped =
                        truncate_str(msg, (inner.width as usize).saturating_sub(2));
                    lines.push(DetailLine::IndentedStyled(
                        wrapped,
                        Style::default().fg(GREEN),
                    ));
                }

                if !ctx.recent_activity.is_empty() {
                    lines.push(DetailLine::Blank);
                    lines.push(DetailLine::styled(
                        "RECENT ACTIVITY",
                        Style::default().fg(CYAN).bold(),
                    ));
                    lines.push(DetailLine::Separator);

                    for entry in &ctx.recent_activity {
                        let summary_style = match entry.kind.as_str() {
                            "user" => Style::default().fg(YELLOW),
                            "tool" => Style::default().fg(MAGENTA),
                            "assistant" => Style::default().fg(GREEN),
                            _ => Style::default().fg(WHITE),
                        };
                        let summary = truncate_str(
                            &entry.summary,
                            (inner.width as usize).saturating_sub(10),
                        );
                        lines.push(DetailLine::Activity {
                            timestamp: entry.timestamp.clone(),
                            summary,
                            style: summary_style,
                        });
                    }
                }
            }

            // Render with scroll offset
            let total_lines = lines.len();
            let visible_height = inner.height as usize;

            // Show scroll-up indicator
            if self.detail_scroll > 0 {
                draw_str(
                    buf,
                    inner.x + inner.width.saturating_sub(1),
                    inner.y,
                    "^",
                    Style::default().fg(DIM),
                );
            }

            let mut screen_y = inner.y;
            for (i, line) in lines.iter().enumerate() {
                if i < self.detail_scroll {
                    continue;
                }
                if screen_y >= inner.y + inner.height {
                    break;
                }
                match line {
                    DetailLine::Styled(text, style) => {
                        draw_str(buf, inner.x, screen_y, text, *style);
                    }
                    DetailLine::IndentedStyled(text, style) => {
                        draw_str(buf, inner.x + 1, screen_y, text, *style);
                    }
                    DetailLine::LabelValue(label, value) => {
                        draw_label_value(
                            buf,
                            inner.x,
                            screen_y,
                            inner.width,
                            label,
                            value,
                        );
                    }
                    DetailLine::Separator => {
                        draw_hline(buf, inner.x, screen_y, inner.width, '─', DIM);
                    }
                    DetailLine::Blank => {}
                    DetailLine::Activity {
                        timestamp,
                        summary,
                        style,
                    } => {
                        draw_str(
                            buf,
                            inner.x,
                            screen_y,
                            timestamp,
                            Style::default().fg(DIM),
                        );
                        draw_str(buf, inner.x + 9, screen_y, summary, *style);
                    }
                }
                screen_y += 1;
            }

            // Show scroll-down indicator
            if self.detail_scroll + visible_height < total_lines {
                let indicator_y = inner.y + inner.height.saturating_sub(1);
                draw_str(
                    buf,
                    inner.x + inner.width.saturating_sub(1),
                    indicator_y,
                    "v",
                    Style::default().fg(DIM),
                );
            }
        } else {
            draw_str(
                buf,
                inner.x,
                inner.y + inner.height / 2,
                "No agent selected",
                Style::default().fg(DIM),
            );
        }
    }
}

// ─── Detail line model (for scrollable detail pane) ───────────────────────

enum DetailLine {
    Styled(String, Style),
    IndentedStyled(String, Style),
    LabelValue(String, String),
    Separator,
    Blank,
    Activity {
        timestamp: String,
        summary: String,
        style: Style,
    },
}

impl DetailLine {
    fn styled(text: &str, style: Style) -> Self {
        Self::Styled(text.to_string(), style)
    }

    fn label_value(label: &str, value: &str) -> Self {
        Self::LabelValue(label.to_string(), value.to_string())
    }
}

// ─── Drawing utilities ─────────────────────────────────────────────────────

fn draw_str(buf: &mut ftui::Buffer, x: u16, y: u16, text: &str, style: Style) {
    let mut col = x;
    for ch in text.chars() {
        if col >= buf.width() || y >= buf.height() {
            break;
        }
        if let Some(cell) = buf.get_mut(col, y) {
            cell.content = CellContent::from_char(ch);
            if let Some(fg) = style.fg {
                cell.fg = fg;
            }
            if let Some(bg) = style.bg {
                cell.bg = bg;
            }
        }
        col += 1;
    }
}

fn draw_hline(buf: &mut ftui::Buffer, x: u16, y: u16, width: u16, ch: char, color: PackedRgba) {
    for col in x..x + width {
        if col >= buf.width() || y >= buf.height() {
            break;
        }
        if let Some(cell) = buf.get_mut(col, y) {
            cell.content = CellContent::from_char(ch);
            cell.fg = color;
            cell.bg = DARK_BG;
        }
    }
}

fn draw_label_value(buf: &mut ftui::Buffer, x: u16, y: u16, width: u16, label: &str, value: &str) {
    let label_str = format!("{}:", label);
    draw_str(buf, x, y, &label_str, Style::default().fg(DIM));
    let val_x = x + label_str.len() as u16 + 1;
    let max_val = (width as usize).saturating_sub(label_str.len() + 1);
    let val = truncate_str(value, max_val);
    draw_str(buf, val_x, y, &val, Style::default().fg(WHITE));
}

fn state_display(state: &AgentState, tick_count: u64) -> (String, &'static str, PackedRgba) {
    let blink = tick_count % 2 == 0;
    match state {
        AgentState::WaitingInput => {
            let icon = if blink { "⚠" } else { " " };
            (icon.to_string(), "NEEDS INPUT", YELLOW)
        }
        AgentState::WaitingPermission => {
            let icon = if blink { "🔒" } else { " " };
            (icon.to_string(), "PERMISSION", RED)
        }
        AgentState::Working => {
            let frame = BRAILLE_SPINNER[(tick_count % 10) as usize];
            (frame.to_string(), "WORKING", GREEN)
        }
        AgentState::ToolRunning => {
            let frame = BRAILLE_SPINNER[(tick_count % 10) as usize];
            (frame.to_string(), "TOOL RUN", GREEN)
        }
        AgentState::Queued => ("⏳".to_string(), "QUEUED", MAGENTA),
        AgentState::Idle => ("💤".to_string(), "IDLE", DIM),
        AgentState::Starting => ("🚀".to_string(), "STARTING", DIM),
    }
}

/// Get telemetry pressure percentage for an agent, or None.
fn agent_pressure_pct(agent: &AgentInstance) -> Option<u8> {
    agent
        .session_context
        .as_ref()?
        .telemetry
        .as_ref()
        .map(|t| t.context_pressure_pct)
}

/// Render compact 3-char block bar + percentage for table CTX column.
/// e.g. "██░ 59%" or "███ 92%"
fn render_context_bar_compact(agent: &AgentInstance) -> String {
    match agent_pressure_pct(agent) {
        Some(pct) => {
            let filled = match pct {
                0..=33 => 1,
                34..=66 => 2,
                _ => 3,
            };
            let bar: String = "█".repeat(filled as usize)
                + &"░".repeat(3 - filled as usize);
            format!("{} {:>3}%", bar, pct)
        }
        None => "  ---".to_string(),
    }
}

/// Color for context pressure: GREEN <50%, YELLOW 50-75%, RED >75%.
fn context_pressure_color(agent: &AgentInstance) -> PackedRgba {
    match agent_pressure_pct(agent) {
        Some(pct) if pct > 75 => RED,
        Some(pct) if pct > 50 => YELLOW,
        Some(_) => GREEN,
        None => DIM,
    }
}

fn mode_display(mode: &PermissionMode) -> (&'static str, PackedRgba) {
    match mode {
        PermissionMode::DangerouslySkip => ("yolo", RED),
        PermissionMode::AllowDangerouslySkip => ("allow", YELLOW),
        PermissionMode::Default => ("default", DIM),
    }
}

/// Render a 10-char context gauge bar: "████████░░"
fn render_context_gauge(pct: u8) -> String {
    let filled = ((pct as f32 / 100.0) * 10.0).round() as usize;
    let filled = filled.min(10);
    let empty = 10 - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Render a 10-char proportional tool bar relative to max.
fn render_tool_bar(count: u32, max_count: u32) -> String {
    if max_count == 0 {
        return "░░░░░░░░░░".to_string();
    }
    let filled = ((count as f32 / max_count as f32) * 10.0).round() as usize;
    let filled = filled.max(1).min(10);
    let empty = 10 - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Format a token count as "123K" or "1,234" for display.
fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// Format a number with comma separators.
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    if len <= 3 {
        return s;
    }
    let mut result = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

fn format_activity_age(secs: u64) -> String {
    if secs == 0 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

fn format_age(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    } else if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

fn truncate_str(s: &str, max: usize) -> String {
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

// ─── Public entry point ────────────────────────────────────────────────────

/// Launch the interactive monitor TUI.
pub fn run_monitor_tui(interval_secs: u64) -> Result<(), crate::CliError> {
    let model = MonitorApp::new(interval_secs);
    let config = ProgramConfig::fullscreen();
    let mut program = Program::with_native_backend(model, config).map_err(|e| crate::CliError {
        code: 9,
        kind: "monitor-tui",
        message: format!("failed to create monitor TUI: {e}"),
        hint: None,
        retryable: false,
    })?;
    program.run().map_err(|e| crate::CliError {
        code: 9,
        kind: "monitor-tui",
        message: format!("monitor TUI error: {e}"),
        hint: None,
        retryable: false,
    })
}
