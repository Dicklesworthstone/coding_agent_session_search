//! Pre-Publish Summary generation for pages export.
//!
//! Generates a comprehensive summary of all content that will be published,
//! enabling users to review and modify their selection before proceeding.
//!
//! # Overview
//!
//! The summary provides:
//! - **Quantitative metrics**: Total conversations, messages, and estimated size
//! - **Temporal scope**: Date range and activity histogram
//! - **Content categorization**: Breakdown by workspace and agent
//! - **Security status**: Encryption configuration and secret scan results
//!
//! # Example
//!
//! ```ignore
//! use crate::pages::summary::{PrePublishSummary, SummaryGenerator};
//!
//! let generator = SummaryGenerator::new(&db_conn);
//! let summary = generator.generate(None)?;
//! println!("{}", summary.render_overview());
//! ```

use crate::pages::encrypt::{KeySlot, SlotType};
use crate::pages::secret_scan::{SecretScanReport, SecretScanSummary};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use frankensqlite::Connection;
use frankensqlite::Row;
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Pre-publish summary containing all information about content to be exported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrePublishSummary {
    // Quantitative metrics
    /// Total number of conversations to be exported.
    pub total_conversations: usize,
    /// Total number of messages across all conversations.
    pub total_messages: usize,
    /// Total character count of all message content.
    pub total_characters: usize,
    /// Estimated size in bytes after compression and encryption.
    pub estimated_size_bytes: usize,

    // Temporal scope
    /// Earliest timestamp in the export set.
    pub earliest_timestamp: Option<DateTime<Utc>>,
    /// Latest timestamp in the export set.
    pub latest_timestamp: Option<DateTime<Utc>>,
    /// Histogram of messages per day.
    pub date_histogram: Vec<DateHistogramEntry>,

    // Content categorization
    /// Per-workspace breakdown.
    pub workspaces: Vec<WorkspaceSummaryItem>,
    /// Per-agent breakdown.
    pub agents: Vec<AgentSummaryItem>,

    // Security status
    /// Summary of secret scan results.
    pub secret_scan: ScanReportSummary,
    /// Encryption configuration summary.
    pub encryption_config: Option<EncryptionSummary>,
    /// Key slots configured for the export.
    pub key_slots: Vec<KeySlotSummary>,

    /// When this summary was generated.
    pub generated_at: DateTime<Utc>,
}

/// Entry in the date histogram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateHistogramEntry {
    /// Date in YYYY-MM-DD format.
    pub date: String,
    /// Number of messages on this date.
    pub message_count: usize,
    /// Number of unique conversations active on this date.
    pub conversation_count: usize,
}

/// Summary of a workspace's content in the export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummaryItem {
    /// Full path of the workspace.
    pub path: String,
    /// Display name (last path component).
    pub display_name: String,
    /// Number of conversations in this workspace.
    pub conversation_count: usize,
    /// Number of messages in this workspace.
    pub message_count: usize,
    /// Date range of conversations in this workspace.
    pub date_range: DateRange,
    /// Sample of conversation titles (first 5).
    pub sample_titles: Vec<String>,
    /// Whether this workspace is included in export.
    pub included: bool,
}

/// Summary of an agent's content in the export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummaryItem {
    /// Agent identifier (e.g., "claude-code", "aider").
    pub name: String,
    /// Number of conversations from this agent.
    pub conversation_count: usize,
    /// Number of messages from this agent.
    pub message_count: usize,
    /// Percentage of total conversations.
    pub percentage: f64,
    /// Whether this agent is included in export.
    pub included: bool,
}

/// Date range with optional bounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateRange {
    /// Earliest timestamp (RFC3339).
    pub earliest: Option<String>,
    /// Latest timestamp (RFC3339).
    pub latest: Option<String>,
}

impl DateRange {
    /// Create a new date range from optional timestamps.
    pub fn from_timestamps(earliest: Option<i64>, latest: Option<i64>) -> Self {
        Self {
            earliest: earliest
                .and_then(DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339()),
            latest: latest
                .and_then(DateTime::from_timestamp_millis)
                .map(|dt| dt.to_rfc3339()),
        }
    }

    /// Get the span in days, if both bounds are present.
    pub fn span_days(&self) -> Option<i64> {
        let earliest = self.earliest.as_ref()?.parse::<DateTime<Utc>>().ok()?;
        let latest = self.latest.as_ref()?.parse::<DateTime<Utc>>().ok()?;
        Some((latest - earliest).num_days())
    }
}

/// Summary of secret scan results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReportSummary {
    /// Total number of findings.
    pub total_findings: usize,
    /// Breakdown by severity.
    pub by_severity: HashMap<String, usize>,
    /// Whether any critical secrets were found.
    pub has_critical: bool,
    /// Whether findings were truncated due to max limit.
    pub truncated: bool,
    /// Status message for display.
    pub status_message: String,
}

impl ScanReportSummary {
    /// Create from a full secret scan report.
    pub fn from_report(report: &SecretScanReport) -> Self {
        let by_severity: HashMap<String, usize> = report
            .summary
            .by_severity
            .iter()
            .map(|(k, v)| (k.label().to_string(), *v))
            .collect();

        let status_message = if report.summary.total == 0 {
            "No secrets detected".to_string()
        } else if report.summary.has_critical {
            format!("{} issues found (including CRITICAL)", report.summary.total)
        } else {
            format!("{} issues found", report.summary.total)
        };

        Self {
            total_findings: report.summary.total,
            by_severity,
            has_critical: report.summary.has_critical,
            truncated: report.summary.truncated,
            status_message,
        }
    }

    /// Create from a summary only.
    pub fn from_summary(summary: &SecretScanSummary) -> Self {
        let by_severity: HashMap<String, usize> = summary
            .by_severity
            .iter()
            .map(|(k, v)| (k.label().to_string(), *v))
            .collect();

        let status_message = if summary.total == 0 {
            "No secrets detected".to_string()
        } else if summary.has_critical {
            format!("{} issues found (including CRITICAL)", summary.total)
        } else {
            format!("{} issues found", summary.total)
        };

        Self {
            total_findings: summary.total,
            by_severity,
            has_critical: summary.has_critical,
            truncated: summary.truncated,
            status_message,
        }
    }
}

/// Summary of encryption configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionSummary {
    /// Encryption algorithm used.
    pub algorithm: String,
    /// Key derivation function.
    pub key_derivation: String,
    /// Number of key slots configured.
    pub key_slot_count: usize,
    /// Estimated decryption time (for display).
    pub estimated_decrypt_time_secs: u64,
}

impl Default for EncryptionSummary {
    fn default() -> Self {
        Self {
            algorithm: "AES-256-GCM".to_string(),
            key_derivation: "Argon2id".to_string(),
            key_slot_count: 0,
            estimated_decrypt_time_secs: 2,
        }
    }
}

/// Type of key slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySlotType {
    Password,
    QrCode,
    Recovery,
}

impl From<SlotType> for KeySlotType {
    fn from(st: SlotType) -> Self {
        match st {
            SlotType::Password => KeySlotType::Password,
            SlotType::Recovery => KeySlotType::Recovery,
        }
    }
}

impl KeySlotType {
    /// Display label for the slot type.
    pub fn label(self) -> &'static str {
        match self {
            KeySlotType::Password => "Password",
            KeySlotType::QrCode => "QR Code",
            KeySlotType::Recovery => "Recovery Key",
        }
    }
}

/// Summary of a key slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeySlotSummary {
    /// Slot index (0-based).
    pub slot_index: usize,
    /// Type of the slot.
    pub slot_type: KeySlotType,
    /// Optional hint for the slot.
    pub hint: Option<String>,
    /// When the slot was created.
    pub created_at: Option<DateTime<Utc>>,
}

impl KeySlotSummary {
    /// Create from a KeySlot.
    pub fn from_key_slot(slot: &KeySlot, index: usize) -> Self {
        Self {
            slot_index: index,
            slot_type: slot.slot_type.into(),
            hint: None, // Hints not stored in KeySlot currently
            created_at: None,
        }
    }
}

/// Set of exclusions to apply before export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExclusionSet {
    /// Workspaces to exclude (full paths).
    pub excluded_workspaces: HashSet<String>,
    /// Conversation IDs to exclude.
    pub excluded_conversations: HashSet<i64>,
    /// Patterns to match against titles for exclusion.
    #[serde(skip)]
    pub excluded_patterns: Vec<Regex>,
    /// Raw pattern strings (for serialization).
    pub excluded_pattern_strings: Vec<String>,
}

impl ExclusionSet {
    /// Create a new empty exclusion set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a workspace to exclusions.
    pub fn exclude_workspace(&mut self, workspace: &str) {
        self.excluded_workspaces.insert(workspace.to_string());
    }

    /// Remove a workspace from exclusions.
    pub fn include_workspace(&mut self, workspace: &str) {
        self.excluded_workspaces.remove(workspace);
    }

    /// Add a conversation to exclusions.
    pub fn exclude_conversation(&mut self, conversation_id: i64) {
        self.excluded_conversations.insert(conversation_id);
    }

    /// Remove a conversation from exclusions.
    pub fn include_conversation(&mut self, conversation_id: i64) {
        self.excluded_conversations.remove(&conversation_id);
    }

    /// Check if a workspace is excluded.
    pub fn is_workspace_excluded(&self, workspace: &str) -> bool {
        self.excluded_workspaces.contains(workspace)
    }

    /// Check if a conversation is excluded.
    pub fn is_conversation_excluded(&self, conversation_id: i64) -> bool {
        self.excluded_conversations.contains(&conversation_id)
    }

    /// Check if a title matches any exclusion pattern.
    pub fn is_excluded(&self, title: &str) -> bool {
        self.excluded_patterns.iter().any(|re| re.is_match(title))
    }

    /// Add a title pattern to exclusions.
    pub fn add_pattern(&mut self, pattern: &str) -> Result<()> {
        let regex = Regex::new(pattern).context("Invalid exclusion pattern")?;
        self.excluded_patterns.push(regex);
        self.excluded_pattern_strings.push(pattern.to_string());
        Ok(())
    }

    /// Check if an item should be excluded based on all criteria.
    pub fn should_exclude(
        &self,
        workspace: Option<&str>,
        conversation_id: i64,
        title: &str,
    ) -> bool {
        if let Some(ws) = workspace
            && self.is_workspace_excluded(ws)
        {
            return true;
        }
        if self.is_conversation_excluded(conversation_id) {
            return true;
        }
        self.is_excluded(title)
    }

    /// Get the count of excluded items.
    pub fn exclusion_counts(&self) -> (usize, usize, usize) {
        (
            self.excluded_workspaces.len(),
            self.excluded_conversations.len(),
            self.excluded_patterns.len(),
        )
    }

    /// Check if any exclusions are active.
    pub fn has_exclusions(&self) -> bool {
        !self.excluded_workspaces.is_empty()
            || !self.excluded_conversations.is_empty()
            || !self.excluded_patterns.is_empty()
    }

    /// Re-compile patterns from strings (for deserialization).
    pub fn compile_patterns(&mut self) -> Result<()> {
        self.excluded_patterns.clear();
        for pattern_str in &self.excluded_pattern_strings {
            let regex = Regex::new(pattern_str)
                .with_context(|| format!("Invalid exclusion pattern: {}", pattern_str))?;
            self.excluded_patterns.push(regex);
        }
        Ok(())
    }
}

/// Filters for summary generation.
#[derive(Debug, Clone, Default)]
pub struct SummaryFilters {
    /// Filter to specific agents.
    pub agents: Option<Vec<String>>,
    /// Filter to specific workspaces.
    pub workspaces: Option<Vec<String>>,
    /// Filter to conversations after this timestamp (millis).
    pub since_ts: Option<i64>,
    /// Filter to conversations before this timestamp (millis).
    pub until_ts: Option<i64>,
}

/// Generator for pre-publish summaries.
pub struct SummaryGenerator<'a> {
    db: &'a Connection,
}

impl<'a> SummaryGenerator<'a> {
    /// Create a new summary generator.
    pub fn new(db: &'a Connection) -> Self {
        Self { db }
    }

    /// Generate a pre-publish summary with optional filters.
    pub fn generate(&self, filters: Option<&SummaryFilters>) -> Result<PrePublishSummary> {
        let filters = filters.cloned().unwrap_or_default();

        // Build WHERE clause for filters
        let (where_clause, params) = self.build_filter_clause(&filters);

        // Get basic counts
        let (total_conversations, total_messages, total_characters) =
            self.get_counts(&where_clause, &params)?;

        // Get time range
        let (earliest_ts, latest_ts) = self.get_time_range(&where_clause, &params)?;

        // Get date histogram
        let date_histogram = self.get_date_histogram(&where_clause, &params)?;

        // Get workspace summary
        let workspaces = self.get_workspace_summary(&where_clause, &params)?;

        // Get agent summary
        let agents = self.get_agent_summary(&where_clause, &params, total_conversations)?;

        // Estimate size (rough: ~60% of raw character count after compression)
        let estimated_size_bytes = estimate_compressed_size(total_characters);

        Ok(PrePublishSummary {
            total_conversations,
            total_messages,
            total_characters,
            estimated_size_bytes,
            earliest_timestamp: earliest_ts.and_then(DateTime::from_timestamp_millis),
            latest_timestamp: latest_ts.and_then(DateTime::from_timestamp_millis),
            date_histogram,
            workspaces,
            agents,
            secret_scan: ScanReportSummary::default(),
            encryption_config: Some(EncryptionSummary::default()),
            key_slots: Vec::new(),
            generated_at: Utc::now(),
        })
    }

    /// Generate a summary with exclusions applied.
    pub fn generate_with_exclusions(
        &self,
        filters: Option<&SummaryFilters>,
        exclusions: &ExclusionSet,
    ) -> Result<PrePublishSummary> {
        let mut summary = self.generate(filters)?;

        // Mark excluded workspaces
        for ws in &mut summary.workspaces {
            ws.included = !exclusions.is_workspace_excluded(&ws.path);
        }

        // Recalculate totals based on included workspaces
        let included_workspaces: HashSet<_> = summary
            .workspaces
            .iter()
            .filter(|w| w.included)
            .map(|w| w.path.clone())
            .collect();

        if exclusions.has_exclusions() {
            // Recalculate counts excluding excluded items
            let (conv_count, msg_count, char_count) =
                self.recalculate_with_exclusions(filters, &included_workspaces, exclusions)?;

            summary.total_conversations = conv_count;
            summary.total_messages = msg_count;
            summary.total_characters = char_count;
            summary.estimated_size_bytes = estimate_compressed_size(char_count);
        }

        Ok(summary)
    }

    /// Build filter WHERE clause.
    fn build_filter_clause(&self, filters: &SummaryFilters) -> (String, Vec<ParamValue>) {
        let mut clauses = Vec::new();
        let mut params: Vec<ParamValue> = Vec::new();

        if let Some(agents) = &filters.agents
            && !agents.is_empty()
        {
            let placeholders: Vec<&str> = (0..agents.len()).map(|_| "?").collect();
            clauses.push(format!("c.agent IN ({})", placeholders.join(", ")));
            for agent in agents {
                params.push(ParamValue::from(agent.as_str()));
            }
        }

        if let Some(workspaces) = &filters.workspaces
            && !workspaces.is_empty()
        {
            let placeholders: Vec<&str> = (0..workspaces.len()).map(|_| "?").collect();
            clauses.push(format!("c.workspace IN ({})", placeholders.join(", ")));
            for ws in workspaces {
                params.push(ParamValue::from(ws.as_str()));
            }
        }

        if let Some(since) = filters.since_ts {
            clauses.push("c.started_at >= ?".to_string());
            params.push(ParamValue::from(since));
        }

        if let Some(until) = filters.until_ts {
            clauses.push("c.started_at <= ?".to_string());
            params.push(ParamValue::from(until));
        }

        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" AND {}", clauses.join(" AND "))
        };

        (where_clause, params)
    }

    /// Build SQL params for queries that prepend one local value before filter params.
    fn prepend_params(first: ParamValue, params: &[ParamValue]) -> Vec<ParamValue> {
        std::iter::once(first)
            .chain(params.iter().cloned())
            .collect()
    }

    /// Get basic counts.
    fn get_counts(
        &self,
        where_clause: &str,
        params: &[ParamValue],
    ) -> Result<(usize, usize, usize)> {
        // Count conversations
        let conv_query = format!(
            "SELECT COUNT(*) FROM conversations c WHERE 1=1{}",
            where_clause
        );
        let total_conversations: i64 = self
            .db
            .query_row_map(&conv_query, params, |row: &Row| row.get_typed(0))
            .context("Failed to count conversations")?;

        // Count messages and characters using subquery to avoid
        // JOIN + aggregate without GROUP BY (frankensqlite limitation).
        let msg_query = format!(
            "SELECT COUNT(*), SUM(LENGTH(content))
             FROM messages
             WHERE conversation_id IN (SELECT c.id FROM conversations c WHERE 1=1{})",
            where_clause
        );
        let (total_messages, total_characters): (i64, i64) = self
            .db
            .query_map_collect(&msg_query, params, |row: &Row| {
                Ok((
                    row.get_typed::<Option<i64>>(0)?.unwrap_or(0),
                    row.get_typed::<Option<i64>>(1)?.unwrap_or(0),
                ))
            })
            .context("Failed to count messages")?
            .into_iter()
            .next()
            .unwrap_or((0, 0));

        Ok((
            total_conversations as usize,
            total_messages as usize,
            total_characters as usize,
        ))
    }

    /// Get time range.
    fn get_time_range(
        &self,
        where_clause: &str,
        params: &[ParamValue],
    ) -> Result<(Option<i64>, Option<i64>)> {
        let query = format!(
            "SELECT MIN(c.started_at), MAX(c.started_at) FROM conversations c WHERE 1=1{}",
            where_clause
        );
        let result: (Option<i64>, Option<i64>) = self
            .db
            .query_row_map(&query, params, |row: &Row| {
                Ok((row.get_typed(0)?, row.get_typed(1)?))
            })
            .context("Failed to get time range")?;
        Ok(result)
    }

    /// Get date histogram.
    fn get_date_histogram(
        &self,
        where_clause: &str,
        params: &[ParamValue],
    ) -> Result<Vec<DateHistogramEntry>> {
        // Use integer day computation instead of DATE() which isn't supported
        // by frankensqlite. The day_epoch is seconds-since-epoch / 86400.
        // Use subquery instead of JOIN to avoid frankensqlite aggregate limitation.
        let query = format!(
            "SELECT created_at / 1000 / 86400,
                    COUNT(*)
             FROM messages
             WHERE created_at IS NOT NULL
               AND conversation_id IN (SELECT c.id FROM conversations c WHERE 1=1{})
             GROUP BY created_at / 1000 / 86400
             ORDER BY created_at / 1000 / 86400",
            where_clause
        );

        // Count distinct conversations per day using a subquery approach.
        let conv_query = format!(
            "SELECT day_epoch, COUNT(*)
             FROM (
                SELECT DISTINCT conversation_id, created_at / 1000 / 86400 AS day_epoch
                FROM messages
                WHERE created_at IS NOT NULL
                  AND conversation_id IN (SELECT c.id FROM conversations c WHERE 1=1{})
             )
             GROUP BY day_epoch",
            where_clause
        );

        let day_msg_rows = self.db.query_map_collect(&query, params, |row: &Row| {
            let day_epoch: i64 = row.get_typed::<Option<i64>>(0)?.unwrap_or(0);
            let msg_count: i64 = row.get_typed::<Option<i64>>(1)?.unwrap_or(0);
            Ok((day_epoch, msg_count as usize))
        })?;

        let day_conv_rows = self
            .db
            .query_map_collect(&conv_query, params, |row: &Row| {
                let day_epoch: i64 = row.get_typed::<Option<i64>>(0)?.unwrap_or(0);
                let conv_count: i64 = row.get_typed::<Option<i64>>(1)?.unwrap_or(0);
                Ok((day_epoch, conv_count as usize))
            })?;

        let conv_map: std::collections::HashMap<i64, usize> = day_conv_rows.into_iter().collect();

        use chrono::{NaiveDate, TimeDelta};
        let epoch_base = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let entries: Vec<DateHistogramEntry> = day_msg_rows
            .into_iter()
            .map(|(day_epoch, message_count)| {
                let date = epoch_base
                    .checked_add_signed(TimeDelta::days(day_epoch))
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| format!("{day_epoch}"));
                DateHistogramEntry {
                    date,
                    message_count,
                    conversation_count: conv_map.get(&day_epoch).copied().unwrap_or(0),
                }
            })
            .collect();
        Ok(entries)
    }

    /// Get workspace summary.
    fn get_workspace_summary(
        &self,
        where_clause: &str,
        params: &[ParamValue],
    ) -> Result<Vec<WorkspaceSummaryItem>> {
        let query = format!(
            "SELECT c.workspace, COUNT(*) as conv_count,
                    MIN(c.started_at), MAX(c.started_at)
             FROM conversations c
             WHERE c.workspace IS NOT NULL{}
             GROUP BY c.workspace
             ORDER BY conv_count DESC",
            where_clause
        );

        let ws_rows = self.db.query_map_collect(&query, params, |row: &Row| {
            Ok((
                row.get_typed::<String>(0)?,
                row.get_typed::<i64>(1)?,
                row.get_typed::<Option<i64>>(2)?,
                row.get_typed::<Option<i64>>(3)?,
            ))
        })?;

        let mut workspaces = Vec::new();
        for (workspace, conv_count, min_ts, max_ts) in ws_rows {
            // Get message count for this workspace
            let msg_query = format!(
                "SELECT COUNT(*) FROM messages
                 WHERE conversation_id IN (SELECT c.id FROM conversations c WHERE c.workspace = ?{})",
                where_clause
            );
            let prepended = Self::prepend_params(ParamValue::from(workspace.as_str()), params);
            let msg_count: i64 = self.db.query_row_map(&msg_query, &prepended, |row: &Row| {
                Ok(row.get_typed::<Option<i64>>(0)?.unwrap_or(0))
            })?;

            // Get sample titles
            let title_query = format!(
                "SELECT c.title FROM conversations c
                 WHERE c.workspace = ? AND c.title IS NOT NULL{}
                 ORDER BY c.started_at DESC LIMIT 5",
                where_clause
            );
            let title_prepended =
                Self::prepend_params(ParamValue::from(workspace.as_str()), params);
            let titles: Vec<String> =
                self.db
                    .query_map_collect(&title_query, &title_prepended, |row: &Row| {
                        row.get_typed(0)
                    })?;

            // Extract display name
            let display_name = std::path::Path::new(&workspace)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| workspace.clone());

            workspaces.push(WorkspaceSummaryItem {
                path: workspace,
                display_name,
                conversation_count: conv_count as usize,
                message_count: msg_count as usize,
                date_range: DateRange::from_timestamps(min_ts, max_ts),
                sample_titles: titles,
                included: true,
            });
        }

        Ok(workspaces)
    }

    /// Get agent summary.
    fn get_agent_summary(
        &self,
        where_clause: &str,
        params: &[ParamValue],
        total_conversations: usize,
    ) -> Result<Vec<AgentSummaryItem>> {
        let query = format!(
            "SELECT c.agent, COUNT(*) as conv_count
             FROM conversations c
             WHERE 1=1{}
             GROUP BY c.agent
             ORDER BY conv_count DESC",
            where_clause
        );

        let agent_rows = self.db.query_map_collect(&query, params, |row: &Row| {
            Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?))
        })?;

        let mut agents = Vec::new();
        for (agent, conv_count) in agent_rows {
            // Get message count
            let msg_query = format!(
                "SELECT COUNT(*) FROM messages
                 WHERE conversation_id IN (SELECT c.id FROM conversations c WHERE c.agent = ?{})",
                where_clause
            );
            let prepended = Self::prepend_params(ParamValue::from(agent.as_str()), params);
            let msg_count: i64 = self.db.query_row_map(&msg_query, &prepended, |row: &Row| {
                Ok(row.get_typed::<Option<i64>>(0)?.unwrap_or(0))
            })?;

            let percentage = if total_conversations > 0 {
                (conv_count as f64 / total_conversations as f64) * 100.0
            } else {
                0.0
            };

            agents.push(AgentSummaryItem {
                name: agent,
                conversation_count: conv_count as usize,
                message_count: msg_count as usize,
                percentage,
                included: true,
            });
        }

        Ok(agents)
    }

    /// Recalculate counts with exclusions.
    fn recalculate_with_exclusions(
        &self,
        filters: Option<&SummaryFilters>,
        included_workspaces: &HashSet<String>,
        exclusions: &ExclusionSet,
    ) -> Result<(usize, usize, usize)> {
        // Build exclusion query
        let mut conv_count = 0usize;
        let mut msg_count = 0usize;
        let mut char_count = 0usize;

        let (where_clause, params) = filters
            .map(|active_filters| self.build_filter_clause(active_filters))
            .unwrap_or_default();

        // Query conversations and filter
        let query = format!(
            "SELECT c.id, c.workspace, c.title,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id),
                    (SELECT SUM(LENGTH(content)) FROM messages WHERE conversation_id = c.id)
             FROM conversations c
             WHERE 1=1{}",
            where_clause
        );

        let conv_rows = self.db.query_map_collect(&query, &params, |row: &Row| {
            Ok((
                row.get_typed::<i64>(0)?,
                row.get_typed::<Option<String>>(1)?,
                row.get_typed::<Option<String>>(2)?,
                row.get_typed::<i64>(3)?,
                row.get_typed::<Option<i64>>(4)?.unwrap_or(0),
            ))
        })?;

        for (id, workspace, title, msgs, chars) in conv_rows {
            let title_str = title.as_deref().unwrap_or("");

            // Check exclusions
            if exclusions.should_exclude(workspace.as_deref(), id, title_str) {
                continue;
            }

            // Check workspace inclusion
            if let Some(ws) = &workspace
                && !included_workspaces.contains(ws)
            {
                continue;
            }

            conv_count += 1;
            msg_count += msgs as usize;
            char_count += chars as usize;
        }

        Ok((conv_count, msg_count, char_count))
    }
}

/// Estimate compressed size from character count.
/// Uses rough heuristic: ~40% of original after compression + encryption overhead.
pub fn estimate_compressed_size(char_count: usize) -> usize {
    let base_size = (char_count as f64 * 0.4) as usize;
    // Add ~5% for encryption overhead (nonces, auth tags, etc.)
    (base_size as f64 * 1.05) as usize
}

/// Format a byte size for display.
pub fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    const GB: usize = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

impl PrePublishSummary {
    /// Render a text overview of the summary.
    pub fn render_overview(&self) -> String {
        let mut output = String::new();

        output.push_str("CONTENT OVERVIEW\n");
        output.push_str("----------------\n");
        output.push_str(&format!("Conversations: {}\n", self.total_conversations));
        output.push_str(&format!("Messages:      {}\n", self.total_messages));
        output.push_str(&format!(
            "Characters:    {} (~{})\n",
            self.total_characters,
            format_size(self.total_characters)
        ));
        output.push_str(&format!(
            "Archive Size:  ~{} (estimated, compressed + encrypted)\n",
            format_size(self.estimated_size_bytes)
        ));
        output.push('\n');

        output.push_str("DATE RANGE\n");
        output.push_str("----------\n");
        if let (Some(earliest), Some(latest)) = (&self.earliest_timestamp, &self.latest_timestamp) {
            let days = (*latest - *earliest).num_days();
            output.push_str(&format!(
                "From: {}  To: {}  ({} days)\n",
                earliest.format("%Y-%m-%d"),
                latest.format("%Y-%m-%d"),
                days
            ));
        } else {
            output.push_str("No date information available\n");
        }
        output.push('\n');

        output.push_str(&format!("WORKSPACES ({})\n", self.workspaces.len()));
        output.push_str("--------------\n");
        for ws in self.workspaces.iter().take(5) {
            let included_marker = if ws.included { " " } else { "x" };
            output.push_str(&format!(
                "[{}] {} ({} conversations)\n",
                included_marker, ws.display_name, ws.conversation_count
            ));
            if !ws.sample_titles.is_empty() {
                let titles: Vec<_> = ws.sample_titles.iter().take(3).cloned().collect();
                output.push_str(&format!("    \"{}\"...\n", titles.join("\", \"")));
            }
        }
        if self.workspaces.len() > 5 {
            output.push_str(&format!("... and {} more\n", self.workspaces.len() - 5));
        }
        output.push('\n');

        output.push_str("AGENTS\n");
        output.push_str("------\n");
        for agent in &self.agents {
            output.push_str(&format!(
                "  {}: {} conversations ({:.0}%)\n",
                agent.name, agent.conversation_count, agent.percentage
            ));
        }
        output.push('\n');

        output.push_str("SECURITY\n");
        output.push_str("--------\n");
        if let Some(enc) = &self.encryption_config {
            output.push_str(&format!("Encryption: {}\n", enc.algorithm));
            output.push_str(&format!("Key Derivation: {}\n", enc.key_derivation));
            output.push_str(&format!("Key Slots: {}\n", enc.key_slot_count));
        }
        output.push_str(&format!(
            "Secret Scan: {}\n",
            self.secret_scan.status_message
        ));

        output
    }

    /// Get count of included workspaces.
    pub fn included_workspace_count(&self) -> usize {
        self.workspaces.iter().filter(|w| w.included).count()
    }

    /// Get count of included agents.
    pub fn included_agent_count(&self) -> usize {
        self.agents.iter().filter(|a| a.included).count()
    }

    /// Update with secret scan results.
    pub fn set_secret_scan(&mut self, report: &SecretScanReport) {
        self.secret_scan = ScanReportSummary::from_report(report);
    }

    /// Update with encryption config.
    pub fn set_encryption_config(&mut self, key_slots: &[KeySlot]) {
        let enc = EncryptionSummary {
            key_slot_count: key_slots.len(),
            ..Default::default()
        };

        self.key_slots = key_slots
            .iter()
            .enumerate()
            .map(|(i, slot)| KeySlotSummary::from_key_slot(slot, i))
            .collect();

        self.encryption_config = Some(enc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankensqlite::compat::BatchExt;
    use tempfile::TempDir;

    fn create_test_db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();

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
        use frankensqlite::compat::ConnectionExt;
        use frankensqlite::params;

        // Insert conversations
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (1, 'claude-code', '/home/user/project-a', 'Fix authentication bug', '/path/a.jsonl', 1700000000000, 5);",
        ).unwrap();
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (2, 'claude-code', '/home/user/project-a', 'Add user profile', '/path/b.jsonl', 1700100000000, 3);",
        ).unwrap();
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (3, 'aider', '/home/user/project-b', 'Setup database', '/path/c.jsonl', 1700200000000, 4);",
        ).unwrap();

        // Insert messages
        for conv_id in 1..=3i64 {
            let msg_count = match conv_id {
                1 => 5,
                2 => 3,
                3 => 4,
                _ => 0,
            };
            for idx in 0..msg_count {
                let role = if idx % 2 == 0 { "user" } else { "assistant" };
                let created_at = 1700000000000i64 + (conv_id * 100000000) + (idx as i64 * 1000);
                conn.execute_compat(
                    "INSERT INTO messages (conversation_id, idx, role, content, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        conv_id,
                        idx as i64,
                        role,
                        format!("Test message {} for conversation {}", idx, conv_id),
                        created_at
                    ],
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn test_summary_generation() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(None).unwrap();

        assert_eq!(summary.total_conversations, 3);
        assert_eq!(summary.total_messages, 12);
        assert!(summary.total_characters > 0);
        assert_eq!(summary.workspaces.len(), 2);
        assert_eq!(summary.agents.len(), 2);
    }

    #[test]
    fn test_summary_with_filters() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let filters = SummaryFilters {
            agents: Some(vec!["claude-code".to_string()]),
            ..Default::default()
        };

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(Some(&filters)).unwrap();

        assert_eq!(summary.total_conversations, 2);
        assert_eq!(summary.total_messages, 8); // 5 + 3
    }

    #[test]
    fn test_workspace_summary_message_counts_respect_time_filter() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let filters = SummaryFilters {
            since_ts: Some(1_700_050_000_000),
            ..Default::default()
        };

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(Some(&filters)).unwrap();

        let project_a = summary
            .workspaces
            .iter()
            .find(|w| w.path == "/home/user/project-a")
            .unwrap();
        assert_eq!(project_a.conversation_count, 1);
        assert_eq!(project_a.message_count, 3);
        assert!(
            project_a
                .sample_titles
                .iter()
                .all(|t| t != "Fix authentication bug")
        );
    }

    #[test]
    fn test_agent_summary_message_counts_respect_time_filter() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let filters = SummaryFilters {
            since_ts: Some(1_700_050_000_000),
            ..Default::default()
        };

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(Some(&filters)).unwrap();

        let claude = summary
            .agents
            .iter()
            .find(|a| a.name == "claude-code")
            .unwrap();
        assert_eq!(claude.conversation_count, 1);
        assert_eq!(claude.message_count, 3);
    }

    #[test]
    fn test_workspace_summary() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(None).unwrap();

        let project_a = summary
            .workspaces
            .iter()
            .find(|w| w.path.contains("project-a"));
        assert!(project_a.is_some());
        let project_a = project_a.unwrap();
        assert_eq!(project_a.conversation_count, 2);
        assert_eq!(project_a.display_name, "project-a");
        assert!(!project_a.sample_titles.is_empty());
    }

    #[test]
    fn test_agent_summary() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(None).unwrap();

        let claude = summary.agents.iter().find(|a| a.name == "claude-code");
        assert!(claude.is_some());
        let claude = claude.unwrap();
        assert_eq!(claude.conversation_count, 2);
        assert!((claude.percentage - 66.67).abs() < 1.0);
    }

    #[test]
    fn test_date_histogram() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(None).unwrap();

        // Each conversation is on a different day
        assert!(!summary.date_histogram.is_empty());
    }

    #[test]
    fn test_exclusion_set() {
        let mut exclusions = ExclusionSet::new();

        exclusions.exclude_workspace("/home/user/project-a");
        assert!(exclusions.is_workspace_excluded("/home/user/project-a"));
        assert!(!exclusions.is_workspace_excluded("/home/user/project-b"));

        exclusions.exclude_conversation(42);
        assert!(exclusions.is_conversation_excluded(42));
        assert!(!exclusions.is_conversation_excluded(43));

        exclusions.add_pattern("(?i)secret").unwrap();
        assert!(exclusions.is_excluded("This is a Secret task"));
        assert!(!exclusions.is_excluded("This is a normal task"));
    }

    #[test]
    fn test_exclusion_should_exclude() {
        let mut exclusions = ExclusionSet::new();
        exclusions.exclude_workspace("/excluded");
        exclusions.exclude_conversation(99);
        exclusions.add_pattern("^Private:").unwrap();

        // Excluded by workspace
        assert!(exclusions.should_exclude(Some("/excluded"), 1, "Normal title"));
        // Excluded by conversation ID
        assert!(exclusions.should_exclude(Some("/normal"), 99, "Normal title"));
        // Excluded by pattern
        assert!(exclusions.should_exclude(Some("/normal"), 1, "Private: Secret stuff"));
        // Not excluded
        assert!(!exclusions.should_exclude(Some("/normal"), 1, "Normal title"));
    }

    #[test]
    fn test_summary_with_exclusions() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let mut exclusions = ExclusionSet::new();
        exclusions.exclude_workspace("/home/user/project-b");

        let generator = SummaryGenerator::new(&conn);
        let summary = generator
            .generate_with_exclusions(None, &exclusions)
            .unwrap();

        // project-b should be marked as not included
        let project_b = summary
            .workspaces
            .iter()
            .find(|w| w.path.contains("project-b"));
        assert!(project_b.is_some());
        assert!(!project_b.unwrap().included);
    }

    #[test]
    fn test_size_estimation() {
        let size = estimate_compressed_size(1_000_000);
        // Should be roughly 40% * 1.05 = 42% of original
        assert!(size > 400_000);
        assert!(size < 450_000);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 bytes");
        assert_eq!(format_size(1500), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
    }

    #[test]
    fn test_date_range() {
        let range = DateRange::from_timestamps(Some(1700000000000), Some(1700100000000));
        assert!(range.earliest.is_some());
        assert!(range.latest.is_some());
        assert!(range.span_days().unwrap() >= 1);
    }

    #[test]
    fn test_scan_report_summary() {
        let summary = ScanReportSummary::default();
        assert_eq!(summary.total_findings, 0);
        assert!(!summary.has_critical);
        assert!(!summary.truncated);
    }

    #[test]
    fn test_encryption_summary() {
        let enc = EncryptionSummary::default();
        assert_eq!(enc.algorithm, "AES-256-GCM");
        assert_eq!(enc.key_derivation, "Argon2id");
    }

    #[test]
    fn test_render_overview() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(None).unwrap();
        let overview = summary.render_overview();

        assert!(overview.contains("CONTENT OVERVIEW"));
        assert!(overview.contains("Conversations: 3"));
        assert!(overview.contains("WORKSPACES"));
        assert!(overview.contains("AGENTS"));
        assert!(overview.contains("SECURITY"));
    }

    #[test]
    fn test_empty_database() {
        let (_dir, conn) = create_test_db();
        // Don't insert any data

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate(None).unwrap();

        assert_eq!(summary.total_conversations, 0);
        assert_eq!(summary.total_messages, 0);
        assert_eq!(summary.total_characters, 0);
        assert!(summary.workspaces.is_empty());
        assert!(summary.agents.is_empty());
    }

    #[test]
    fn test_key_slot_summary() {
        use crate::pages::encrypt::{KdfAlgorithm, KeySlot, SlotType};

        let slot = KeySlot {
            id: 0,
            slot_type: SlotType::Password,
            kdf: KdfAlgorithm::Argon2id,
            salt: "test".to_string(),
            wrapped_dek: "test".to_string(),
            nonce: "test".to_string(),
            argon2_params: None,
        };

        let summary = KeySlotSummary::from_key_slot(&slot, 0);
        assert_eq!(summary.slot_index, 0);
        assert_eq!(summary.slot_type, KeySlotType::Password);
    }

    #[test]
    fn test_exclusion_compile_patterns() {
        let mut exclusions = ExclusionSet::new();
        exclusions.excluded_pattern_strings = vec!["test.*pattern".to_string()];

        exclusions.compile_patterns().unwrap();

        assert_eq!(exclusions.excluded_patterns.len(), 1);
        assert!(exclusions.is_excluded("test123pattern"));
    }

    #[test]
    fn test_key_slot_type_label() {
        assert_eq!(KeySlotType::Password.label(), "Password");
        assert_eq!(KeySlotType::QrCode.label(), "QR Code");
        assert_eq!(KeySlotType::Recovery.label(), "Recovery Key");
    }

    #[test]
    fn test_exclusion_recount_keeps_workspace_less_conversations() {
        let (_dir, conn) = create_test_db();

        // Conversation without workspace should still be counted when exclusions
        // are active but do not match this conversation.
        conn.execute(
            "INSERT INTO conversations (id, agent, workspace, title, source_path, started_at, message_count)
             VALUES (10, 'codex', NULL, 'General session', '/path/no-workspace.jsonl', 1700300000000, 1);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (conversation_id, idx, role, content, created_at)
             VALUES (10, 0, 'user', 'Workspace-less message', 1700300001000);",
        )
        .unwrap();

        let mut exclusions = ExclusionSet::new();
        exclusions.add_pattern("^DOES_NOT_MATCH$").unwrap();

        let generator = SummaryGenerator::new(&conn);
        let summary = generator
            .generate_with_exclusions(None, &exclusions)
            .unwrap();

        assert_eq!(summary.total_conversations, 1);
        assert_eq!(summary.total_messages, 1);
        assert!(summary.workspaces.is_empty());
    }

    #[test]
    fn test_exclusion_recount_respects_active_filters() {
        let (_dir, conn) = create_test_db();
        insert_test_data(&conn);

        // Restrict to a single claude-code conversation in project-a.
        let filters = SummaryFilters {
            agents: Some(vec!["claude-code".to_string()]),
            since_ts: Some(1_700_050_000_000),
            ..Default::default()
        };

        let generator = SummaryGenerator::new(&conn);
        let baseline = generator.generate(Some(&filters)).unwrap();
        assert_eq!(baseline.total_conversations, 1);
        assert_eq!(baseline.total_messages, 3);

        // Trigger recount path without excluding any actual rows.
        let mut exclusions = ExclusionSet::new();
        exclusions.add_pattern("^DOES_NOT_MATCH$").unwrap();
        let summary = generator
            .generate_with_exclusions(Some(&filters), &exclusions)
            .unwrap();

        assert_eq!(summary.total_conversations, 1);
        assert_eq!(summary.total_messages, 3);
    }
}
