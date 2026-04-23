//! Pre-computed analytics generator for pages export.
//!
//! Generates pre-computed analytics data files (statistics.json, timeline.json, etc.)
//! during export that enable instant dashboard rendering in the browser without
//! expensive SQL aggregations.
//!
//! # Generated Files
//!
//! All files are encrypted with the main database and included in the payload:
//!
//! - `statistics.json` - Overall metrics (counts, time range)
//! - `agent_summary.json` - Per-agent breakdown
//! - `workspace_summary.json` - Per-workspace breakdown
//! - `timeline.json` - Activity over time (daily/weekly/monthly)
//! - `top_terms.json` - Common topics/terms from titles
//!
//! # Example
//!
//! ```ignore
//! use crate::pages::analytics::AnalyticsGenerator;
//!
//! let generator = AnalyticsGenerator::new(&db_conn)?;
//! let bundle = generator.generate_all()?;
//! bundle.write_to_dir(&output_dir)?;
//! ```

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use frankensqlite::compat::{ConnectionExt, RowExt};
use frankensqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;
use tracing::info;

/// Stop words to filter out from term extraction.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "is", "it", "as", "was", "be", "are", "been", "being", "have", "has", "had", "do",
    "does", "did", "will", "would", "could", "should", "may", "might", "must", "shall", "can",
    "need", "this", "that", "these", "those", "i", "you", "he", "she", "we", "they", "what",
    "which", "who", "when", "where", "why", "how", "all", "each", "every", "both", "few", "more",
    "most", "other", "some", "such", "no", "nor", "not", "only", "own", "same", "so", "than",
    "too", "very", "just", "also", "now", "here", "there", "then", "once", "about", "after",
    "again", "into", "over", "under", "out", "up", "down", "off", "any", "its", "your", "my",
    "our", "their", "his", "her", "him", "them", "me", "us", "if", "else", "while", "during",
    "before",
];

/// Overall statistics for the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statistics {
    pub total_conversations: usize,
    pub total_messages: usize,
    pub total_characters: usize,
    // BTreeMap so statistics.json serialization is byte-deterministic
    // across runs. `pub write_to_dir` emits these via
    // `serde_json::to_string_pretty`; a HashMap here would make every
    // regenerate emit a diff even when the data is unchanged, breaking
    // reproducible builds, git hygiene, and any content-hash checks.
    pub agents: BTreeMap<String, AgentStats>,
    pub roles: BTreeMap<String, usize>,
    pub time_range: TimeRange,
    /// RFC3339 timestamp
    pub computed_at: String,
}

/// Per-agent statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStats {
    pub conversations: usize,
    pub messages: usize,
}

/// Time range for the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    /// RFC3339 timestamp or None
    pub earliest: Option<String>,
    /// RFC3339 timestamp or None
    pub latest: Option<String>,
}

/// Timeline data with daily/weekly/monthly aggregations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub daily: Vec<DailyEntry>,
    pub weekly: Vec<WeeklyEntry>,
    pub monthly: Vec<MonthlyEntry>,
    // BTreeMap for deterministic timeline.json serialization (see
    // Statistics.agents comment for rationale).
    pub by_agent: BTreeMap<String, AgentTimeline>,
}

/// Agent-specific timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTimeline {
    pub daily: Vec<DailyEntry>,
    pub weekly: Vec<WeeklyEntry>,
    pub monthly: Vec<MonthlyEntry>,
}

/// Daily activity entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyEntry {
    pub date: String,
    pub messages: usize,
    pub conversations: usize,
}

/// Weekly activity entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeeklyEntry {
    pub week: String,
    pub messages: usize,
    pub conversations: usize,
}

/// Monthly activity entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonthlyEntry {
    pub month: String,
    pub messages: usize,
    pub conversations: usize,
}

/// Per-workspace summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub workspaces: Vec<WorkspaceEntry>,
}

/// Individual workspace entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub path: String,
    pub display_name: String,
    pub conversations: usize,
    pub messages: usize,
    pub agents: Vec<String>,
    pub date_range: TimeRange,
    pub recent_titles: Vec<String>,
}

/// Per-agent summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub agents: Vec<AgentEntry>,
}

/// Individual agent entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub name: String,
    pub conversations: usize,
    pub messages: usize,
    pub workspaces: Vec<String>,
    pub date_range: TimeRange,
    pub avg_messages_per_conversation: f64,
}

/// Top terms extracted from conversation titles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopTerms {
    pub terms: Vec<(String, usize)>,
}

/// Bundle of all analytics data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsBundle {
    pub statistics: Statistics,
    pub timeline: Timeline,
    pub workspace_summary: WorkspaceSummary,
    pub agent_summary: AgentSummary,
    pub top_terms: TopTerms,
}

impl AnalyticsBundle {
    /// Write all analytics files to a directory.
    pub fn write_to_dir(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).context("Failed to create analytics directory")?;

        // Write statistics.json
        let stats_path = dir.join("statistics.json");
        let stats_json = serde_json::to_string_pretty(&self.statistics)
            .context("Failed to serialize statistics")?;
        std::fs::write(&stats_path, stats_json).context("Failed to write statistics.json")?;

        // Write timeline.json
        let timeline_path = dir.join("timeline.json");
        let timeline_json =
            serde_json::to_string_pretty(&self.timeline).context("Failed to serialize timeline")?;
        std::fs::write(&timeline_path, timeline_json).context("Failed to write timeline.json")?;

        // Write workspace_summary.json
        let workspace_path = dir.join("workspace_summary.json");
        let workspace_json = serde_json::to_string_pretty(&self.workspace_summary)
            .context("Failed to serialize workspace_summary")?;
        std::fs::write(&workspace_path, workspace_json)
            .context("Failed to write workspace_summary.json")?;

        // Write agent_summary.json
        let agent_path = dir.join("agent_summary.json");
        let agent_json = serde_json::to_string_pretty(&self.agent_summary)
            .context("Failed to serialize agent_summary")?;
        std::fs::write(&agent_path, agent_json).context("Failed to write agent_summary.json")?;

        // Write top_terms.json
        let terms_path = dir.join("top_terms.json");
        let terms_json = serde_json::to_string_pretty(&self.top_terms)
            .context("Failed to serialize top_terms")?;
        std::fs::write(&terms_path, terms_json).context("Failed to write top_terms.json")?;

        info!(
            "Analytics written to {:?}: statistics.json, timeline.json, workspace_summary.json, agent_summary.json, top_terms.json",
            dir
        );

        Ok(())
    }
}

/// Generator for pre-computed analytics data.
pub struct AnalyticsGenerator<'a> {
    db: &'a Connection,
}

impl<'a> AnalyticsGenerator<'a> {
    /// Create a new analytics generator for the given database connection.
    pub fn new(db: &'a Connection) -> Self {
        Self { db }
    }

    /// Generate all analytics data.
    pub fn generate_all(&self) -> Result<AnalyticsBundle> {
        info!("Generating pre-computed analytics...");

        let statistics = self.generate_statistics()?;
        let timeline = self.generate_timeline()?;
        let workspace_summary = self.generate_workspace_summary()?;
        let agent_summary = self.generate_agent_summary()?;
        let top_terms = self.generate_top_terms()?;

        Ok(AnalyticsBundle {
            statistics,
            timeline,
            workspace_summary,
            agent_summary,
            top_terms,
        })
    }

    /// Generate overall statistics.
    fn generate_statistics(&self) -> Result<Statistics> {
        info!("Generating statistics...");

        // Total conversations
        let total_conversations: i64 = self
            .db
            .query_row_map("SELECT COUNT(*) FROM conversations", &[], |row: &Row| {
                row.get_typed(0)
            })
            .context("Failed to count conversations")?;

        // Total messages
        let total_messages: i64 = self
            .db
            .query_row_map("SELECT COUNT(*) FROM messages", &[], |row: &Row| {
                row.get_typed(0)
            })
            .context("Failed to count messages")?;

        // Total characters
        let total_characters: i64 = self
            .db
            .query_row_map(
                "SELECT SUM(LENGTH(content)) FROM messages",
                &[],
                |row: &Row| row.get_typed::<Option<i64>>(0),
            )
            .context("Failed to sum content lengths")?
            .unwrap_or(0);

        // Per-agent stats
        let mut agents: BTreeMap<String, AgentStats> = BTreeMap::new();
        let agent_conv_rows: Vec<(String, i64)> = self.db.query_map_collect(
            "SELECT agent, COUNT(*) as conv_count FROM conversations GROUP BY agent",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
        )?;
        for (agent, conv_count) in agent_conv_rows {
            agents.insert(
                agent.clone(),
                AgentStats {
                    conversations: conv_count as usize,
                    messages: 0, // Will be filled below
                },
            );
        }

        // Fill in message counts per agent
        let msg_rows: Vec<(String, i64)> = self.db.query_map_collect(
            "SELECT c.agent, COUNT(m.id) FROM messages m
             JOIN conversations c ON m.conversation_id = c.id
             GROUP BY c.agent",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
        )?;
        for (agent, msg_count) in msg_rows {
            if let Some(stats) = agents.get_mut(&agent) {
                stats.messages = msg_count as usize;
            }
        }

        // Per-role counts
        let mut roles: BTreeMap<String, usize> = BTreeMap::new();
        let role_rows: Vec<(String, i64)> = self.db.query_map_collect(
            "SELECT role, COUNT(*) FROM messages GROUP BY role",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
        )?;
        for (role, count) in role_rows {
            roles.insert(role, count as usize);
        }

        // Time range
        let time_range: (Option<i64>, Option<i64>) = self
            .db
            .query_row_map(
                "SELECT MIN(started_at), MAX(started_at) FROM conversations",
                &[],
                |row: &Row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .context("Failed to get time range")?;

        Ok(Statistics {
            total_conversations: total_conversations as usize,
            total_messages: total_messages as usize,
            total_characters: total_characters as usize,
            agents,
            roles,
            time_range: TimeRange {
                earliest: time_range
                    .0
                    .and_then(DateTime::from_timestamp_millis)
                    .map(|dt| dt.to_rfc3339()),
                latest: time_range
                    .1
                    .and_then(DateTime::from_timestamp_millis)
                    .map(|dt| dt.to_rfc3339()),
            },
            computed_at: Utc::now().to_rfc3339(),
        })
    }

    /// Generate timeline data.
    fn generate_timeline(&self) -> Result<Timeline> {
        info!("Generating timeline...");

        // Daily aggregation from messages
        let mut daily_map: HashMap<String, DailyEntry> = HashMap::new();
        let mut daily_conv_ids: HashMap<String, HashSet<i64>> = HashMap::new();

        let timeline_rows: Vec<(Option<String>, i64)> = self.db.query_map_collect(
            "SELECT DATE(m.created_at/1000, 'unixepoch') as date, m.conversation_id
             FROM messages m
             WHERE m.created_at IS NOT NULL
             ORDER BY date",
            &[],
            |row: &Row| {
                Ok((
                    row.get_typed::<Option<String>>(0)?,
                    row.get_typed::<i64>(1)?,
                ))
            },
        )?;

        for (date_opt, conv_id) in timeline_rows {
            if let Some(date) = date_opt {
                let entry = daily_map.entry(date.clone()).or_insert(DailyEntry {
                    date: date.clone(),
                    messages: 0,
                    conversations: 0,
                });
                entry.messages += 1;
                daily_conv_ids.entry(date).or_default().insert(conv_id);
            }
        }

        // Fill in conversation counts
        for (date, conv_ids) in &daily_conv_ids {
            if let Some(entry) = daily_map.get_mut(date) {
                entry.conversations = conv_ids.len();
            }
        }

        let mut daily: Vec<DailyEntry> = daily_map.into_values().collect();
        daily.sort_by(|a, b| a.date.cmp(&b.date));

        // Aggregate to weekly
        let weekly = aggregate_to_weekly(&daily);

        // Aggregate to monthly
        let monthly = aggregate_to_monthly(&daily);

        // Per-agent timeline
        let mut by_agent: BTreeMap<String, AgentTimeline> = BTreeMap::new();
        let mut agent_daily_map: HashMap<String, HashMap<String, DailyEntry>> = HashMap::new();
        let mut agent_daily_conv_ids: HashMap<String, HashMap<String, HashSet<i64>>> =
            HashMap::new();

        let agent_timeline_rows: Vec<(Option<String>, String, i64)> = self.db.query_map_collect(
            "SELECT DATE(m.created_at/1000, 'unixepoch') as date, c.agent, m.conversation_id
             FROM messages m
             JOIN conversations c ON m.conversation_id = c.id
             WHERE m.created_at IS NOT NULL
             ORDER BY date",
            &[],
            |row: &Row| {
                Ok((
                    row.get_typed::<Option<String>>(0)?,
                    row.get_typed::<String>(1)?,
                    row.get_typed::<i64>(2)?,
                ))
            },
        )?;

        for (date_opt, agent, conv_id) in agent_timeline_rows {
            if let Some(date) = date_opt {
                let agent_map = agent_daily_map.entry(agent.clone()).or_default();
                let entry = agent_map.entry(date.clone()).or_insert(DailyEntry {
                    date: date.clone(),
                    messages: 0,
                    conversations: 0,
                });
                entry.messages += 1;

                agent_daily_conv_ids
                    .entry(agent)
                    .or_default()
                    .entry(date)
                    .or_default()
                    .insert(conv_id);
            }
        }

        // Fill in conversation counts per agent
        for (agent, conv_ids_map) in &agent_daily_conv_ids {
            if let Some(daily_map) = agent_daily_map.get_mut(agent) {
                for (date, conv_ids) in conv_ids_map {
                    if let Some(entry) = daily_map.get_mut(date) {
                        entry.conversations = conv_ids.len();
                    }
                }
            }
        }

        // Convert to sorted vectors and build AgentTimeline
        for (agent, daily_map) in agent_daily_map {
            let mut agent_daily: Vec<DailyEntry> = daily_map.into_values().collect();
            agent_daily.sort_by(|a, b| a.date.cmp(&b.date));
            let agent_weekly = aggregate_to_weekly(&agent_daily);
            let agent_monthly = aggregate_to_monthly(&agent_daily);

            by_agent.insert(
                agent,
                AgentTimeline {
                    daily: agent_daily,
                    weekly: agent_weekly,
                    monthly: agent_monthly,
                },
            );
        }

        Ok(Timeline {
            daily,
            weekly,
            monthly,
            by_agent,
        })
    }

    /// Generate workspace summary.
    fn generate_workspace_summary(&self) -> Result<WorkspaceSummary> {
        info!("Generating workspace summary...");
        let started = Instant::now();

        let mut workspaces: Vec<WorkspaceEntry> = Vec::new();

        // Query 1: base workspace rows with conversation/time aggregates.
        let workspace_rows: Vec<(String, i64, Option<i64>, Option<i64>)> =
            self.db.query_map_collect(
                "SELECT workspace, COUNT(*) as conv_count,
                    MIN(started_at), MAX(started_at)
             FROM conversations
             WHERE workspace IS NOT NULL
             GROUP BY workspace
             ORDER BY conv_count DESC",
                &[],
                |row: &Row| {
                    Ok((
                        row.get_typed::<String>(0)?,
                        row.get_typed::<i64>(1)?,
                        row.get_typed::<Option<i64>>(2)?,
                        row.get_typed::<Option<i64>>(3)?,
                    ))
                },
            )?;

        // Query 2: message counts for every workspace.
        let mut messages_by_workspace: HashMap<String, i64> = HashMap::new();
        let ws_msg_rows: Vec<(String, i64)> = self.db.query_map_collect(
            "SELECT c.workspace, COUNT(m.id)
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             WHERE c.workspace IS NOT NULL
             GROUP BY c.workspace",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
        )?;
        for (workspace, msg_count) in ws_msg_rows {
            messages_by_workspace.insert(workspace, msg_count);
        }

        // Query 3: distinct agents for every workspace.
        let mut agents_by_workspace: HashMap<String, Vec<String>> = HashMap::new();
        let ws_agent_rows: Vec<(String, String)> = self.db.query_map_collect(
            "SELECT workspace, agent
             FROM conversations
             WHERE workspace IS NOT NULL
             GROUP BY workspace, agent
             ORDER BY workspace, agent",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<String>(1)?)),
        )?;
        for (workspace, agent) in ws_agent_rows {
            agents_by_workspace
                .entry(workspace)
                .or_default()
                .push(agent);
        }

        // Query 4: recent titles per workspace (sorted by started_at DESC, top 5 per workspace in Rust).
        let mut recent_titles_by_workspace: HashMap<String, Vec<String>> = HashMap::new();
        let ws_title_rows: Vec<(String, String)> = self.db.query_map_collect(
            "SELECT workspace, title
             FROM conversations
             WHERE workspace IS NOT NULL AND title IS NOT NULL
             ORDER BY workspace, started_at DESC",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<String>(1)?)),
        )?;
        for (workspace, title) in ws_title_rows {
            let titles = recent_titles_by_workspace.entry(workspace).or_default();
            if titles.len() < 5 {
                titles.push(title);
            }
        }

        for (workspace, conv_count, min_ts, max_ts) in workspace_rows {
            let msg_count = messages_by_workspace.get(&workspace).copied().unwrap_or(0);
            let agents = agents_by_workspace.remove(&workspace).unwrap_or_default();
            let recent_titles = recent_titles_by_workspace
                .remove(&workspace)
                .unwrap_or_default();

            // Extract display name (last path component)
            let display_name = Path::new(&workspace)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| workspace.clone());

            workspaces.push(WorkspaceEntry {
                path: workspace,
                display_name,
                conversations: conv_count as usize,
                messages: msg_count as usize,
                agents,
                date_range: TimeRange {
                    earliest: min_ts
                        .and_then(DateTime::from_timestamp_millis)
                        .map(|dt| dt.to_rfc3339()),
                    latest: max_ts
                        .and_then(DateTime::from_timestamp_millis)
                        .map(|dt| dt.to_rfc3339()),
                },
                recent_titles,
            });
        }

        info!(
            query_count = 4,
            workspace_rows = workspaces.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "Workspace summary generated using set-based aggregation"
        );

        Ok(WorkspaceSummary { workspaces })
    }

    /// Generate agent summary.
    fn generate_agent_summary(&self) -> Result<AgentSummary> {
        info!("Generating agent summary...");
        let started = Instant::now();

        let mut agents: Vec<AgentEntry> = Vec::new();

        // Query 1: base agent rows with conversation/time aggregates.
        let agent_rows: Vec<(String, i64, Option<i64>, Option<i64>)> = self.db.query_map_collect(
            "SELECT agent, COUNT(*) as conv_count,
                    MIN(started_at), MAX(started_at)
             FROM conversations
             GROUP BY agent
             ORDER BY conv_count DESC",
            &[],
            |row: &Row| {
                Ok((
                    row.get_typed::<String>(0)?,
                    row.get_typed::<i64>(1)?,
                    row.get_typed::<Option<i64>>(2)?,
                    row.get_typed::<Option<i64>>(3)?,
                ))
            },
        )?;

        // Query 2: message counts for every agent.
        let mut messages_by_agent: HashMap<String, i64> = HashMap::new();
        let agent_msg_rows: Vec<(String, i64)> = self.db.query_map_collect(
            "SELECT c.agent, COUNT(m.id)
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             GROUP BY c.agent",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
        )?;
        for (agent, msg_count) in agent_msg_rows {
            messages_by_agent.insert(agent, msg_count);
        }

        // Query 3: distinct workspaces for every agent.
        let mut workspaces_by_agent: HashMap<String, Vec<String>> = HashMap::new();
        let agent_ws_rows: Vec<(String, String)> = self.db.query_map_collect(
            "SELECT agent, workspace
             FROM conversations
             WHERE workspace IS NOT NULL
             GROUP BY agent, workspace
             ORDER BY agent, workspace",
            &[],
            |row: &Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<String>(1)?)),
        )?;
        for (agent, workspace) in agent_ws_rows {
            workspaces_by_agent
                .entry(agent)
                .or_default()
                .push(workspace);
        }

        for (agent, conv_count, min_ts, max_ts) in agent_rows {
            let msg_count = messages_by_agent.get(&agent).copied().unwrap_or(0);
            let workspaces = workspaces_by_agent.remove(&agent).unwrap_or_default();

            let avg_messages = if conv_count > 0 {
                msg_count as f64 / conv_count as f64
            } else {
                0.0
            };

            agents.push(AgentEntry {
                name: agent,
                conversations: conv_count as usize,
                messages: msg_count as usize,
                workspaces,
                date_range: TimeRange {
                    earliest: min_ts
                        .and_then(DateTime::from_timestamp_millis)
                        .map(|dt| dt.to_rfc3339()),
                    latest: max_ts
                        .and_then(DateTime::from_timestamp_millis)
                        .map(|dt| dt.to_rfc3339()),
                },
                avg_messages_per_conversation: avg_messages,
            });
        }

        info!(
            query_count = 3,
            agent_rows = agents.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "Agent summary generated using set-based aggregation"
        );

        Ok(AgentSummary { agents })
    }

    /// Generate top terms from conversation titles.
    fn generate_top_terms(&self) -> Result<TopTerms> {
        info!("Generating top terms...");

        let stop_words: HashSet<&str> = STOP_WORDS.iter().copied().collect();

        // Get all titles
        let titles: Vec<String> = self.db.query_map_collect(
            "SELECT title FROM conversations WHERE title IS NOT NULL",
            &[],
            |row: &Row| row.get_typed::<String>(0),
        )?;

        let mut term_counts: HashMap<String, usize> = HashMap::new();

        for title in titles {
            for word in title.split_whitespace() {
                // Clean the word: remove punctuation, lowercase
                let word: String = word
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .collect::<String>()
                    .to_lowercase();

                // Filter: minimum length 3, not a stop word
                if word.len() >= 3 && !stop_words.contains(word.as_str()) {
                    *term_counts.entry(word).or_insert(0) += 1;
                }
            }
        }

        // Sort by count descending
        let mut top: Vec<(String, usize)> = term_counts.into_iter().collect();
        top.sort_by_key(|entry| std::cmp::Reverse(entry.1));

        // Keep top 100
        top.truncate(100);

        Ok(TopTerms { terms: top })
    }
}

/// Aggregate daily entries to weekly.
pub fn aggregate_to_weekly(daily: &[DailyEntry]) -> Vec<WeeklyEntry> {
    let mut weekly_map: HashMap<String, WeeklyEntry> = HashMap::new();

    for entry in daily {
        // Parse date and get ISO week
        if let Ok(date) = NaiveDate::parse_from_str(&entry.date, "%Y-%m-%d") {
            let iso_week = date.iso_week();
            let week_str = format!("{}-W{:02}", iso_week.year(), iso_week.week());

            let weekly = weekly_map.entry(week_str.clone()).or_insert(WeeklyEntry {
                week: week_str,
                messages: 0,
                conversations: 0,
            });
            weekly.messages += entry.messages;
            weekly.conversations += entry.conversations;
        }
    }

    let mut result: Vec<WeeklyEntry> = weekly_map.into_values().collect();
    result.sort_by(|a, b| a.week.cmp(&b.week));
    result
}

/// Aggregate daily entries to monthly.
pub fn aggregate_to_monthly(daily: &[DailyEntry]) -> Vec<MonthlyEntry> {
    let mut monthly_map: HashMap<String, MonthlyEntry> = HashMap::new();

    for entry in daily {
        // Extract YYYY-MM from date
        if entry.date.len() >= 7 {
            let month_str = entry.date[..7].to_string();

            let monthly = monthly_map
                .entry(month_str.clone())
                .or_insert(MonthlyEntry {
                    month: month_str,
                    messages: 0,
                    conversations: 0,
                });
            monthly.messages += entry.messages;
            monthly.conversations += entry.conversations;
        }
    }

    let mut result: Vec<MonthlyEntry> = monthly_map.into_values().collect();
    result.sort_by(|a, b| a.month.cmp(&b.month));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();

        // Create schema
        conn.execute_batch(
            "CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent TEXT NOT NULL,
                workspace TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                message_count INTEGER,
                metadata_json TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );",
        )
        .unwrap();

        (dir, conn)
    }

    fn insert_test_data(conn: &Connection) {
        // Insert conversations
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (1, 'claude-code', '/home/user/project-a', 'Debug authentication flow', '/path/a.jsonl', 1700000000000, 5)",
        ).unwrap();
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (2, 'claude-code', '/home/user/project-a', 'Fix database connection', '/path/b.jsonl', 1700100000000, 3)",
        ).unwrap();
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (3, 'codex', '/home/user/project-b', 'Add user authentication', '/path/c.jsonl', 1700200000000, 4)",
        ).unwrap();

        // Insert messages
        for conv_id in 1..=3 {
            let msg_count = match conv_id {
                1 => 5,
                2 => 3,
                3 => 4,
                _ => 0,
            };
            for idx in 0..msg_count {
                let role = if idx % 2 == 0 { "user" } else { "assistant" };
                let created_at =
                    1700000000000i64 + (conv_id as i64 * 100000000) + (idx as i64 * 1000);
                let content = format!("Message {} for conv {}", idx, conv_id);
                conn.execute_compat(
                    "INSERT INTO messages (conversation_id, idx, role, content, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    frankensqlite::params![
                        conv_id as i64,
                        idx as i64,
                        role,
                        content.as_str(),
                        created_at
                    ],
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn test_statistics_generation() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let stats = generator.generate_statistics().unwrap();

        assert_eq!(stats.total_conversations, 3);
        assert_eq!(stats.total_messages, 12); // 5 + 3 + 4
        assert!(stats.agents.contains_key("claude-code"));
        assert!(stats.agents.contains_key("codex"));
        assert_eq!(stats.agents["claude-code"].conversations, 2);
        assert_eq!(stats.agents["codex"].conversations, 1);
    }

    #[test]
    fn test_timeline_aggregation() {
        let daily = vec![
            DailyEntry {
                date: "2024-01-01".into(),
                messages: 10,
                conversations: 1,
            },
            DailyEntry {
                date: "2024-01-02".into(),
                messages: 20,
                conversations: 2,
            },
            DailyEntry {
                date: "2024-01-08".into(),
                messages: 15,
                conversations: 1,
            },
        ];

        let weekly = aggregate_to_weekly(&daily);
        assert_eq!(weekly.len(), 2); // Week 1 and Week 2

        let monthly = aggregate_to_monthly(&daily);
        assert_eq!(monthly.len(), 1);
        assert_eq!(monthly[0].messages, 45); // 10 + 20 + 15
    }

    #[test]
    fn test_top_terms_extraction() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let top = generator.generate_top_terms().unwrap();

        // "authentication" appears in 2 titles
        assert!(
            top.terms
                .iter()
                .any(|(term, count)| term == "authentication" && *count >= 2)
        );
    }

    #[test]
    fn test_workspace_summary() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let summary = generator.generate_workspace_summary().unwrap();

        assert_eq!(summary.workspaces.len(), 2);

        // project-a has 2 conversations
        let project_a = summary
            .workspaces
            .iter()
            .find(|w| w.path.contains("project-a"));
        assert!(project_a.is_some());
        assert_eq!(project_a.unwrap().conversations, 2);
    }

    #[test]
    fn test_agent_summary() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let summary = generator.generate_agent_summary().unwrap();

        assert_eq!(summary.agents.len(), 2);

        let claude = summary.agents.iter().find(|a| a.name == "claude-code");
        assert!(claude.is_some());
        assert_eq!(claude.unwrap().conversations, 2);
        assert_eq!(claude.unwrap().messages, 8); // 5 + 3
    }

    #[test]
    fn test_workspace_summary_distinct_agents_and_recent_titles() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let summary = generator.generate_workspace_summary().unwrap();

        let project_a = summary
            .workspaces
            .iter()
            .find(|w| w.path == "/home/user/project-a")
            .expect("project-a workspace should exist");

        assert_eq!(project_a.messages, 8); // 5 + 3
        assert_eq!(project_a.agents, vec!["claude-code".to_string()]);
        assert_eq!(project_a.recent_titles.len(), 2);
        assert_eq!(
            project_a.recent_titles.first().map(String::as_str),
            Some("Fix database connection")
        );
    }

    #[test]
    fn test_agent_summary_high_cardinality_distribution() {
        let (_dir, conn) = create_test_db();

        let mut conv_id: i64 = 1;

        // High-cardinality main agent across many workspaces.
        for i in 0..40 {
            let workspace = format!("/home/user/ws-{}", i % 10);
            let started_at = 1_700_000_000_000i64 + i as i64 * 1_000;
            let title = format!("Claude conversation {}", i);
            let source = format!("/path/{}.jsonl", conv_id);
            conn.execute_compat(
                "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
                 VALUES (?1, 'claude-code', ?2, ?3, ?4, ?5, 1)",
                frankensqlite::params![
                    conv_id,
                    workspace.as_str(),
                    title.as_str(),
                    source.as_str(),
                    started_at
                ],
            )
            .unwrap();
            let content = format!("message {}", i);
            conn.execute_compat(
                "INSERT INTO messages (conversation_id, idx, role, content, created_at)
                 VALUES (?1, 0, 'assistant', ?2, ?3)",
                frankensqlite::params![conv_id, content.as_str(), started_at],
            )
            .unwrap();
            conv_id += 1;
        }

        // Secondary agent with lower cardinality.
        for i in 0..5 {
            let started_at = 1_700_100_000_000i64 + i as i64 * 1_000;
            let title = format!("Codex conversation {}", i);
            let source = format!("/path/{}.jsonl", conv_id);
            conn.execute_compat(
                "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
                 VALUES (?1, 'codex', '/home/user/codex-ws', ?2, ?3, ?4, 1)",
                frankensqlite::params![
                    conv_id,
                    title.as_str(),
                    source.as_str(),
                    started_at
                ],
            )
            .unwrap();
            let content = format!("codex {}", i);
            conn.execute_compat(
                "INSERT INTO messages (conversation_id, idx, role, content, created_at)
                 VALUES (?1, 0, 'assistant', ?2, ?3)",
                frankensqlite::params![conv_id, content.as_str(), started_at],
            )
            .unwrap();
            conv_id += 1;
        }

        let generator = AnalyticsGenerator::new(&conn);
        let summary = generator.generate_agent_summary().unwrap();

        let claude = summary
            .agents
            .iter()
            .find(|a| a.name == "claude-code")
            .expect("claude-code agent should exist");
        assert_eq!(claude.conversations, 40);
        assert_eq!(claude.messages, 40);
        assert_eq!(claude.workspaces.len(), 10);
        assert!((claude.avg_messages_per_conversation - 1.0).abs() < f64::EPSILON);

        let codex = summary
            .agents
            .iter()
            .find(|a| a.name == "codex")
            .expect("codex agent should exist");
        assert_eq!(codex.conversations, 5);
        assert_eq!(codex.messages, 5);
    }

    #[test]
    fn test_bundle_write() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let bundle = generator.generate_all().unwrap();

        let output_dir = TempDir::new().unwrap();
        bundle.write_to_dir(output_dir.path()).unwrap();

        // Verify files exist
        assert!(output_dir.path().join("statistics.json").exists());
        assert!(output_dir.path().join("timeline.json").exists());
        assert!(output_dir.path().join("workspace_summary.json").exists());
        assert!(output_dir.path().join("agent_summary.json").exists());
        assert!(output_dir.path().join("top_terms.json").exists());
    }

    #[test]
    fn test_generate_all() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = AnalyticsGenerator::new(&conn);
        let bundle = generator.generate_all().unwrap();

        // Verify bundle contains all parts
        assert_eq!(bundle.statistics.total_conversations, 3);
        assert!(!bundle.timeline.daily.is_empty() || bundle.timeline.monthly.is_empty());
        assert!(!bundle.workspace_summary.workspaces.is_empty());
        assert!(!bundle.agent_summary.agents.is_empty());
        // top_terms might be empty depending on stop word filtering
    }

    #[test]
    fn test_empty_database() {
        let (_dir, conn) = create_test_db();
        // Don't insert any data

        let generator = AnalyticsGenerator::new(&conn);
        let bundle = generator.generate_all().unwrap();

        assert_eq!(bundle.statistics.total_conversations, 0);
        assert_eq!(bundle.statistics.total_messages, 0);
        assert!(bundle.timeline.daily.is_empty());
        assert!(bundle.workspace_summary.workspaces.is_empty());
        assert!(bundle.agent_summary.agents.is_empty());
        assert!(bundle.top_terms.terms.is_empty());
    }
}
