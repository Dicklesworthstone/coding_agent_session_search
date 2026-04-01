use crate::model::types::{Conversation, Message, MessageRole, Workspace};
use crate::storage::sqlite::FrankenStorage;
use crate::ui::components::theme::ThemePalette;
use anyhow::Result;
use frankensqlite::Row;
use frankensqlite::compat::{ConnectionExt, RowExt};
use lru::LruCache;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// -------------------------------------------------------------------------
// Input Mode
// -------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Query,
    Agent,
    Workspace,
    CreatedFrom,
    CreatedTo,
    PaneFilter,
    /// Inline find within the detail pane (local, non-indexed)
    DetailFind,
}

// -------------------------------------------------------------------------
// Conversation View
// -------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ConversationView {
    pub convo: Conversation,
    pub messages: Vec<Message>,
    pub workspace: Option<Workspace>,
}

// -------------------------------------------------------------------------
// Conversation Cache (P1 Opt 1.3)
// -------------------------------------------------------------------------

/// Cache statistics for monitoring performance.
#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub evictions: AtomicU64,
}

impl CacheStats {
    /// Get current stats as a tuple: (hits, misses, evictions).
    pub fn get(&self) -> (u64, u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.evictions.load(Ordering::Relaxed),
        )
    }

    /// Calculate hit rate as a percentage (0.0 - 1.0).
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }
}

/// Number of cache shards (must be power of 2 for efficient modulo).
const NUM_SHARDS: usize = 16;

/// Default capacity per shard.
const DEFAULT_CAPACITY_PER_SHARD: usize = 256;

/// Sharded LRU cache for ConversationView to reduce lock contention.
///
/// Caching conversation views avoids:
/// - Database queries (conversation + messages)
/// - JSON parsing (metadata_json, extra_json)
///
/// This is particularly beneficial for:
/// - TUI scrolling (repeated access to same results)
/// - Detail view expansion (view -> expand -> view pattern)
pub struct ConversationCache {
    shards: [RwLock<LruCache<u64, Arc<ConversationView>>>; NUM_SHARDS],
    stats: CacheStats,
}

impl ConversationCache {
    /// Create a new cache with the specified capacity per shard.
    pub fn new(capacity_per_shard: usize) -> Self {
        Self {
            shards: std::array::from_fn(|_| {
                RwLock::new(LruCache::new(
                    NonZeroUsize::new(capacity_per_shard).unwrap_or(NonZeroUsize::MIN),
                ))
            }),
            stats: CacheStats::default(),
        }
    }

    /// Hash a source identity to a u64 key using rustc-hash's FxHasher.
    #[inline]
    fn hash_key(source_id: Option<&str>, source_path: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        source_id.unwrap_or("").hash(&mut hasher);
        source_path.hash(&mut hasher);
        hasher.finish()
    }

    /// Get the shard index for a given hash.
    #[inline]
    fn shard_index(hash: u64) -> usize {
        (hash as usize) % NUM_SHARDS
    }

    /// Get a cached conversation view by source identity.
    pub fn get(&self, source_id: Option<&str>, source_path: &str) -> Option<Arc<ConversationView>> {
        let hash = Self::hash_key(source_id, source_path);
        let shard_idx = Self::shard_index(hash);
        let mut shard = self.shards[shard_idx].write();

        if let Some(cached) = shard.get(&hash) {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            Some(Arc::clone(cached))
        } else {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Insert a conversation view into the cache.
    pub fn insert(
        &self,
        source_id: Option<&str>,
        source_path: &str,
        view: ConversationView,
    ) -> Arc<ConversationView> {
        let hash = Self::hash_key(source_id, source_path);
        let shard_idx = Self::shard_index(hash);
        let arc = Arc::new(view);

        let mut shard = self.shards[shard_idx].write();
        // Only count eviction if shard is full AND key doesn't already exist
        if shard.len() == shard.cap().get() && !shard.contains(&hash) {
            self.stats.evictions.fetch_add(1, Ordering::Relaxed);
        }
        shard.put(hash, Arc::clone(&arc));

        arc
    }

    /// Invalidate a specific cache entry by source identity.
    pub fn invalidate(&self, source_id: Option<&str>, source_path: &str) {
        let hash = Self::hash_key(source_id, source_path);
        let shard_idx = Self::shard_index(hash);
        let mut shard = self.shards[shard_idx].write();
        shard.pop(&hash);
    }

    /// Invalidate all cache entries.
    pub fn invalidate_all(&self) {
        for shard in &self.shards {
            shard.write().clear();
        }
    }

    /// Get cache statistics.
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Get total number of cached entries across all shards.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().len()).sum()
    }

    /// Check if cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Global conversation cache instance.
pub static CONVERSATION_CACHE: Lazy<ConversationCache> = Lazy::new(|| {
    let capacity = dotenvy::var("CASS_CONV_CACHE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CAPACITY_PER_SHARD);
    ConversationCache::new(capacity)
});

// -------------------------------------------------------------------------
// Load Conversation (with caching)
// -------------------------------------------------------------------------

/// Load a conversation from the database (bypassing cache).
/// Use `load_conversation` or `load_conversation_for_source` for cached access.
pub(crate) fn load_conversation_uncached(
    storage: &FrankenStorage,
    source_id: Option<&str>,
    source_path: &str,
) -> Result<Option<ConversationView>> {
    let (sql, params) = if let Some(source_id) = source_id {
        (
            "SELECT c.id, a.slug, w.id, w.path, w.display_name, c.external_id, c.title, c.source_path,
                    c.started_at, c.ended_at, c.approx_tokens, c.metadata_json, c.source_id, c.origin_host, c.metadata_bin
             FROM conversations c
             JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             WHERE c.source_path = ?1 AND COALESCE(c.source_id, 'local') = ?2
             ORDER BY c.started_at DESC LIMIT 1",
            frankensqlite::params![source_path, source_id],
        )
    } else {
        (
            "SELECT c.id, a.slug, w.id, w.path, w.display_name, c.external_id, c.title, c.source_path,
                    c.started_at, c.ended_at, c.approx_tokens, c.metadata_json, c.source_id, c.origin_host, c.metadata_bin
             FROM conversations c
             JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             WHERE c.source_path = ?1
             ORDER BY c.started_at DESC LIMIT 1",
            frankensqlite::params![source_path],
        )
    };
    let rows = storage.raw().query_map_collect(
        sql,
        params,
        |row: &Row| {
            let convo_id: i64 = row.get_typed(0)?;
            let workspace_path = row
                .get_typed::<Option<String>>(3)?
                .map(std::path::PathBuf::from);
            let metadata_json = row
                .get_typed::<Option<String>>(11)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .or_else(|| {
                    row.get_typed::<Option<Vec<u8>>>(14)
                        .ok()
                        .flatten()
                        .and_then(|b| rmp_serde::from_slice(&b).ok())
                })
                .unwrap_or_default();
            let convo = Conversation {
                id: Some(convo_id),
                agent_slug: row.get_typed(1)?,
                workspace: workspace_path.clone(),
                external_id: row.get_typed(5)?,
                title: row.get_typed(6)?,
                source_path: std::path::PathBuf::from(row.get_typed::<String>(7)?),
                started_at: row.get_typed(8)?,
                ended_at: row.get_typed(9)?,
                approx_tokens: row.get_typed(10)?,
                metadata_json,
                messages: Vec::new(),
                source_id: row
                    .get_typed::<Option<String>>(12)?
                    .unwrap_or_else(|| "local".to_string()),
                origin_host: row.get_typed(13)?,
            };
            let workspace = row.get_typed::<Option<i64>>(2)?.map(|id| Workspace {
                id: Some(id),
                path: workspace_path.unwrap_or_default(),
                display_name: row.get_typed(4).ok().flatten(),
            });
            Ok((convo_id, convo, workspace))
        },
    )?;
    if let Some((convo_id, convo, workspace)) = rows.into_iter().next() {
        let messages = storage.fetch_messages(convo_id)?;
        return Ok(Some(ConversationView {
            convo,
            messages,
            workspace,
        }));
    }
    Ok(None)
}

/// Load a conversation with LRU caching.
///
/// This is the primary function for loading conversations in the TUI.
/// It uses a sharded LRU cache to avoid repeated database queries and
/// JSON parsing for the same conversation.
///
/// Cache behavior:
/// - Hit: Returns cached Arc<ConversationView> (fast path)
/// - Miss: Queries database, parses JSON, caches result
///
/// The cache is keyed by source identity and has a configurable capacity
/// via the CASS_CONV_CACHE_SIZE environment variable (default: 256 per shard,
/// 4096 total entries across 16 shards).
pub fn load_conversation(
    storage: &FrankenStorage,
    source_path: &str,
) -> Result<Option<ConversationView>> {
    // Fast path: check cache first
    if let Some(cached) = CONVERSATION_CACHE.get(None, source_path) {
        // Clone out of Arc for API compatibility
        return Ok(Some((*cached).clone()));
    }

    // Cache miss: load from database
    let view = load_conversation_uncached(storage, None, source_path)?;

    // Cache the result if found
    if let Some(v) = view {
        CONVERSATION_CACHE.insert(None, source_path, v.clone());
        return Ok(Some(v));
    }

    Ok(None)
}

/// Load a conversation for a specific source with caching.
pub fn load_conversation_for_source(
    storage: &FrankenStorage,
    source_id: &str,
    source_path: &str,
) -> Result<Option<ConversationView>> {
    if let Some(cached) = CONVERSATION_CACHE.get(Some(source_id), source_path) {
        return Ok(Some((*cached).clone()));
    }

    let view = load_conversation_uncached(storage, Some(source_id), source_path)?;

    if let Some(v) = view {
        CONVERSATION_CACHE.insert(Some(source_id), source_path, v.clone());
        return Ok(Some(v));
    }

    Ok(None)
}

/// Load a conversation with caching, returning Arc for efficiency.
///
/// Use this variant when you need to hold the conversation view for
/// an extended period without cloning.
pub fn load_conversation_arc(
    storage: &FrankenStorage,
    source_path: &str,
) -> Result<Option<Arc<ConversationView>>> {
    // Fast path: check cache first
    if let Some(cached) = CONVERSATION_CACHE.get(None, source_path) {
        return Ok(Some(cached));
    }

    // Cache miss: load from database
    let view = load_conversation_uncached(storage, None, source_path)?;

    // Cache and return the Arc
    if let Some(v) = view {
        let arc = CONVERSATION_CACHE.insert(None, source_path, v);
        return Ok(Some(arc));
    }

    Ok(None)
}

/// Log conversation cache statistics.
///
/// Outputs cache stats at debug level via tracing.
pub fn log_conversation_cache_stats() {
    let (hits, misses, evictions) = CONVERSATION_CACHE.stats().get();
    let hit_rate = CONVERSATION_CACHE.stats().hit_rate();
    let count = CONVERSATION_CACHE.len();

    tracing::debug!(
        target: "cass::perf::conversation_cache",
        hits = hits,
        misses = misses,
        evictions = evictions,
        hit_rate = format!("{:.1}%", hit_rate * 100.0),
        cached_count = count,
        "Conversation cache statistics"
    );
}

pub fn role_style(role: &MessageRole, palette: ThemePalette) -> ftui::Style {
    match role {
        MessageRole::User => ftui::Style::new().fg(palette.user),
        MessageRole::Agent => ftui::Style::new().fg(palette.agent),
        MessageRole::Tool => ftui::Style::new().fg(palette.tool),
        MessageRole::System => ftui::Style::new().fg(palette.system),
        MessageRole::Other(_) => ftui::Style::new().fg(palette.hint),
    }
}

// -------------------------------------------------------------------------
// Shared TUI types (moved from tui.rs to remove ratatui dependency)
// -------------------------------------------------------------------------

/// How search results are ranked and ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RankingMode {
    RecentHeavy,
    Balanced,
    RelevanceHeavy,
    MatchQualityHeavy,
    DateNewest,
    DateOldest,
}

/// Format a timestamp as a short human-readable date for filter chips.
/// Shows "Nov 25" for same year, "Nov 25, 2023" for other years.
pub fn format_time_short(ms: i64) -> String {
    use chrono::{DateTime, Datelike, Utc};
    let now = Utc::now();
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| {
            if dt.year() == now.year() {
                dt.format("%b %d").to_string()
            } else {
                dt.format("%b %d, %Y").to_string()
            }
        })
        .unwrap_or_else(|| "?".to_string())
}

// =========================================================================
// Explainability Cockpit — Information Architecture (1mfw3.3.1)
// =========================================================================
//
// The cockpit is an inspector-mode overlay that surfaces causal explanations
// for adaptive runtime decisions: diff strategy, resize coalescing, frame
// budget/degradation, and a correlating timeline of decision events.
//
// Panel taxonomy:
//   1. DiffStrategy   — Why the last frame used full vs partial redraw.
//   2. ResizeRegime   — BOCPD regime classification and coalescer decisions.
//   3. BudgetHealth   — Frame budget vs actual, degradation level, PID state.
//   4. Timeline       — Chronological feed of major decision events.
//
// Each panel has a data contract struct defining required fields, source of
// truth, and empty/error-state policies.

/// Panel taxonomy for the explainability cockpit.
///
/// Each variant represents one cockpit surface. Panels are rendered as
/// stacked sections inside the inspector overlay when the cockpit mode is
/// active. The inspector can be in either classic (Timing/Layout/HitRegions)
/// or cockpit mode, toggled independently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CockpitPanel {
    #[default]
    /// Frame diff strategy decisions: full vs partial redraw, dirty-row counts.
    DiffStrategy,
    /// Resize coalescer regime: Steady vs Burst, BOCPD probability, recent history.
    ResizeRegime,
    /// Frame budget health: target vs actual, degradation level, PID controller state.
    BudgetHealth,
    /// Chronological timeline of major decision events across all subsystems.
    Timeline,
}

impl CockpitPanel {
    pub fn label(self) -> &'static str {
        match self {
            Self::DiffStrategy => "Diff",
            Self::ResizeRegime => "Resize",
            Self::BudgetHealth => "Budget",
            Self::Timeline => "Timeline",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::DiffStrategy => Self::ResizeRegime,
            Self::ResizeRegime => Self::BudgetHealth,
            Self::BudgetHealth => Self::Timeline,
            Self::Timeline => Self::DiffStrategy,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::DiffStrategy => Self::Timeline,
            Self::ResizeRegime => Self::DiffStrategy,
            Self::BudgetHealth => Self::ResizeRegime,
            Self::Timeline => Self::BudgetHealth,
        }
    }

    /// All panels in display order.
    pub const ALL: [CockpitPanel; 4] = [
        Self::DiffStrategy,
        Self::ResizeRegime,
        Self::BudgetHealth,
        Self::Timeline,
    ];
}

/// Empty/error-state display policy for cockpit panels.
///
/// When telemetry data is missing (cold start, no resize events, etc.),
/// panels should never crash or show garbled output. Each field specifies
/// the placeholder text to render when the corresponding data is absent.
#[derive(Clone, Debug)]
pub struct CockpitEmptyPolicy {
    /// Placeholder when no evidence is available at all.
    pub no_data: &'static str,
    /// Placeholder when the subsystem hasn't fired yet (e.g., no resize events).
    pub awaiting: &'static str,
    /// Placeholder when the feature is disabled in config.
    pub disabled: &'static str,
}

impl Default for CockpitEmptyPolicy {
    fn default() -> Self {
        Self {
            no_data: "\u{2014}", // em dash
            awaiting: "awaiting first event\u{2026}",
            disabled: "(disabled)",
        }
    }
}

/// Data contract for the Diff Strategy cockpit panel.
///
/// Source of truth: `ftui::runtime::evidence_telemetry::diff_snapshot()`
///
/// Answers: "Why did the last frame use full vs partial redraw?"
#[derive(Clone, Debug, Default)]
pub struct DiffStrategyContract {
    /// Whether the last frame was a full redraw.
    pub last_was_full_redraw: bool,
    /// Number of dirty rows detected in the last partial redraw.
    pub dirty_row_count: u32,
    /// Total row count for the frame (dirty_row_count / total = dirty ratio).
    pub total_row_count: u32,
    /// Reason for the diff decision (human-readable).
    pub reason: &'static str,
    /// Number of consecutive full redraws.
    pub consecutive_full_redraws: u32,
    /// Cumulative full-redraw ratio (full / total frames observed).
    pub full_redraw_ratio: f64,
}

impl DiffStrategyContract {
    /// Dirty row ratio (0.0..1.0). Returns 0.0 if total_row_count is zero.
    pub fn dirty_ratio(&self) -> f64 {
        if self.total_row_count == 0 {
            0.0
        } else {
            self.dirty_row_count as f64 / self.total_row_count as f64
        }
    }

    /// Whether meaningful data has been captured.
    pub fn has_data(&self) -> bool {
        self.total_row_count > 0
    }
}

/// Data contract for the Resize Regime cockpit panel.
///
/// Source of truth: `ftui::runtime::evidence_telemetry::resize_snapshot()`
/// and `ResizeEvidenceSummary::recent_resizes` ring buffer.
///
/// Answers: "What resize regime are we in and why?"
#[derive(Clone, Debug)]
pub struct ResizeRegimeContract {
    /// Current regime label ("Steady", "Burst", or em-dash).
    pub regime: &'static str,
    /// Current terminal size (cols, rows).
    pub terminal_size: Option<(u16, u16)>,
    /// BOCPD burst probability (0.0..1.0), None if BOCPD disabled.
    pub bocpd_p_burst: Option<f64>,
    /// BOCPD recommended coalescer delay (ms), None if not applicable.
    pub bocpd_delay_ms: Option<u32>,
    /// Number of resize events in history buffer.
    pub history_len: usize,
    /// Most recent resize action ("apply", "defer", "coalesce").
    pub last_action: &'static str,
    /// Inter-arrival time of the most recent resize event (ms).
    pub last_dt_ms: f64,
    /// Events per second at the last decision.
    pub last_event_rate: f64,
}

impl Default for ResizeRegimeContract {
    fn default() -> Self {
        Self {
            regime: "\u{2014}",
            terminal_size: None,
            bocpd_p_burst: None,
            bocpd_delay_ms: None,
            history_len: 0,
            last_action: "\u{2014}",
            last_dt_ms: 0.0,
            last_event_rate: 0.0,
        }
    }
}

impl ResizeRegimeContract {
    /// Whether meaningful data has been captured.
    pub fn has_data(&self) -> bool {
        self.regime != "\u{2014}"
    }
}

/// Data contract for the Budget Health cockpit panel.
///
/// Source of truth: `ftui::runtime::evidence_telemetry::budget_snapshot()`
/// and `ResizeEvidenceSummary` budget-related fields.
///
/// Answers: "Is the frame budget healthy? What degradation is active?"
#[derive(Clone, Debug)]
pub struct BudgetHealthContract {
    /// Current degradation level label.
    pub degradation: &'static str,
    /// Target frame budget (microseconds).
    pub budget_us: f64,
    /// Actual frame time (microseconds).
    pub frame_time_us: f64,
    /// PID controller output (positive = headroom, negative = over-budget).
    pub pid_output: f64,
    /// Whether the budget controller is still in warmup.
    pub in_warmup: bool,
    /// Total frames observed by the budget controller.
    pub frames_observed: u32,
    /// Budget pressure ratio: frame_time / budget (>1.0 means over-budget).
    pub pressure: f64,
}

impl Default for BudgetHealthContract {
    fn default() -> Self {
        Self {
            degradation: "\u{2014}",
            budget_us: 0.0,
            frame_time_us: 0.0,
            pid_output: 0.0,
            in_warmup: true,
            frames_observed: 0,
            pressure: 0.0,
        }
    }
}

impl BudgetHealthContract {
    /// Whether meaningful data has been captured.
    pub fn has_data(&self) -> bool {
        self.frames_observed > 0
    }

    /// Whether the frame budget is currently exceeded.
    pub fn is_over_budget(&self) -> bool {
        self.pressure > 1.0
    }
}

/// A single event in the cockpit timeline feed.
///
/// Timeline events correlate decision points across subsystems,
/// enabling causal diagnosis ("the resize burst caused degradation
/// to drop to SimpleBorders").
#[derive(Clone, Debug)]
pub struct CockpitTimelineEvent {
    /// Subsystem that generated the event.
    pub source: CockpitPanel,
    /// Human-readable one-line summary of what happened.
    pub summary: String,
    /// Monotonic event index for ordering.
    pub event_idx: u64,
    /// Elapsed time since app start (seconds).
    pub elapsed_secs: f64,
    /// Severity/importance of the event.
    pub severity: TimelineEventSeverity,
}

/// Severity levels for cockpit timeline events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimelineEventSeverity {
    /// Routine decision (e.g., normal resize apply).
    #[default]
    Info,
    /// Notable state change (e.g., regime transition Steady -> Burst).
    StateChange,
    /// Degradation or pressure event (e.g., over-budget, degradation level change).
    Warning,
}

/// Data contract for the Timeline cockpit panel.
///
/// Source of truth: aggregated from all other cockpit contracts.
/// Events are pushed by `EvidenceSnapshots::refresh()` when it detects
/// state transitions.
///
/// Answers: "What changed, when, and across which subsystem?"
#[derive(Clone, Debug)]
pub struct TimelineContract {
    /// Ring buffer of recent timeline events (newest last).
    pub events: std::collections::VecDeque<CockpitTimelineEvent>,
    /// Maximum events to retain.
    pub capacity: usize,
}

/// Default timeline capacity.
const TIMELINE_CAPACITY: usize = 64;

impl Default for TimelineContract {
    fn default() -> Self {
        Self::new()
    }
}

impl TimelineContract {
    pub fn new() -> Self {
        Self {
            events: std::collections::VecDeque::with_capacity(TIMELINE_CAPACITY),
            capacity: TIMELINE_CAPACITY,
        }
    }

    /// Push a new event, evicting the oldest if at capacity.
    pub fn push(&mut self, event: CockpitTimelineEvent) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Whether any events have been recorded.
    pub fn has_data(&self) -> bool {
        !self.events.is_empty()
    }

    /// Number of events in the buffer.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Cockpit display mode controlling overlay sizing behaviour.
///
/// **Overlay** is a compact bottom-right panel (default).
/// **Expanded** is a taller panel that occupies more vertical space,
/// allowing multi-panel stacking and more timeline events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CockpitMode {
    /// Compact overlay anchored to bottom-right corner.
    #[default]
    Overlay,
    /// Expanded surface that takes more vertical space for deeper inspection.
    Expanded,
}

impl CockpitMode {
    /// Cycle to the next mode.
    pub fn cycle(self) -> Self {
        match self {
            Self::Overlay => Self::Expanded,
            Self::Expanded => Self::Overlay,
        }
    }

    /// Short label for status display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Overlay => "overlay",
            Self::Expanded => "expanded",
        }
    }
}

/// Aggregated cockpit state holding all panel contracts.
///
/// This struct is the single rendering-ready data source for the
/// cockpit overlay. It is updated each tick by polling evidence
/// telemetry and detecting state transitions for timeline events.
#[derive(Clone, Debug, Default)]
pub struct CockpitState {
    /// Active cockpit panel (for single-panel focus mode).
    pub active_panel: CockpitPanel,
    /// Whether cockpit mode is active (vs classic inspector tabs).
    pub enabled: bool,
    /// Display mode (overlay vs expanded).
    pub mode: CockpitMode,
    /// Diff strategy contract.
    pub diff: DiffStrategyContract,
    /// Resize regime contract.
    pub resize: ResizeRegimeContract,
    /// Budget health contract.
    pub budget: BudgetHealthContract,
    /// Timeline event feed.
    pub timeline: TimelineContract,
    /// Empty-state display policy.
    pub empty_policy: CockpitEmptyPolicy,
}

impl CockpitState {
    pub fn new() -> Self {
        Self {
            timeline: TimelineContract::new(),
            ..Default::default()
        }
    }

    /// Whether any panel has meaningful data to display.
    pub fn has_any_data(&self) -> bool {
        self.diff.has_data()
            || self.resize.has_data()
            || self.budget.has_data()
            || self.timeline.has_data()
    }

    /// Get the empty-state message for a panel.
    pub fn empty_message(&self, panel: CockpitPanel) -> &'static str {
        match panel {
            CockpitPanel::DiffStrategy => {
                if self.diff.has_data() {
                    ""
                } else {
                    self.empty_policy.awaiting
                }
            }
            CockpitPanel::ResizeRegime => {
                if self.resize.has_data() {
                    ""
                } else {
                    self.empty_policy.awaiting
                }
            }
            CockpitPanel::BudgetHealth => {
                if self.budget.has_data() {
                    ""
                } else {
                    self.empty_policy.awaiting
                }
            }
            CockpitPanel::Timeline => {
                if self.timeline.has_data() {
                    ""
                } else {
                    self.empty_policy.no_data
                }
            }
        }
    }
}

// -------------------------------------------------------------------------
// Unit Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_test_view(id: i64) -> ConversationView {
        ConversationView {
            convo: Conversation {
                id: Some(id),
                agent_slug: "claude".to_string(),
                workspace: Some(PathBuf::from("/test/workspace")),
                external_id: Some(format!("ext-{}", id)),
                title: Some(format!("Test Conversation {}", id)),
                source_path: PathBuf::from(format!("/test/path/{}.jsonl", id)),
                started_at: Some(1704067200 + id),
                ended_at: None,
                approx_tokens: Some(1000),
                metadata_json: serde_json::json!({"test": true}),
                messages: Vec::new(),
                source_id: "local".to_string(),
                origin_host: None,
            },
            messages: vec![Message {
                id: Some(1),
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1704067200),
                content: "Test message".to_string(),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            }],
            workspace: Some(Workspace {
                id: Some(1),
                path: PathBuf::from("/test/workspace"),
                display_name: None,
            }),
        }
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = ConversationCache::new(10);
        let view = make_test_view(1);
        let source_path = "/test/path/1.jsonl";

        // Insert into cache
        let arc = cache.insert(None, source_path, view.clone());
        assert_eq!(arc.convo.id, Some(1));

        // Get from cache
        let cached = cache.get(None, source_path);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().convo.id, Some(1));

        // Check stats
        let (hits, misses, _) = cache.stats().get();
        assert_eq!(hits, 1);
        assert_eq!(misses, 0);
    }

    #[test]
    fn test_cache_distinguishes_same_path_across_sources() {
        let cache = ConversationCache::new(10);
        let source_path = "/test/shared/session.jsonl";

        let mut local = make_test_view(1);
        local.convo.source_path = PathBuf::from(source_path);
        local.convo.source_id = "local".to_string();
        let mut remote = make_test_view(2);
        remote.convo.source_path = PathBuf::from(source_path);
        remote.convo.source_id = "work-laptop".to_string();

        cache.insert(Some("local"), source_path, local);
        cache.insert(Some("work-laptop"), source_path, remote);

        let local_cached = cache.get(Some("local"), source_path).expect("local cached");
        let remote_cached = cache
            .get(Some("work-laptop"), source_path)
            .expect("remote cached");

        assert_eq!(local_cached.convo.source_id, "local");
        assert_eq!(remote_cached.convo.source_id, "work-laptop");
        assert_ne!(local_cached.convo.id, remote_cached.convo.id);
    }

    #[test]
    fn load_conversation_for_source_selects_exact_source_id() {
        use crate::storage::sqlite::FrankenStorage;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("open storage");
        let conn = storage.raw();
        let shared_path = "/shared/session.jsonl";

        conn.execute("INSERT INTO agents (id, slug, name, kind, created_at, updated_at) VALUES (1, 'claude_code', 'Claude Code', 'local', 0, 0)")
            .expect("insert agent");
        {
            use frankensqlite::compat::{ParamValue, param_slice_to_values};
            let p = [ParamValue::from(shared_path.to_string())];
            conn.execute_with_params(
                "INSERT INTO conversations (id, agent_id, external_id, title, source_path, source_id, started_at) VALUES (1, 1, 'local-ext', 'Local Session', ?1, 'local', 200)",
                &param_slice_to_values(&p),
            )
            .expect("insert local conversation");
            conn.execute_with_params(
                "INSERT INTO conversations (id, agent_id, external_id, title, source_path, source_id, started_at) VALUES (2, 1, 'remote-ext', 'Remote Session', ?1, 'work-laptop', 100)",
                &param_slice_to_values(&p),
            )
            .expect("insert remote conversation");
        }
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content) VALUES (1, 1, 0, 'user', 'local body')",
        )
        .expect("insert local message");
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, content) VALUES (2, 2, 0, 'user', 'remote body')",
        )
        .expect("insert remote message");

        let local = load_conversation_for_source(&storage, "local", shared_path)
            .expect("load local")
            .expect("local conversation");
        let remote = load_conversation_for_source(&storage, "work-laptop", shared_path)
            .expect("load remote")
            .expect("remote conversation");

        assert_eq!(local.convo.source_id, "local");
        assert_eq!(local.convo.title.as_deref(), Some("Local Session"));
        assert_eq!(local.messages[0].content, "local body");

        assert_eq!(remote.convo.source_id, "work-laptop");
        assert_eq!(remote.convo.title.as_deref(), Some("Remote Session"));
        assert_eq!(remote.messages[0].content, "remote body");
    }

    #[test]
    fn test_cache_miss() {
        let cache = ConversationCache::new(10);

        // Get from empty cache
        let cached = cache.get(None, "/nonexistent/path.jsonl");
        assert!(cached.is_none());

        // Check stats
        let (hits, misses, _) = cache.stats().get();
        assert_eq!(hits, 0);
        assert_eq!(misses, 1);
    }

    #[test]
    fn test_cache_invalidation() {
        let cache = ConversationCache::new(10);
        let view = make_test_view(1);
        let source_path = "/test/path/1.jsonl";

        // Insert and verify
        cache.insert(None, source_path, view);
        assert!(cache.get(None, source_path).is_some());

        // Invalidate
        cache.invalidate(None, source_path);
        assert!(cache.get(None, source_path).is_none());
    }

    #[test]
    fn test_cache_invalidate_all() {
        let cache = ConversationCache::new(10);

        // Insert multiple entries
        for i in 0..5 {
            let view = make_test_view(i);
            let source_path = format!("/test/path/{}.jsonl", i);
            cache.insert(None, &source_path, view);
        }

        assert_eq!(cache.len(), 5);

        // Invalidate all
        cache.invalidate_all();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_cache_lru_eviction() {
        let cache = ConversationCache::new(2); // 2 per shard, 32 total

        // Insert more entries than a single shard can hold
        // All entries go to same shard by using paths that hash to same shard
        // (in practice, FxHasher distributes well, so we insert many entries)
        for i in 0..100 {
            let view = make_test_view(i);
            let source_path = format!("/test/path/{}.jsonl", i);
            cache.insert(None, &source_path, view);
        }

        // Some early entries should have been evicted
        let (_, _, evictions) = cache.stats().get();
        assert!(evictions > 0, "Expected some evictions with small capacity");
    }

    #[test]
    fn test_cache_hit_rate() {
        let cache = ConversationCache::new(10);
        let view = make_test_view(1);
        let source_path = "/test/path/1.jsonl";

        // Initial hit rate is 0
        assert_eq!(cache.stats().hit_rate(), 0.0);

        // Insert and access twice (1 miss on insert lookup, then 2 hits)
        cache.insert(None, source_path, view);
        let _ = cache.get(None, source_path);
        let _ = cache.get(None, source_path);

        // Hit rate should be positive (2 hits / 2 total)
        let hit_rate = cache.stats().hit_rate();
        assert!(
            hit_rate > 0.5,
            "Expected >50% hit rate, got {:.1}%",
            hit_rate * 100.0
        );
    }

    #[test]
    fn test_cache_shard_distribution() {
        let cache = ConversationCache::new(100);

        // Insert 1000 entries
        for i in 0..1000 {
            let view = make_test_view(i);
            let source_path = format!("/various/paths/{}/session.jsonl", i);
            cache.insert(None, &source_path, view);
        }

        // All entries should be cached
        assert_eq!(cache.len(), 1000);
    }

    #[test]
    fn test_cache_concurrent_access() {
        use std::thread;

        let cache = Arc::new(ConversationCache::new(100));
        let mut handles = vec![];

        // Spawn writers
        for t in 0..4 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..250 {
                    let id = t * 250 + i;
                    let view = make_test_view(id);
                    let source_path = format!("/test/path/{}.jsonl", id);
                    cache.insert(None, &source_path, view);
                }
            }));
        }

        // Spawn readers
        for _ in 0..4 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..1000 {
                    let source_path = format!("/test/path/{}.jsonl", i);
                    let _ = cache.get(None, &source_path);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify cache is consistent
        let (hits, misses, _) = cache.stats().get();
        assert!(hits + misses > 0, "Expected some cache operations");
    }

    // =====================================================================
    // Cockpit IA contract tests (1mfw3.3.1)
    // =====================================================================

    #[test]
    fn cockpit_panel_label_and_navigation() {
        assert_eq!(CockpitPanel::DiffStrategy.label(), "Diff");
        assert_eq!(CockpitPanel::ResizeRegime.label(), "Resize");
        assert_eq!(CockpitPanel::BudgetHealth.label(), "Budget");
        assert_eq!(CockpitPanel::Timeline.label(), "Timeline");

        // Full forward cycle
        let mut p = CockpitPanel::DiffStrategy;
        p = p.next();
        assert_eq!(p, CockpitPanel::ResizeRegime);
        p = p.next();
        assert_eq!(p, CockpitPanel::BudgetHealth);
        p = p.next();
        assert_eq!(p, CockpitPanel::Timeline);
        p = p.next();
        assert_eq!(p, CockpitPanel::DiffStrategy);

        // Full backward cycle
        p = CockpitPanel::DiffStrategy;
        p = p.prev();
        assert_eq!(p, CockpitPanel::Timeline);
        p = p.prev();
        assert_eq!(p, CockpitPanel::BudgetHealth);
        p = p.prev();
        assert_eq!(p, CockpitPanel::ResizeRegime);
        p = p.prev();
        assert_eq!(p, CockpitPanel::DiffStrategy);
    }

    #[test]
    fn cockpit_panel_all_constant() {
        assert_eq!(CockpitPanel::ALL.len(), 4);
        assert_eq!(CockpitPanel::ALL[0], CockpitPanel::DiffStrategy);
        assert_eq!(CockpitPanel::ALL[3], CockpitPanel::Timeline);
    }

    #[test]
    fn diff_strategy_contract_defaults_no_data() {
        let diff = DiffStrategyContract::default();
        assert!(!diff.has_data());
        assert_eq!(diff.dirty_ratio(), 0.0);
        assert!(!diff.last_was_full_redraw);
    }

    #[test]
    fn diff_strategy_contract_dirty_ratio() {
        let diff = DiffStrategyContract {
            dirty_row_count: 10,
            total_row_count: 40,
            ..Default::default()
        };
        assert!(diff.has_data());
        assert!((diff.dirty_ratio() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn resize_regime_contract_defaults_no_data() {
        let resize = ResizeRegimeContract::default();
        assert!(!resize.has_data());
        assert_eq!(resize.regime, "\u{2014}");
    }

    #[test]
    fn resize_regime_contract_with_data() {
        let resize = ResizeRegimeContract {
            regime: "Burst",
            terminal_size: Some((120, 40)),
            bocpd_p_burst: Some(0.87),
            history_len: 5,
            last_action: "defer",
            ..Default::default()
        };
        assert!(resize.has_data());
        assert_eq!(resize.terminal_size, Some((120, 40)));
    }

    #[test]
    fn budget_health_contract_defaults_no_data() {
        let budget = BudgetHealthContract::default();
        assert!(!budget.has_data());
        assert!(!budget.is_over_budget());
    }

    #[test]
    fn budget_health_contract_over_budget() {
        let budget = BudgetHealthContract {
            budget_us: 16_666.0,
            frame_time_us: 25_000.0,
            pressure: 1.5,
            frames_observed: 100,
            ..Default::default()
        };
        assert!(budget.has_data());
        assert!(budget.is_over_budget());
    }

    #[test]
    fn timeline_contract_push_and_eviction() {
        let mut timeline = TimelineContract {
            events: std::collections::VecDeque::new(),
            capacity: 3,
        };
        assert!(timeline.is_empty());
        assert!(!timeline.has_data());

        for i in 0..5 {
            timeline.push(CockpitTimelineEvent {
                source: CockpitPanel::BudgetHealth,
                summary: format!("event {i}"),
                event_idx: i,
                elapsed_secs: i as f64,
                severity: TimelineEventSeverity::Info,
            });
        }

        assert_eq!(timeline.len(), 3);
        assert!(timeline.has_data());
        // Oldest events should be evicted
        assert_eq!(timeline.events[0].event_idx, 2);
        assert_eq!(timeline.events[2].event_idx, 4);
    }

    #[test]
    fn cockpit_state_empty_messages() {
        let state = CockpitState::new();
        assert!(!state.has_any_data());

        // All panels should return awaiting/no_data messages
        assert!(!state.empty_message(CockpitPanel::DiffStrategy).is_empty());
        assert!(!state.empty_message(CockpitPanel::ResizeRegime).is_empty());
        assert!(!state.empty_message(CockpitPanel::BudgetHealth).is_empty());
        assert!(!state.empty_message(CockpitPanel::Timeline).is_empty());
    }

    #[test]
    fn cockpit_state_partial_data() {
        let mut state = CockpitState::new();
        state.resize = ResizeRegimeContract {
            regime: "Steady",
            ..Default::default()
        };
        assert!(state.has_any_data());
        // Resize has data, so empty_message returns ""
        assert_eq!(state.empty_message(CockpitPanel::ResizeRegime), "");
        // Others still show placeholder
        assert!(!state.empty_message(CockpitPanel::DiffStrategy).is_empty());
    }

    #[test]
    fn timeline_event_severity_default_is_info() {
        assert_eq!(
            TimelineEventSeverity::default(),
            TimelineEventSeverity::Info
        );
    }

    #[test]
    fn cockpit_empty_policy_defaults() {
        let policy = CockpitEmptyPolicy::default();
        assert_eq!(policy.no_data, "\u{2014}");
        assert!(policy.awaiting.contains("awaiting"));
        assert!(policy.disabled.contains("disabled"));
    }

    // -- CockpitMode tests (1mfw3.3.3) ------------------------------------

    #[test]
    fn cockpit_mode_default_is_overlay() {
        assert_eq!(CockpitMode::default(), CockpitMode::Overlay);
    }

    #[test]
    fn cockpit_mode_cycle() {
        assert_eq!(CockpitMode::Overlay.cycle(), CockpitMode::Expanded);
        assert_eq!(CockpitMode::Expanded.cycle(), CockpitMode::Overlay);
    }

    #[test]
    fn cockpit_mode_labels() {
        assert_eq!(CockpitMode::Overlay.label(), "overlay");
        assert_eq!(CockpitMode::Expanded.label(), "expanded");
    }

    #[test]
    fn cockpit_state_includes_mode() {
        let state = CockpitState::new();
        assert_eq!(state.mode, CockpitMode::Overlay);
        assert!(!state.enabled);
    }
}
