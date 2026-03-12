//! ftui-based live monitoring dashboard for active Claude Code instances.
//!
//! Renders a split-pane TUI: agent table on the left, detail pane on the right.
//! Refreshes every N seconds via background task. Keyboard-navigable.

use std::time::Duration;

use ftui::layout::{Constraint, Flex};
use ftui::render::cell::{CellContent, PackedRgba};
use ftui::{Cmd, Event, Frame, KeyCode, KeyEvent, Model, Program, ProgramConfig, Style, TaskSpec};

use ftui::core::geometry::Rect;

use crate::monitor::state::{AgentInstance, AgentState, PermissionMode};

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
}

impl MonitorApp {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            agents: Vec::new(),
            selected: 0,
            tick_count: 0,
            interval_secs,
        }
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
                // Preserve selection position
                let old_pid = self.agents.get(self.selected).map(|a| a.pid);
                self.agents = agents;

                // Try to keep the same agent selected by PID
                if let Some(pid) = old_pid {
                    if let Some(idx) = self.agents.iter().position(|a| a.pid == pid) {
                        self.selected = idx;
                    }
                }
                if self.selected >= self.agents.len() && !self.agents.is_empty() {
                    self.selected = self.agents.len() - 1;
                }

                Cmd::none()
            }
            MonitorMsg::SelectNext => {
                if !self.agents.is_empty() {
                    self.selected = (self.selected + 1).min(self.agents.len() - 1);
                }
                Cmd::none()
            }
            MonitorMsg::SelectPrev => {
                self.selected = self.selected.saturating_sub(1);
                Cmd::none()
            }
            MonitorMsg::SelectFirst => {
                self.selected = 0;
                Cmd::none()
            }
            MonitorMsg::SelectLast => {
                if !self.agents.is_empty() {
                    self.selected = self.agents.len() - 1;
                }
                Cmd::none()
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

        if self.agents.is_empty() {
            self.render_empty(buf, v_chunks[1]);
        } else {
            // Horizontal split: table (60%) | detail (40%)
            let h_chunks = Flex::horizontal()
                .constraints([Constraint::Percentage(60.0), Constraint::Percentage(40.0)])
                .split(v_chunks[1]);

            self.render_table(buf, h_chunks[0]);
            self.render_detail(buf, h_chunks[1]);
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
        let needs_attention = self.agents.iter().filter(|a| a.state.needs_attention()).count();
        let badge = if needs_attention > 0 {
            format!(
                "{} agents  {} need attention",
                self.agents.len(),
                needs_attention
            )
        } else {
            format!("{} agents active", self.agents.len())
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
        let footer = format!(
            " {} Live  │  j/k Navigate  │  q Quit  │  Refreshing every {}s",
            dot, self.interval_secs
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

    fn render_table(&self, buf: &mut ftui::Buffer, area: Rect) {
        if area.height < 3 || area.width < 20 {
            return;
        }

        // Column header
        let header_y = area.y;
        let header = format!(
            " {:<22} {:<16} {:<8} {:<8}",
            "PROJECT", "STATE", "AGE", "MODE"
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

        for (i, agent) in self.agents.iter().enumerate() {
            if i >= max_rows {
                break;
            }
            let y = row_start + i as u16;
            let is_selected = i == self.selected;
            let blink = self.tick_count % 2 == 0;

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

            // Project name
            let name = truncate_str(&agent.project_name, 21);
            draw_str(
                buf,
                area.x + 1,
                y,
                &format!(" {:<21}", name),
                Style::default().fg(WHITE).bg(row_bg),
            );

            // State with color
            let (state_icon, state_text, state_color) = state_display(&agent.state, blink);
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
        }
    }

    fn render_detail(&self, buf: &mut ftui::Buffer, area: Rect) {
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

        if let Some(agent) = self.agents.get(self.selected) {
            let mut y = inner.y;

            // Title
            draw_str(
                buf,
                inner.x,
                y,
                "AGENT DETAIL",
                Style::default().fg(CYAN).bold(),
            );
            y += 1;
            draw_hline(buf, inner.x, y, inner.width, '─', DIM);
            y += 1;

            // PID
            draw_label_value(buf, inner.x, y, inner.width, "PID", &agent.pid.to_string());
            y += 1;

            // TTY
            draw_label_value(buf, inner.x, y, inner.width, "TTY", &agent.tty);
            y += 1;

            // CWD
            let cwd = agent.cwd.to_string_lossy();
            draw_label_value(buf, inner.x, y, inner.width, "CWD", &cwd);
            y += 1;

            // State
            let blink = self.tick_count % 2 == 0;
            let (icon, text, color) = state_display(&agent.state, blink);
            draw_str(
                buf,
                inner.x,
                y,
                &format!("State:  {} {}", icon, text),
                Style::default().fg(color),
            );
            y += 1;

            // Last activity
            let activity = if agent.last_activity_secs == 0 {
                "just now".to_string()
            } else {
                format!("{}s ago", agent.last_activity_secs)
            };
            draw_label_value(buf, inner.x, y, inner.width, "Activity", &activity);
            y += 2;

            // Session context
            if let Some(ref ctx) = agent.session_context {
                draw_str(
                    buf,
                    inner.x,
                    y,
                    "SESSION CONTEXT",
                    Style::default().fg(CYAN).bold(),
                );
                y += 1;
                draw_hline(buf, inner.x, y, inner.width, '─', DIM);
                y += 1;

                if let Some(ref model) = ctx.model {
                    draw_label_value(buf, inner.x, y, inner.width, "Model", model);
                    y += 1;
                }
                if let Some(ref branch) = ctx.git_branch {
                    draw_label_value(buf, inner.x, y, inner.width, "Branch", branch);
                    y += 1;
                }
                if let Some(ref msg) = ctx.last_user_message {
                    y += 1;
                    draw_str(
                        buf,
                        inner.x,
                        y,
                        "Last User Message:",
                        Style::default().fg(DIM),
                    );
                    y += 1;
                    let wrapped = truncate_str(msg, (inner.width as usize).saturating_sub(2));
                    draw_str(
                        buf,
                        inner.x + 1,
                        y,
                        &wrapped,
                        Style::default().fg(WHITE),
                    );
                    y += 1;
                }
                if let Some(ref msg) = ctx.last_assistant_message {
                    y += 1;
                    draw_str(
                        buf,
                        inner.x,
                        y,
                        "Last Assistant:",
                        Style::default().fg(DIM),
                    );
                    y += 1;
                    let wrapped = truncate_str(msg, (inner.width as usize).saturating_sub(2));
                    draw_str(
                        buf,
                        inner.x + 1,
                        y,
                        &wrapped,
                        Style::default().fg(GREEN),
                    );
                    y += 1;
                }

                // Recent activity log
                if !ctx.recent_activity.is_empty() {
                    y += 1;
                    draw_str(
                        buf,
                        inner.x,
                        y,
                        "RECENT ACTIVITY",
                        Style::default().fg(CYAN).bold(),
                    );
                    y += 1;
                    draw_hline(buf, inner.x, y, inner.width, '─', DIM);
                    y += 1;

                    for entry in &ctx.recent_activity {
                        if y >= inner.y + inner.height {
                            break;
                        }
                        let time_style = Style::default().fg(DIM);
                        let summary_style = match entry.kind.as_str() {
                            "user" => Style::default().fg(YELLOW),
                            "tool" => Style::default().fg(MAGENTA),
                            "assistant" => Style::default().fg(GREEN),
                            _ => Style::default().fg(WHITE),
                        };

                        draw_str(buf, inner.x, y, &entry.timestamp, time_style);
                        let summary = truncate_str(
                            &entry.summary,
                            (inner.width as usize).saturating_sub(10),
                        );
                        draw_str(buf, inner.x + 9, y, &summary, summary_style);
                        y += 1;
                    }
                }
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

fn state_display(state: &AgentState, blink: bool) -> (&'static str, &'static str, PackedRgba) {
    match state {
        AgentState::WaitingInput => {
            let icon = if blink { "⚠" } else { " " };
            (icon, "NEEDS INPUT", YELLOW)
        }
        AgentState::WaitingPermission => {
            let icon = if blink { "🔒" } else { " " };
            (icon, "PERMISSION", RED)
        }
        AgentState::Working => ("⚙", "WORKING", GREEN),
        AgentState::ToolRunning => ("🔧", "TOOL RUN", GREEN),
        AgentState::Queued => ("⏳", "QUEUED", MAGENTA),
        AgentState::Idle => ("💤", "IDLE", DIM),
        AgentState::Starting => ("🚀", "STARTING", DIM),
    }
}

fn mode_display(mode: &PermissionMode) -> (&'static str, PackedRgba) {
    match mode {
        PermissionMode::DangerouslySkip => ("yolo", RED),
        PermissionMode::AllowDangerouslySkip => ("allow", YELLOW),
        PermissionMode::Default => ("default", DIM),
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
