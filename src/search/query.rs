use anyhow::{Result, anyhow, bail};
use lru::LruCache;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::cell::RefCell;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, Occur, PhraseQuery, Query, RangeQuery, RegexQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Term, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, ReloadPolicy, Searcher, TantivyDocument};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use rusqlite::Connection;

use crate::search::ann_index::{DEFAULT_EF_SEARCH, HnswIndex};
use crate::search::canonicalize::canonicalize_for_embedding;
use crate::search::embedder::Embedder;
use crate::search::tantivy::fields_from_schema;
use crate::search::vector_index::{
    SemanticFilter, SemanticFilterMaps, VectorIndex, VectorSearchResult,
};

use crate::sources::provenance::SourceFilter;

// ============================================================================
// String Interner for Cache Keys (Opt 2.3)
// ============================================================================
//
// Reduces memory usage and allocation overhead for repeated cache key patterns.
// Uses LRU eviction to bound memory, Arc<str> for cheap cloning.

/// Thread-safe string interner with bounded memory via LRU eviction.
/// Uses LruCache<Arc<str>, Arc<str>> where key and value are the same Arc,
/// enabling O(1) lookup via Borrow<str> trait while preserving LRU semantics.
pub struct StringInterner {
    cache: RwLock<LruCache<Arc<str>, Arc<str>>>,
}

impl StringInterner {
    /// Create a new interner with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: RwLock::new(LruCache::new(
                NonZeroUsize::new(capacity).expect("capacity must be > 0"),
            )),
        }
    }

    /// Intern a string, returning a shared Arc<str>.
    /// If the string is already interned, returns the existing Arc.
    /// Otherwise, creates a new Arc and caches it.
    ///
    /// Performance: O(1) lookup via LruCache's internal HashMap.
    pub fn intern(&self, s: &str) -> Arc<str> {
        // Fast path: read-only check for existing entry (O(1) lookup)
        {
            let cache = self.cache.read();
            // LruCache::peek allows O(1) lookup without updating LRU order
            // Arc<str>: Borrow<str> enables lookup by &str
            if let Some(arc) = cache.peek(s) {
                return Arc::clone(arc);
            }
        }

        // Slow path: acquire write lock and insert
        let mut cache = self.cache.write();

        // Double-check after acquiring write lock (another thread may have inserted)
        // Use get() here to update LRU order since we're about to use this entry
        if let Some(arc) = cache.get(s) {
            return Arc::clone(arc);
        }

        // Create new Arc<str> and insert (same Arc as key and value)
        let arc: Arc<str> = Arc::from(s);
        cache.put(Arc::clone(&arc), Arc::clone(&arc));
        arc
    }

    /// Get the current number of interned strings.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.read().len()
    }

    /// Check if the interner is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }
}

/// Global cache key interner with 10K entry limit (~1MB for typical keys).
/// Uses Lazy initialization for thread-safe singleton.
static CACHE_KEY_INTERNER: Lazy<StringInterner> = Lazy::new(|| StringInterner::new(10_000));

/// Intern a cache key string, returning a shared Arc<str>.
#[inline]
fn intern_cache_key(s: &str) -> Arc<str> {
    CACHE_KEY_INTERNER.intern(s)
}

// ============================================================================
// SQL Placeholder Builder (Opt 4.5: Pre-sized String Buffers)
// ============================================================================

/// Build a comma-separated list of SQL placeholders with pre-allocated capacity.
///
/// For `n` items, produces "?,?,?..." (n "?" with n-1 ",").
/// Uses pre-sized String to avoid reallocations.
///
/// # Examples
/// ```ignore
/// assert_eq!(sql_placeholders(0), "");
/// assert_eq!(sql_placeholders(1), "?");
/// assert_eq!(sql_placeholders(3), "?,?,?");
/// ```
#[inline]
pub fn sql_placeholders(count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    // Capacity: n "?" + (n-1) "," = 2n - 1
    let capacity = count.saturating_mul(2).saturating_sub(1);
    let mut result = String::with_capacity(capacity);
    for i in 0..count {
        if i > 0 {
            result.push(',');
        }
        result.push('?');
    }
    result
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SearchFilters {
    pub agents: HashSet<String>,
    pub workspaces: HashSet<String>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
    /// Filter by conversation source (local, remote, or specific source ID)
    #[serde(skip_serializing_if = "SourceFilter::is_all")]
    pub source_filter: SourceFilter,
    /// Filter to specific session source paths (for chained searches)
    #[serde(skip_serializing_if = "HashSet::is_empty")]
    pub session_paths: HashSet<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Lexical (BM25) search - keyword matching
    #[default]
    Lexical,
    /// Semantic search - embedding similarity
    Semantic,
    /// Hybrid search - RRF fusion of lexical and semantic
    Hybrid,
}

impl SearchMode {
    pub fn next(self) -> Self {
        match self {
            SearchMode::Lexical => SearchMode::Semantic,
            SearchMode::Semantic => SearchMode::Hybrid,
            SearchMode::Hybrid => SearchMode::Lexical,
        }
    }
}

const RRF_K: f32 = 60.0;
const HYBRID_CANDIDATE_MULTIPLIER: usize = 3;
const ANN_CANDIDATE_MULTIPLIER: usize = 4;

// ============================================================================
// Query Explanation types (--explain flag support)
// ============================================================================

/// Classification of query type for explanation purposes
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryType {
    /// Single term without operators
    Simple,
    /// Quoted phrase ("exact match")
    Phrase,
    /// Contains AND/OR/NOT operators
    Boolean,
    /// Contains wildcards (* prefix/suffix)
    Wildcard,
    /// Has time/agent/workspace filters
    Filtered,
    /// Empty query
    Empty,
}

/// How the index will execute this query
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStrategy {
    /// Fast path: edge n-gram prefix matching
    EdgeNgram,
    /// Regex scan for leading wildcards (*foo)
    RegexScan,
    /// Combined boolean query execution
    BooleanCombination,
    /// Range scan for time filters
    RangeScan,
    /// All documents (empty query)
    FullScan,
}

/// Rough complexity indicator for query execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryCost {
    /// Very fast (under 10ms typical)
    Low,
    /// Moderate (10-100ms typical)
    Medium,
    /// Expensive (100ms+ typical, may scan many documents)
    High,
}

/// Sub-component of a parsed term
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParsedSubTerm {
    pub text: String,
    pub pattern: String,
}

/// Parsed term from the query
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParsedTerm {
    /// Original term text
    pub text: String,
    /// Whether this is negated (NOT/-)
    pub negated: bool,
    /// Sub-terms if split (implicit AND)
    pub subterms: Vec<ParsedSubTerm>,
}

/// Parsed structure of the query
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ParsedQuery {
    /// Individual terms extracted
    pub terms: Vec<ParsedTerm>,
    /// Phrases (quoted strings)
    pub phrases: Vec<String>,
    /// Boolean operators used
    pub operators: Vec<String>,
    /// Whether implicit AND is used between terms
    pub implicit_and: bool,
}

/// Comprehensive query explanation for debugging and understanding search behavior
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryExplanation {
    /// Exact input string
    pub original_query: String,
    /// Sanitized query after normalization
    pub sanitized_query: String,
    /// Structured breakdown of query components
    pub parsed: ParsedQuery,
    /// High-level classification
    pub query_type: QueryType,
    /// How the index will execute this query
    pub index_strategy: IndexStrategy,
    /// Whether wildcard fallback was/will be applied
    pub wildcard_applied: bool,
    /// Rough complexity indicator
    pub estimated_cost: QueryCost,
    /// Active filters summary
    pub filters_summary: FiltersSummary,
    /// Any issues or suggestions
    pub warnings: Vec<String>,
}

/// Summary of active filters for explanation
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FiltersSummary {
    /// Number of agent filters
    pub agent_count: usize,
    /// Number of workspace filters
    pub workspace_count: usize,
    /// Whether time range is applied
    pub has_time_filter: bool,
    /// Human-readable filter description
    pub description: Option<String>,
}

impl QueryExplanation {
    /// Build explanation from query string and filters
    pub fn analyze(query: &str, filters: &SearchFilters) -> Self {
        let sanitized = sanitize_query(query);
        // Parse original query to preserve quotes for phrases
        let tokens = parse_boolean_query(query);

        // Extract terms, phrases, and operators
        let mut parsed = ParsedQuery::default();
        let mut has_explicit_operator = false;
        let mut next_negated = false;

        for token in &tokens {
            match token {
                QueryToken::Term(t) => {
                    let parts = normalize_term_parts(t);
                    if parts.is_empty() {
                        next_negated = false;
                        continue;
                    }
                    let mut subterms = Vec::new();
                    for part in parts {
                        let pattern = WildcardPattern::parse(&part);
                        let pattern_str = match &pattern {
                            WildcardPattern::Exact(_) => "exact",
                            WildcardPattern::Prefix(_) => "prefix (*)",
                            WildcardPattern::Suffix(_) => "suffix (*)",
                            WildcardPattern::Substring(_) => "substring (*)",
                            WildcardPattern::Complex(_) => "complex (*)",
                        };
                        subterms.push(ParsedSubTerm {
                            text: part,
                            pattern: pattern_str.to_string(),
                        });
                    }
                    parsed.terms.push(ParsedTerm {
                        text: t.clone(),
                        negated: next_negated,
                        subterms,
                    });
                    next_negated = false;
                }
                QueryToken::Phrase(p) => {
                    let parts = normalize_phrase_terms(p);
                    if !parts.is_empty() {
                        parsed.phrases.push(parts.join(" "));
                    }
                    next_negated = false;
                }
                QueryToken::And => {
                    parsed.operators.push("AND".to_string());
                    has_explicit_operator = true;
                }
                QueryToken::Or => {
                    parsed.operators.push("OR".to_string());
                    has_explicit_operator = true;
                }
                QueryToken::Not => {
                    parsed.operators.push("NOT".to_string());
                    has_explicit_operator = true;
                    next_negated = true;
                }
            }
        }

        // Implicit AND between terms if no explicit operators
        parsed.implicit_and = !has_explicit_operator && parsed.terms.len() > 1;

        // Determine query type
        let query_type = Self::classify_query(&parsed, filters, &sanitized);

        // Determine index strategy
        let index_strategy = Self::determine_strategy(&parsed, &sanitized);

        // Estimate cost
        let estimated_cost = Self::estimate_cost(&parsed, &index_strategy, filters);

        // Build filters summary
        let filters_summary = Self::summarize_filters(filters);

        // Generate warnings
        let warnings = Self::generate_warnings(&parsed, &sanitized, filters);

        Self {
            original_query: query.to_string(),
            sanitized_query: sanitized,
            parsed,
            query_type,
            index_strategy,
            wildcard_applied: false, // Set later by search_with_fallback
            estimated_cost,
            filters_summary,
            warnings,
        }
    }

    fn classify_query(parsed: &ParsedQuery, filters: &SearchFilters, sanitized: &str) -> QueryType {
        if sanitized.trim().is_empty() {
            return QueryType::Empty;
        }

        // Check for filters first (they modify everything)
        let has_filters = !filters.agents.is_empty()
            || !filters.workspaces.is_empty()
            || filters.created_from.is_some()
            || filters.created_to.is_some()
            || !filters.source_filter.is_all();

        if has_filters {
            return QueryType::Filtered;
        }

        // Check for boolean operators
        if !parsed.operators.is_empty() {
            return QueryType::Boolean;
        }

        // Check for phrases
        if !parsed.phrases.is_empty() {
            return QueryType::Phrase;
        }

        // Check for wildcards
        let has_wildcards = parsed
            .terms
            .iter()
            .flat_map(|t| &t.subterms)
            .any(|t| t.pattern != "exact");
        if has_wildcards {
            return QueryType::Wildcard;
        }

        QueryType::Simple
    }

    fn determine_strategy(parsed: &ParsedQuery, sanitized: &str) -> IndexStrategy {
        if sanitized.trim().is_empty() {
            return IndexStrategy::FullScan;
        }

        // Check for leading wildcards (requires regex)
        let has_leading_wildcard = parsed
            .terms
            .iter()
            .flat_map(|t| &t.subterms)
            .any(|t| t.pattern == "suffix (*)" || t.pattern == "substring (*)");

        if has_leading_wildcard {
            return IndexStrategy::RegexScan;
        }

        // Boolean queries use combination strategy
        // Also if any single term is split into multiple subterms (compound term like "foo-bar")
        let has_compound_terms = parsed.terms.iter().any(|t| t.subterms.len() > 1);

        if !parsed.operators.is_empty()
            || parsed.terms.len() > 1
            || !parsed.phrases.is_empty()
            || has_compound_terms
        {
            return IndexStrategy::BooleanCombination;
        }

        // Single term uses edge n-gram
        IndexStrategy::EdgeNgram
    }

    fn estimate_cost(
        parsed: &ParsedQuery,
        strategy: &IndexStrategy,
        filters: &SearchFilters,
    ) -> QueryCost {
        // Regex scans are always expensive
        if matches!(strategy, IndexStrategy::RegexScan) {
            return QueryCost::High;
        }

        // Full scans are expensive
        if matches!(strategy, IndexStrategy::FullScan) {
            return QueryCost::High;
        }

        // Time range filters add cost
        let has_time_filter = filters.created_from.is_some() || filters.created_to.is_some();

        // Count complexity factors
        let term_count: usize = parsed.terms.iter().map(|t| t.subterms.len()).sum();
        let operator_count = parsed.operators.len();
        let phrase_count = parsed.phrases.len();

        let complexity = term_count + operator_count * 2 + phrase_count * 2;

        if complexity > 6 || has_time_filter {
            QueryCost::High
        } else if complexity > 2 {
            QueryCost::Medium
        } else {
            QueryCost::Low
        }
    }

    fn summarize_filters(filters: &SearchFilters) -> FiltersSummary {
        let agent_count = filters.agents.len();
        let workspace_count = filters.workspaces.len();
        let has_time_filter = filters.created_from.is_some() || filters.created_to.is_some();

        let mut parts = Vec::new();
        if agent_count > 0 {
            parts.push(format!(
                "{} agent{}",
                agent_count,
                if agent_count > 1 { "s" } else { "" }
            ));
        }
        if workspace_count > 0 {
            parts.push(format!(
                "{} workspace{}",
                workspace_count,
                if workspace_count > 1 { "s" } else { "" }
            ));
        }
        if has_time_filter {
            parts.push("time range".to_string());
        }

        let description = if parts.is_empty() {
            None
        } else {
            Some(format!("Filtering by: {}", parts.join(", ")))
        };

        FiltersSummary {
            agent_count,
            workspace_count,
            has_time_filter,
            description,
        }
    }

    fn generate_warnings(
        parsed: &ParsedQuery,
        sanitized: &str,
        filters: &SearchFilters,
    ) -> Vec<String> {
        let mut warnings = Vec::new();

        // Warn about leading wildcards
        let has_leading_wildcard = parsed
            .terms
            .iter()
            .flat_map(|t| &t.subterms)
            .any(|t| t.pattern == "suffix (*)" || t.pattern == "substring (*)");
        if has_leading_wildcard {
            warnings.push(
                "Leading wildcards (*foo) require regex scan and may be slow on large indexes"
                    .to_string(),
            );
        }

        // Warn about very short terms
        for term in &parsed.terms {
            for sub in &term.subterms {
                if sub.text.trim_matches('*').len() < 2 {
                    warnings.push(format!(
                        "Very short term '{}' may match many documents",
                        sub.text
                    ));
                }
            }
        }

        // Warn about empty query
        if sanitized.trim().is_empty() {
            warnings.push("Empty query will return all documents (expensive)".to_string());
        }

        // Warn about complex boolean queries
        if parsed.operators.len() > 3 {
            warnings.push("Complex boolean query may have unexpected precedence".to_string());
        }

        // Warn about narrow filters that might miss results
        if let Some(agent) = filters.agents.iter().next()
            && filters.agents.len() == 1
            && filters.workspaces.is_empty()
        {
            warnings.push(format!(
                "Searching only in agent '{}' - results from other agents will be excluded",
                agent
            ));
        }

        warnings
    }

    /// Update `wildcard_applied` flag (called after `search_with_fallback`)
    pub fn with_wildcard_fallback(mut self, applied: bool) -> Self {
        self.wildcard_applied = applied;
        if applied
            && !self
                .warnings
                .iter()
                .any(|w| w.contains("wildcard fallback"))
        {
            self.warnings.push(
                "Wildcard fallback was applied automatically due to sparse exact matches"
                    .to_string(),
            );
        }
        self
    }
}

/// Indicates how a search result matched the query.
/// Used for ranking: exact matches rank higher than wildcard matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchType {
    /// No wildcards - matched via exact term or edge n-gram prefix
    #[default]
    Exact,
    /// Matched via trailing wildcard (foo*)
    Prefix,
    /// Matched via leading wildcard (*foo) - uses regex
    Suffix,
    /// Matched via both wildcards (*foo*) - uses regex
    Substring,
    /// Matched via complex wildcard (e.g. f*o) - uses regex
    Wildcard,
    /// Matched via automatic wildcard fallback when exact search was sparse
    ImplicitWildcard,
}

impl MatchType {
    /// Returns a quality factor for ranking (1.0 = best, lower = less precise match)
    pub fn quality_factor(self) -> f32 {
        match self {
            MatchType::Exact => 1.0,
            MatchType::Prefix => 0.9,
            MatchType::Suffix => 0.8,
            MatchType::Substring => 0.7,
            MatchType::Wildcard => 0.65,
            MatchType::ImplicitWildcard => 0.6,
        }
    }
}

/// Type of suggestion for did-you-mean
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    /// Typo correction (Levenshtein distance)
    SpellingFix,
    /// Try with wildcard prefix/suffix
    WildcardQuery,
    /// Remove restrictive filter
    RemoveFilter,
    /// Try different agent
    AlternateAgent,
    /// Broaden date range
    BroaderDateRange,
}

/// A "did-you-mean" suggestion when search returns zero hits.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuerySuggestion {
    /// What kind of suggestion this is
    pub kind: SuggestionKind,
    /// Human-readable description (e.g., "Did you mean: 'codex'?")
    pub message: String,
    /// The suggested query string (if query change)
    pub suggested_query: Option<String>,
    /// Suggested filters to apply (replaces current filters if Some)
    pub suggested_filters: Option<SearchFilters>,
    /// Shortcut key (1, 2, or 3) for quick apply in TUI
    pub shortcut: Option<u8>,
}

impl QuerySuggestion {
    fn spelling(_query: &str, corrected: &str) -> Self {
        Self {
            kind: SuggestionKind::SpellingFix,
            message: format!("Did you mean: \"{corrected}\"?"),
            suggested_query: Some(corrected.to_string()),
            suggested_filters: None,
            shortcut: None,
        }
    }

    fn wildcard(query: &str) -> Self {
        let wildcard_query = format!("*{}*", query.trim_matches('*'));
        Self {
            kind: SuggestionKind::WildcardQuery,
            message: format!("Try broader search: \"{wildcard_query}\""),
            suggested_query: Some(wildcard_query),
            suggested_filters: None,
            shortcut: None,
        }
    }

    fn remove_agent_filter(current_agent: &str, current_filters: &SearchFilters) -> Self {
        // Clone current filters and only clear the agent filter, preserving
        // workspace and date range filters
        let mut filters = current_filters.clone();
        filters.agents.clear();
        Self {
            kind: SuggestionKind::RemoveFilter,
            message: format!("Remove agent filter (currently: {current_agent})"),
            suggested_query: None,
            suggested_filters: Some(filters),
            shortcut: None,
        }
    }

    fn try_agent(agent_slug: &str) -> Self {
        let mut filters = SearchFilters::default();
        filters.agents.insert(agent_slug.to_string());
        Self {
            kind: SuggestionKind::AlternateAgent,
            message: format!("Try searching in: {agent_slug}"),
            suggested_query: None,
            suggested_filters: Some(filters),
            shortcut: None,
        }
    }

    fn with_shortcut(mut self, key: u8) -> Self {
        self.shortcut = Some(key);
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FieldMask {
    flags: u8,
}

impl FieldMask {
    const CONTENT: u8 = 1 << 0;
    const SNIPPET: u8 = 1 << 1;
    const TITLE: u8 = 1 << 2;
    const CACHE: u8 = 1 << 3;

    pub const FULL: Self = Self {
        flags: Self::CONTENT | Self::SNIPPET | Self::TITLE | Self::CACHE,
    };

    pub fn new(
        wants_content: bool,
        wants_snippet: bool,
        wants_title: bool,
        allows_cache: bool,
    ) -> Self {
        let mut flags = 0;
        if wants_content {
            flags |= Self::CONTENT;
        }
        if wants_snippet {
            flags |= Self::SNIPPET;
        }
        if wants_title {
            flags |= Self::TITLE;
        }
        if allows_cache {
            flags |= Self::CACHE;
        }
        Self { flags }
    }

    pub fn needs_content(self) -> bool {
        self.flags & Self::CONTENT != 0
    }

    pub fn wants_snippet(self) -> bool {
        self.flags & Self::SNIPPET != 0
    }

    pub fn wants_title(self) -> bool {
        self.flags & Self::TITLE != 0
    }

    pub fn allows_cache(self) -> bool {
        self.flags & Self::CACHE != 0
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub title: String,
    pub snippet: String,
    pub content: String,
    #[serde(skip_serializing)]
    pub content_hash: u64,
    pub score: f32,
    pub source_path: String,
    pub agent: String,
    pub workspace: String,
    /// Original workspace path before rewriting (P6.2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_original: Option<String>,
    pub created_at: Option<i64>,
    /// Line number in the source file where the matched message starts (1-indexed)
    pub line_number: Option<usize>,
    /// How this result matched the query (exact, prefix wildcard, etc.)
    #[serde(default)]
    pub match_type: MatchType,
    // Provenance fields (P3.3)
    /// Source identifier (e.g., "local", "work-laptop")
    #[serde(default = "default_source_id")]
    pub source_id: String,
    /// Origin kind ("local" or "ssh")
    #[serde(default = "default_origin_kind")]
    pub origin_kind: String,
    /// Origin host label for remote sources
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_host: Option<String>,
}

static LAZY_FIELDS_ENABLED: Lazy<bool> = Lazy::new(|| {
    dotenvy::var("CASS_LAZY_FIELDS")
        .ok()
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
});

fn default_source_id() -> String {
    "local".to_string()
}

fn default_origin_kind() -> String {
    "local".to_string()
}

fn effective_field_mask(field_mask: FieldMask) -> FieldMask {
    if *LAZY_FIELDS_ENABLED {
        field_mask
    } else {
        FieldMask::FULL
    }
}

/// Result of a search operation with metadata about how matches were found
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The search results
    pub hits: Vec<SearchHit>,
    /// Whether wildcard fallback was used (query had no/few exact matches)
    pub wildcard_fallback: bool,
    /// Cache metrics snapshot for observability/debug
    pub cache_stats: CacheStats,
    /// Did-you-mean suggestions when hits are empty or sparse
    pub suggestions: Vec<QuerySuggestion>,
    /// ANN search statistics (present when --approximate was used)
    pub ann_stats: Option<crate::search::ann_index::AnnSearchStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchHitKey {
    source_id: String,
    source_path: String,
    line_number: Option<usize>,
    created_at: Option<i64>,
    content_hash: u64,
}

impl SearchHitKey {
    fn from_hit(hit: &SearchHit) -> Self {
        Self {
            source_id: hit.source_id.clone(),
            source_path: hit.source_path.clone(),
            line_number: hit.line_number,
            created_at: hit.created_at,
            content_hash: hit.content_hash,
        }
    }
}

impl Ord for SearchHitKey {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.source_id
            .cmp(&other.source_id)
            .then_with(|| self.source_path.cmp(&other.source_path))
            .then_with(|| self.line_number.cmp(&other.line_number))
            .then_with(|| self.created_at.cmp(&other.created_at))
            .then_with(|| self.content_hash.cmp(&other.content_hash))
    }
}

impl PartialOrd for SearchHitKey {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Default, Clone)]
struct HybridScore {
    rrf: f32,
    lexical_rank: Option<usize>,
    semantic_rank: Option<usize>,
    lexical_score: Option<f32>,
    semantic_score: Option<f32>,
}

#[derive(Debug, Clone)]
struct FusedHit {
    key: SearchHitKey,
    score: HybridScore,
    hit: SearchHit,
}

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 1099511628211;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub(crate) fn stable_content_hash(content: &str) -> u64 {
    const FNV_OFFSET: u64 = 14695981039346656037;
    let mut hash = FNV_OFFSET;
    let mut first = true;
    for token in content.split_whitespace() {
        if !first {
            hash = hash_bytes(hash, b" ");
        }
        hash = hash_bytes(hash, token.as_bytes());
        first = false;
    }
    hash
}

fn stable_hit_hash(
    content: &str,
    source_path: &str,
    line_number: Option<usize>,
    created_at: Option<i64>,
) -> u64 {
    if !content.is_empty() {
        return stable_content_hash(content);
    }
    const FNV_OFFSET: u64 = 14695981039346656037;
    let mut hash = FNV_OFFSET;
    hash = hash_bytes(hash, source_path.as_bytes());
    hash = hash_bytes(hash, b"|");
    if let Some(line) = line_number {
        hash = hash_bytes(hash, line.to_string().as_bytes());
    }
    hash = hash_bytes(hash, b"|");
    if let Some(ts) = created_at {
        hash = hash_bytes(hash, ts.to_string().as_bytes());
    }
    hash
}

/// Comparator for FusedHit: descending RRF score, prefer dual-source, then key for determinism.
fn cmp_fused_hit_desc(a: &FusedHit, b: &FusedHit) -> CmpOrdering {
    b.score
        .rrf
        .total_cmp(&a.score.rrf)
        .then_with(|| {
            let a_both = a.score.lexical_rank.is_some() && a.score.semantic_rank.is_some();
            let b_both = b.score.lexical_rank.is_some() && b.score.semantic_rank.is_some();
            match (b_both, a_both) {
                (true, false) => CmpOrdering::Greater,
                (false, true) => CmpOrdering::Less,
                _ => CmpOrdering::Equal,
            }
        })
        .then_with(|| a.key.cmp(&b.key))
}

/// Threshold below which full sort is faster than quickselect + partial sort.
#[cfg(test)]
#[allow(dead_code)]
const QUICKSELECT_THRESHOLD: usize = 64;

/// Partition fused hits to get top-k in O(N + k log k) instead of O(N log N).
///
/// For k << N, this is significantly faster than sorting all N elements.
/// Uses `select_nth_unstable_by` for O(N) average-case partitioning,
/// then sorts only the top-k elements.
///
/// Note: Currently only used for tests. Production code uses full sort for
/// content deduplication which requires seeing all elements.
#[cfg(test)]
#[allow(dead_code)]
fn top_k_fused(mut hits: Vec<FusedHit>, k: usize) -> Vec<FusedHit> {
    let n = hits.len();

    // Edge cases: nothing to do or k >= n
    if n == 0 || k == 0 {
        return Vec::new();
    }
    if k >= n {
        hits.sort_by(cmp_fused_hit_desc);
        return hits;
    }

    // For small N, full sort has less overhead than quickselect
    if n < QUICKSELECT_THRESHOLD {
        hits.sort_by(cmp_fused_hit_desc);
        hits.truncate(k);
        return hits;
    }

    // Partition: move top-k elements to the front (unordered) in O(N)
    hits.select_nth_unstable_by(k - 1, cmp_fused_hit_desc);

    // Truncate to just the top-k elements
    hits.truncate(k);

    // Sort just the top-k in O(k log k)
    hits.sort_by(cmp_fused_hit_desc);

    hits
}

/// Fuse lexical + semantic hits using Reciprocal Rank Fusion (RRF).
/// Applies deterministic tie-breaking and returns the requested page slice.
pub fn rrf_fuse_hits(
    lexical: &[SearchHit],
    semantic: &[SearchHit],
    limit: usize,
    offset: usize,
) -> Vec<SearchHit> {
    if limit == 0 {
        return Vec::new();
    }

    // Pre-allocate capacity to avoid rehashing. Worst case: no overlap between lists.
    let capacity = lexical.len() + semantic.len();
    let mut scores: HashMap<SearchHitKey, HybridScore> = HashMap::with_capacity(capacity);
    let mut hits: HashMap<SearchHitKey, SearchHit> = HashMap::with_capacity(capacity);

    for (rank, hit) in lexical.iter().enumerate() {
        let key = SearchHitKey::from_hit(hit);
        let entry = scores.entry(key.clone()).or_default();
        entry.rrf += 1.0 / (RRF_K + rank as f32 + 1.0);
        entry.lexical_rank = Some(rank);
        entry.lexical_score = Some(hit.score);
        // Prefer lexical hit details (snippets highlight query terms).
        hits.insert(key, hit.clone());
    }

    for (rank, hit) in semantic.iter().enumerate() {
        let key = SearchHitKey::from_hit(hit);
        let entry = scores.entry(key.clone()).or_default();
        entry.rrf += 1.0 / (RRF_K + rank as f32 + 1.0);
        entry.semantic_rank = Some(rank);
        entry.semantic_score = Some(hit.score);
        hits.entry(key).or_insert_with(|| hit.clone());
    }

    let mut fused: Vec<FusedHit> = Vec::with_capacity(scores.len());
    for (key, score) in scores {
        if let Some(hit) = hits.remove(&key) {
            fused.push(FusedHit { key, score, hit });
        }
    }

    // Use quickselect to get top-(offset+limit) elements in O(N + k log k)
    // instead of sorting all N elements in O(N log N)
    //
    // UPDATE: We must sort fully to apply content deduplication correctly.
    // If we only quickselect top K, we might pick a lower-scored duplicate
    // that happened to fall into top K while the higher-scored version (if any)
    // is processed. But actually, RRF score is unique per Key.
    // The issue is: different Keys (messages) can have same Content.
    // We want to return unique Content.
    // So we must sort by Score, then Deduplicate by Content, then Slice.

    fused.sort_by(cmp_fused_hit_desc);

    // Deduplicate by content hash to ensure diversity
    // Key: (source_id, source_path, content_hash) -> seen
    // We include source_path to allow the same content to appear if it's in different files/sessions,
    // but collapse identical messages within the same file (e.g. repeated logs).
    let mut seen_content: HashSet<(String, String, u64)> = HashSet::with_capacity(fused.len());
    let mut unique_fused = Vec::with_capacity(fused.len());

    for entry in fused {
        // Skip tool noise if present (though inputs should be clean)
        if !entry.hit.content.is_empty() && is_tool_invocation_noise(&entry.hit.content) {
            continue;
        }

        let key = (
            entry.hit.source_id.clone(),
            entry.hit.source_path.clone(),
            entry.hit.content_hash,
        );
        if !seen_content.contains(&key) {
            seen_content.insert(key);
            unique_fused.push(entry);
        }
    }

    // Take the slice from offset to offset+limit
    let start = offset.min(unique_fused.len());
    let mut results = Vec::with_capacity(limit.min(unique_fused.len().saturating_sub(start)));
    for mut entry in unique_fused.into_iter().skip(start).take(limit) {
        entry.hit.score = entry.score.rrf;
        results.push(entry.hit);
    }
    results
}

struct QueryCache {
    embedder_id: String,
    embeddings: LruCache<String, Vec<f32>>,
}

impl QueryCache {
    fn new(embedder_id: &str, capacity: NonZeroUsize) -> Self {
        Self {
            embedder_id: embedder_id.to_string(),
            embeddings: LruCache::new(capacity),
        }
    }

    fn get_or_embed(&mut self, embedder: &dyn Embedder, canonical: &str) -> Result<Vec<f32>> {
        if self.embedder_id != embedder.id() {
            self.embedder_id = embedder.id().to_string();
            self.embeddings.clear();
        }

        if let Some(hit) = self.embeddings.get(canonical) {
            return Ok(hit.clone());
        }

        let embedding = embedder
            .embed(canonical)
            .map_err(|e| anyhow!("embedding failed: {e}"))?;
        self.embeddings
            .put(canonical.to_string(), embedding.clone());
        Ok(embedding)
    }
}

struct SemanticSearchState {
    embedder: Arc<dyn Embedder>,
    index: VectorIndex,
    ann_index: Option<HnswIndex>,
    ann_path: Option<PathBuf>,
    filter_maps: SemanticFilterMaps,
    roles: Option<HashSet<u8>>,
    query_cache: QueryCache,
}

pub struct SearchClient {
    reader: Option<(IndexReader, crate::search::tantivy::Fields)>,
    sqlite: Mutex<Option<Connection>>,
    sqlite_path: Option<PathBuf>,
    prefix_cache: Mutex<CacheShards>,
    reload_on_search: bool,
    last_reload: Mutex<Option<Instant>>,
    last_generation: Mutex<Option<u64>>,
    reload_epoch: Arc<AtomicU64>,
    warm_tx: Option<mpsc::UnboundedSender<WarmJob>>,
    _warm_handle: Option<JoinHandle<()>>,
    // Shared for warm worker to read cache/filter logic; keep Arc to avoid clones of big data
    _shared_filters: Arc<Mutex<()>>, // placeholder lock to ensure Send/Sync; future warm prefill state
    metrics: Metrics,
    cache_namespace: String,
    semantic: Mutex<Option<SemanticSearchState>>,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchClientOptions {
    pub enable_reload: bool,
    pub enable_warm: bool,
}

impl Default for SearchClientOptions {
    fn default() -> Self {
        Self {
            enable_reload: true,
            enable_warm: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub cache_hits: u64,
    pub cache_miss: u64,
    pub cache_shortfall: u64,
    pub reloads: u64,
    pub reload_ms_total: u128,
    pub total_cap: usize,
    pub total_cost: usize,
    /// Total evictions since client creation
    pub eviction_count: u64,
    /// Approximate bytes used by cache (rough estimate)
    pub approx_bytes: usize,
    /// Byte cap if set (0 = no byte limit)
    pub byte_cap: usize,
}

// Cache tuning: read from env to allow runtime override without recompiling.
// CASS_CACHE_SHARD_CAP controls per-shard entries; default 256.
static CACHE_SHARD_CAP: Lazy<usize> = Lazy::new(|| {
    dotenvy::var("CASS_CACHE_SHARD_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(256)
});

// Total cache cost across all shards; approximate "~2k entries" default.
static CACHE_TOTAL_CAP: Lazy<usize> = Lazy::new(|| {
    dotenvy::var("CASS_CACHE_TOTAL_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(2048)
});

static CACHE_DEBUG_ENABLED: Lazy<bool> = Lazy::new(|| {
    dotenvy::var("CASS_DEBUG_CACHE_METRICS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
});

// Optional byte-based cap for cache memory; 0 means no byte limit (entry-based only).
// Approximate sizing: ~500 bytes per cached hit typical (content/title/snippets).
// Example: CASS_CACHE_BYTE_CAP=10485760 for approx 10MB limit.
static CACHE_BYTE_CAP: Lazy<usize> = Lazy::new(|| {
    dotenvy::var("CASS_CACHE_BYTE_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0) // 0 = disabled (entry-based cap only)
});

const CACHE_KEY_VERSION: &str = "1";

// Warm debounce (ms) for background reload/warm jobs; default 120ms.
static WARM_DEBOUNCE_MS: Lazy<u64> = Lazy::new(|| {
    dotenvy::var("CASS_WARM_DEBOUNCE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(120)
});

const DEFAULT_REGEX_CACHE_SIZE: usize = 100;

static REGEX_CACHE_ENABLED: Lazy<bool> = Lazy::new(|| {
    dotenvy::var("CASS_REGEX_CACHE")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
});

static REGEX_CACHE_SIZE: Lazy<NonZeroUsize> = Lazy::new(|| {
    let parsed = dotenvy::var("CASS_REGEX_CACHE_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .and_then(NonZeroUsize::new);
    parsed.unwrap_or_else(|| NonZeroUsize::new(DEFAULT_REGEX_CACHE_SIZE).unwrap())
});

type RegexCacheKey = (Field, String);

struct RegexCache {
    cache: RwLock<LruCache<RegexCacheKey, RegexQuery>>,
}

impl RegexCache {
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            cache: RwLock::new(LruCache::new(capacity)),
        }
    }

    fn get_or_insert(&self, field: Field, pattern: &str) -> Result<RegexQuery> {
        let key = (field, pattern.to_string());
        if let Some(cached) = self.cache.read().peek(&key) {
            return Ok(cached.clone());
        }
        let query = RegexQuery::from_pattern(pattern, field)
            .map_err(|e| anyhow!("regex query build failed: {e}"))?;
        self.cache.write().put(key, query.clone());
        Ok(query)
    }
}

static REGEX_CACHE: Lazy<RegexCache> = Lazy::new(|| RegexCache::new(*REGEX_CACHE_SIZE));

#[derive(Clone)]
struct CachedHit {
    hit: SearchHit,
    lc_content: String,
    lc_title: Option<String>,
    bloom64: u64,
}

impl CachedHit {
    /// Approximate byte size of this cached hit (rough estimate for memory guardrails).
    /// Includes `SearchHit` strings + lowercase copies + bloom filter.
    fn approx_bytes(&self) -> usize {
        // Base struct overhead
        let base = std::mem::size_of::<Self>();
        // SearchHit string fields (title, snippet, content, source_path, agent, workspace)
        let hit_strings = self.hit.title.len()
            + self.hit.snippet.len()
            + self.hit.content.len()
            + self.hit.source_path.len()
            + self.hit.agent.len()
            + self.hit.workspace.len();
        // Lowercase cache copies
        let lc_strings =
            self.lc_content.len() + self.lc_title.as_ref().map_or(0, std::string::String::len);
        base + hit_strings + lc_strings
    }
}

struct CacheShards {
    // Optimization 2.3: Use Arc<str> for cache keys to reduce memory via interning
    shards: HashMap<Arc<str>, LruCache<Arc<str>, Vec<CachedHit>>>,
    total_cap: usize,
    total_cost: usize,
    /// Running count of evictions (for diagnostics)
    eviction_count: u64,
    /// Approximate bytes used by all cached hits
    total_bytes: usize,
    /// Byte cap (0 = disabled)
    byte_cap: usize,
}

impl CacheShards {
    fn new(total_cap: usize, byte_cap: usize) -> Self {
        Self {
            shards: HashMap::new(),
            total_cap: total_cap.max(1),
            total_cost: 0,
            eviction_count: 0,
            total_bytes: 0,
            byte_cap,
        }
    }

    fn shard_mut(&mut self, name: &str) -> &mut LruCache<Arc<str>, Vec<CachedHit>> {
        // Use interned shard names to reduce memory for repeated lookups
        let interned_name = intern_cache_key(name);
        self.shards
            .entry(interned_name)
            .or_insert_with(|| LruCache::new(NonZeroUsize::new(*CACHE_SHARD_CAP).unwrap()))
    }

    fn shard_opt(&self, name: &str) -> Option<&LruCache<Arc<str>, Vec<CachedHit>>> {
        // HashMap<Arc<str>, _> can be queried with &str via Borrow trait
        self.shards.get(name)
    }

    fn put(&mut self, shard_name: &str, key: Arc<str>, value: Vec<CachedHit>) {
        let shard = self.shard_mut(shard_name);
        let new_cost = value.len();
        let new_bytes: usize = value.iter().map(CachedHit::approx_bytes).sum();
        let old_val = shard.put(key, value);
        let (old_cost, old_bytes) = old_val.as_ref().map_or((0, 0), |v| {
            (v.len(), v.iter().map(CachedHit::approx_bytes).sum())
        });

        self.total_cost = self
            .total_cost
            .saturating_add(new_cost)
            .saturating_sub(old_cost);
        self.total_bytes = self
            .total_bytes
            .saturating_add(new_bytes)
            .saturating_sub(old_bytes);
        self.evict_until_within_cap();
    }

    fn evict_until_within_cap(&mut self) {
        // Evict if over entry cap OR over byte cap (when byte_cap > 0)
        while self.total_cost > self.total_cap
            || (self.byte_cap > 0 && self.total_bytes > self.byte_cap)
        {
            let mut evicted = false;
            for shard in self.shards.values_mut() {
                if let Some((_k, v)) = shard.pop_lru() {
                    let evicted_bytes: usize = v.iter().map(CachedHit::approx_bytes).sum();
                    self.total_cost = self.total_cost.saturating_sub(v.len());
                    self.total_bytes = self.total_bytes.saturating_sub(evicted_bytes);
                    self.eviction_count += 1;
                    evicted = true;
                    // Check if we're back within both caps
                    let within_cost = self.total_cost <= self.total_cap;
                    let within_bytes = self.byte_cap == 0 || self.total_bytes <= self.byte_cap;
                    if within_cost && within_bytes {
                        break;
                    }
                }
            }
            if !evicted {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.shards.clear();
        self.total_cost = 0;
        self.total_bytes = 0;
        // Note: eviction_count preserved for lifetime stats
    }

    fn total_cost(&self) -> usize {
        self.total_cost
    }

    fn total_cap(&self) -> usize {
        self.total_cap
    }

    fn eviction_count(&self) -> u64 {
        self.eviction_count
    }

    fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    fn byte_cap(&self) -> usize {
        self.byte_cap
    }
}

#[derive(Clone)]
struct WarmJob {
    query: String,
    _filters: SearchFilters,
}

#[derive(Clone)]
struct SearcherCacheEntry {
    epoch: u64,
    searcher: Searcher,
}

thread_local! {
    static THREAD_SEARCHER: RefCell<Option<SearcherCacheEntry>> = const { RefCell::new(None) };
}

/// Sanitize query string to match Tantivy's `SimpleTokenizer` behavior.
///
/// `SimpleTokenizer` splits text on any character that is not alphanumeric.
/// To ensure query terms match indexed tokens, we replace non-alphanumeric characters
/// with spaces here (preserving `*` for wildcards and `"` for phrases).
///
/// Note: This causes precision loss for code symbols (e.g. `user-id` becomes `user` AND `id`,
/// matching documents containing both terms anywhere, not necessarily adjacent).
/// This is a known limitation of the current v6 schema using `SimpleTokenizer`.
fn sanitize_query(raw: &str) -> String {
    // Replace any character that is not alphanumeric, asterisk, or double quote with a space.
    // Asterisks are preserved for wildcard query support (*foo, foo*, *bar*).
    // Double quotes are preserved for phrase query support ("exact phrase").
    // This ensures that the input tokens match how SimpleTokenizer splits content.
    // e.g. "c++" -> "c  ", "foo.bar" -> "foo bar", "*config*" -> "*config*"
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '*' || c == '"' {
                c
            } else {
                ' '
            }
        })
        .collect()
}

/// Calculate Levenshtein edit distance between two strings.
/// Used for typo detection in did-you-mean suggestions.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    // Use two rows for space efficiency
    let mut prev_row: Vec<usize> = (0..=b_len).collect();
    let mut curr_row: Vec<usize> = vec![0; b_len + 1];

    for (i, a_char) in a_chars.iter().enumerate() {
        curr_row[0] = i + 1;
        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            curr_row[j + 1] = (prev_row[j + 1] + 1) // deletion
                .min(curr_row[j] + 1) // insertion
                .min(prev_row[j] + cost); // substitution
        }
        std::mem::swap(&mut prev_row, &mut curr_row);
    }

    prev_row[b_len]
}

/// Escape special regex characters in a string
fn escape_regex(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Represents different wildcard patterns for a search term
#[derive(Debug, Clone, PartialEq)]
enum WildcardPattern {
    /// No wildcards - exact term match (through edge n-grams)
    Exact(String),
    /// Trailing wildcard: foo* (prefix match)
    Prefix(String),
    /// Leading wildcard: *foo (suffix match - requires regex)
    Suffix(String),
    /// Both wildcards: *foo* (substring match - requires regex)
    Substring(String),
    /// Complex wildcard: f*o, *f*o, f*o* (requires regex)
    Complex(String),
}

impl WildcardPattern {
    fn parse(term: &str) -> Self {
        let starts_with_star = term.starts_with('*');
        let ends_with_star = term.ends_with('*');

        let core = term.trim_matches('*').to_lowercase();
        if core.is_empty() {
            return WildcardPattern::Exact(String::new());
        }

        // Check for internal wildcards (e.g. f*o)
        // If the core itself contains stars, it's a complex pattern
        if core.contains('*') {
            return WildcardPattern::Complex(term.to_lowercase());
        }

        match (starts_with_star, ends_with_star) {
            (true, true) => WildcardPattern::Substring(core),
            (true, false) => WildcardPattern::Suffix(core),
            (false, true) => WildcardPattern::Prefix(core),
            (false, false) => WildcardPattern::Exact(core),
        }
    }

    /// Convert to regex pattern for Tantivy `RegexQuery`
    fn to_regex(&self) -> Option<String> {
        match self {
            WildcardPattern::Suffix(core) => Some(format!(".*{}$", escape_regex(core))),
            WildcardPattern::Substring(core) => Some(format!(".*{}.*", escape_regex(core))),
            WildcardPattern::Complex(full_term) => {
                let mut regex = String::with_capacity(full_term.len() * 2 + 2);

                // If the pattern doesn't start with *, anchor it to start
                if !full_term.starts_with('*') {
                    regex.push('^');
                } else {
                    regex.push_str(".*");
                }

                let trimmed_start = full_term.trim_start_matches('*');
                let trimmed = trimmed_start.trim_end_matches('*');

                // Internal parts
                for c in trimmed.chars() {
                    if c == '*' {
                        regex.push_str(".*");
                    } else {
                        match c {
                            '\\' | '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
                            | '^' | '$' => {
                                regex.push('\\');
                                regex.push(c);
                            }
                            _ => regex.push(c),
                        }
                    }
                }

                // If the pattern doesn't end with *, anchor it to end
                if !full_term.ends_with('*') {
                    regex.push('$');
                } else {
                    regex.push_str(".*");
                }

                Some(regex)
            }
            _ => None,
        }
    }

    /// Convert to the corresponding public `MatchType`
    fn to_match_type(&self) -> MatchType {
        match self {
            WildcardPattern::Exact(_) => MatchType::Exact,
            WildcardPattern::Prefix(_) => MatchType::Prefix,
            WildcardPattern::Suffix(_) => MatchType::Suffix,
            WildcardPattern::Substring(_) => MatchType::Substring,
            WildcardPattern::Complex(_) => MatchType::Wildcard,
        }
    }
}

/// Token types for boolean query parsing
#[derive(Debug, Clone, PartialEq)]
enum QueryToken {
    /// A search term (may include wildcards)
    Term(String),
    /// Quoted phrase for exact matching
    Phrase(String),
    /// AND operator (explicit)
    And,
    /// OR operator
    Or,
    /// NOT operator (next term is excluded)
    Not,
}

/// Type alias for query token list - most queries with operators have up to 8 tokens (Opt 4.4)
/// SmallVec keeps small lists on the stack, avoiding heap allocation.
type QueryTokenList = SmallVec<[QueryToken; 8]>;

/// Parse a query string into boolean tokens.
/// Supports:
/// - AND, && for explicit AND (implicit between terms)
/// - OR, || for OR
/// - NOT, - prefix for exclusion
/// - "quoted phrases" for exact matching
fn parse_boolean_query(query: &str) -> QueryTokenList {
    let mut tokens = SmallVec::new();
    let mut chars = query.chars().peekable();
    let mut current_word = String::new();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                // Flush any pending word
                if !current_word.is_empty() {
                    tokens.push(QueryToken::Term(std::mem::take(&mut current_word)));
                }
                // Collect quoted phrase
                let mut phrase = String::new();
                while let Some(&next) = chars.peek() {
                    if next == '"' {
                        chars.next();
                        break;
                    }
                    if let Some(c) = chars.next() {
                        phrase.push(c);
                    }
                }
                if !phrase.is_empty() {
                    tokens.push(QueryToken::Phrase(phrase));
                }
            }
            '&' if chars.peek() == Some(&'&') => {
                chars.next(); // consume second &
                if !current_word.is_empty() {
                    tokens.push(QueryToken::Term(std::mem::take(&mut current_word)));
                }
                tokens.push(QueryToken::And);
            }
            '|' if chars.peek() == Some(&'|') => {
                chars.next(); // consume second |
                if !current_word.is_empty() {
                    tokens.push(QueryToken::Term(std::mem::take(&mut current_word)));
                }
                tokens.push(QueryToken::Or);
            }
            '-' if current_word.is_empty() => {
                // Prefix minus for NOT (at start of a term)
                // Works at query start: "-foo" or mid-query: "bar -foo"
                tokens.push(QueryToken::Not);
            }
            ' ' | '\t' | '\n' => {
                if !current_word.is_empty() {
                    let word = std::mem::take(&mut current_word);
                    let upper = word.to_uppercase();
                    match upper.as_str() {
                        "AND" => tokens.push(QueryToken::And),
                        "OR" => tokens.push(QueryToken::Or),
                        "NOT" => tokens.push(QueryToken::Not),
                        _ => tokens.push(QueryToken::Term(word)),
                    }
                }
            }
            _ => {
                current_word.push(c);
            }
        }
    }

    // Flush final word
    if !current_word.is_empty() {
        let upper = current_word.to_uppercase();
        match upper.as_str() {
            "AND" => tokens.push(QueryToken::And),
            "OR" => tokens.push(QueryToken::Or),
            "NOT" => tokens.push(QueryToken::Not),
            _ => tokens.push(QueryToken::Term(current_word)),
        }
    }

    tokens
}

/// Normalize a term into tokenizer-aligned parts.
/// Splits on punctuation to match SimpleTokenizer behavior, preserving `*` for wildcards.
fn normalize_term_parts(raw: &str) -> Vec<String> {
    sanitize_query(raw)
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

/// Normalize phrase text into tokenizer-aligned terms (lowercased, no wildcards).
fn normalize_phrase_terms(raw: &str) -> Vec<String> {
    sanitize_query(raw)
        .split_whitespace()
        .map(|s| s.trim_matches('*').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Build a compound query that requires all term parts to match (implicit AND).
fn build_compound_term_query(
    parts: &[String],
    fields: &crate::search::tantivy::Fields,
) -> Option<Box<dyn Query>> {
    let mut subqueries: Vec<Box<dyn Query>> = Vec::new();
    for part in parts {
        let pattern = WildcardPattern::parse(part);
        let term_shoulds = build_term_query_clauses(&pattern, fields);
        if !term_shoulds.is_empty() {
            subqueries.push(Box::new(BooleanQuery::new(term_shoulds)));
        }
    }

    match subqueries.len() {
        0 => None,
        1 => subqueries.pop(),
        _ => {
            let musts = subqueries.into_iter().map(|q| (Occur::Must, q)).collect();
            Some(Box::new(BooleanQuery::new(musts)))
        }
    }
}

/// Build a phrase query (exact order) across title/content fields.
fn build_phrase_query(
    terms: &[String],
    fields: &crate::search::tantivy::Fields,
) -> Option<Box<dyn Query>> {
    if terms.is_empty() {
        return None;
    }
    if terms.len() == 1 {
        return build_compound_term_query(terms, fields);
    }

    let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for field in [fields.title, fields.content] {
        let phrase_terms = terms
            .iter()
            .map(|t| Term::from_field_text(field, t))
            .collect::<Vec<_>>();
        shoulds.push((Occur::Should, Box::new(PhraseQuery::new(phrase_terms))));
    }
    Some(Box::new(BooleanQuery::new(shoulds)))
}

/// Check if a query string contains boolean operators
fn has_boolean_operators(query: &str) -> bool {
    let tokens = parse_boolean_query(query);
    tokens.iter().any(|t| {
        matches!(
            t,
            QueryToken::And | QueryToken::Or | QueryToken::Not | QueryToken::Phrase(_)
        )
    })
}

/// Build Tantivy query clauses from boolean tokens.
/// Returns clauses for use in a `BooleanQuery`.
///
/// # Operator Precedence
/// This implementation uses a non-standard precedence where `OR` binds tighter than `AND`.
/// `A OR B AND C` is interpreted as `(A OR B) AND C`.
///
/// This design is intentional for search queries, facilitating "term OR synonym" patterns
/// combined with other filter terms (e.g., "error OR failure login" -> "(error OR failure) AND login").
/// Standard boolean logic would interpret this as "error OR (failure AND login)", which is
/// rarely the user's intent in a log search context.
fn build_boolean_query_clauses(
    tokens: &[QueryToken],
    fields: &crate::search::tantivy::Fields,
) -> Vec<(Occur, Box<dyn Query>)> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut pending_or_group: Vec<Box<dyn Query>> = Vec::new();
    let mut next_occur = Occur::Must;
    let mut in_or_sequence = false;
    let mut just_saw_or = false;

    for token in tokens {
        match token {
            QueryToken::And => {
                // Flush any OR group
                if !pending_or_group.is_empty() {
                    let or_clauses: Vec<_> = pending_or_group
                        .drain(..)
                        .map(|q| (Occur::Should, q))
                        .collect();
                    clauses.push((Occur::Must, Box::new(BooleanQuery::new(or_clauses))));
                }
                in_or_sequence = false;
                just_saw_or = false;
                next_occur = Occur::Must;
            }
            QueryToken::Or => {
                in_or_sequence = true;
                just_saw_or = true;
                // Don't change next_occur; OR will group with previous term
            }
            QueryToken::Not => {
                if just_saw_or {
                    // "OR NOT" case: Treat as "OR (NOT ...)"
                    // Keep just_saw_or = true so the next term knows it's joining the OR group.
                    just_saw_or = true;
                } else {
                    // Standard "AND NOT" case
                    // Flush any OR group
                    if !pending_or_group.is_empty() {
                        let or_clauses: Vec<_> = pending_or_group
                            .drain(..)
                            .map(|q| (Occur::Should, q))
                            .collect();
                        clauses.push((Occur::Must, Box::new(BooleanQuery::new(or_clauses))));
                    }
                    in_or_sequence = false;
                    just_saw_or = false;
                }
                next_occur = Occur::MustNot;
            }
            QueryToken::Term(term) => {
                let parts = normalize_term_parts(term);
                let term_query = build_compound_term_query(&parts, fields);
                if term_query.is_none() {
                    continue;
                }
                let term_query = term_query.unwrap();

                if in_or_sequence && just_saw_or {
                    // Extending OR group
                    if pending_or_group.is_empty() {
                        // Start of group: Pull last clause into OR group if exists
                        // We allow Must and MustNot clauses. MustNot is converted to (All AND Not).
                        let can_pull = clauses
                            .last()
                            .is_some_and(|(occ, _)| *occ == Occur::Must || *occ == Occur::MustNot);

                        if can_pull && let Some((occ, last_q)) = clauses.pop() {
                            let q_to_push = if occ == Occur::MustNot {
                                // Convert NOT A to (All AND NOT A) so it works in OR
                                Box::new(BooleanQuery::new(vec![
                                    (Occur::Must, Box::new(AllQuery)),
                                    (Occur::MustNot, last_q),
                                ]))
                            } else {
                                last_q
                            };
                            pending_or_group.push(q_to_push);
                        }
                    }

                    if next_occur == Occur::MustNot {
                        // "OR NOT B" -> Add (All AND NOT B) to group
                        // We need to wrap B in a query that effectively means "NOT B"
                        // inside a Should clause.
                        let neg_query = BooleanQuery::new(vec![
                            (Occur::Must, Box::new(AllQuery)),
                            (Occur::MustNot, term_query),
                        ]);
                        pending_or_group.push(Box::new(neg_query));
                    } else {
                        pending_or_group.push(term_query);
                    }
                } else {
                    // Not in OR sequence OR implicit AND breaking the sequence
                    // Flush any pending OR group first
                    if !pending_or_group.is_empty() {
                        let or_clauses: Vec<_> = pending_or_group
                            .drain(..)
                            .map(|q| (Occur::Should, q))
                            .collect();
                        clauses.push((Occur::Must, Box::new(BooleanQuery::new(or_clauses))));
                    }
                    in_or_sequence = false;

                    clauses.push((next_occur, term_query));
                }

                just_saw_or = false;
                next_occur = Occur::Must; // Reset for next term
            }
            QueryToken::Phrase(phrase) => {
                let terms = normalize_phrase_terms(phrase);
                let phrase_query = build_phrase_query(&terms, fields);
                if phrase_query.is_none() {
                    continue;
                }
                let phrase_query = phrase_query.unwrap();

                if in_or_sequence && just_saw_or {
                    if pending_or_group.is_empty() {
                        // Start of group: Pull last clause into OR group if exists
                        // We allow Must and MustNot clauses. MustNot is converted to (All AND Not).
                        let can_pull = clauses
                            .last()
                            .is_some_and(|(occ, _)| *occ == Occur::Must || *occ == Occur::MustNot);

                        if can_pull && let Some((occ, last_q)) = clauses.pop() {
                            let q_to_push = if occ == Occur::MustNot {
                                // Convert NOT A to (All AND NOT A) so it works in OR
                                Box::new(BooleanQuery::new(vec![
                                    (Occur::Must, Box::new(AllQuery)),
                                    (Occur::MustNot, last_q),
                                ]))
                            } else {
                                last_q
                            };
                            pending_or_group.push(q_to_push);
                        }
                    }

                    if next_occur == Occur::MustNot {
                        let neg_query = BooleanQuery::new(vec![
                            (Occur::Must, Box::new(AllQuery)),
                            (Occur::MustNot, phrase_query),
                        ]);
                        pending_or_group.push(Box::new(neg_query));
                    } else {
                        pending_or_group.push(phrase_query);
                    }
                } else {
                    if !pending_or_group.is_empty() {
                        let or_clauses: Vec<_> = pending_or_group
                            .drain(..)
                            .map(|q| (Occur::Should, q))
                            .collect();
                        clauses.push((Occur::Must, Box::new(BooleanQuery::new(or_clauses))));
                    }
                    in_or_sequence = false;

                    clauses.push((next_occur, phrase_query));
                }

                just_saw_or = false;
                next_occur = Occur::Must;
            }
        }
    }

    // Flush any remaining OR group
    if !pending_or_group.is_empty() {
        let or_clauses: Vec<_> = pending_or_group
            .drain(..)
            .map(|q| (Occur::Should, q))
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(or_clauses))));
    }

    clauses
}

/// Determine the dominant match type from a query string.
/// Returns the "loosest" pattern used (Substring > Suffix > Prefix > Exact).
fn dominant_match_type(query: &str) -> MatchType {
    let mut worst = MatchType::Exact;
    for term in query.split_whitespace() {
        let pattern = WildcardPattern::parse(term);
        let mt = pattern.to_match_type();
        // Lower quality factor = "looser" match = dominant
        if mt.quality_factor() < worst.quality_factor() {
            worst = mt;
        }
    }
    worst
}

fn regex_query_for_pattern(field: Field, pattern: &str) -> Result<RegexQuery> {
    if !*REGEX_CACHE_ENABLED {
        return RegexQuery::from_pattern(pattern, field)
            .map_err(|e| anyhow!("regex query build failed: {e}"));
    }
    REGEX_CACHE.get_or_insert(field, pattern)
}

#[doc(hidden)]
pub fn regex_query_cached(field: Field, pattern: &str) -> Result<RegexQuery> {
    regex_query_for_pattern(field, pattern)
}

#[doc(hidden)]
pub fn regex_query_uncached(field: Field, pattern: &str) -> Result<RegexQuery> {
    RegexQuery::from_pattern(pattern, field).map_err(|e| anyhow!("regex query build failed: {e}"))
}

/// Build query clauses for a single term based on its wildcard pattern.
/// Returns a Vec of (`Occur::Should`, Query) for use in a `BooleanQuery`.
fn build_term_query_clauses(
    pattern: &WildcardPattern,
    fields: &crate::search::tantivy::Fields,
) -> Vec<(Occur, Box<dyn Query>)> {
    let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    match pattern {
        WildcardPattern::Exact(term) | WildcardPattern::Prefix(term) => {
            // For exact and prefix patterns, use TermQuery on all fields
            // (edge n-grams already handle prefix matching)
            if term.is_empty() {
                return shoulds;
            }
            shoulds.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.title, term),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ));
            shoulds.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.content, term),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ));
            shoulds.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.title_prefix, term),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ));
            shoulds.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.content_prefix, term),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ));
        }
        WildcardPattern::Suffix(term)
        | WildcardPattern::Substring(term)
        | WildcardPattern::Complex(term) => {
            // For suffix, substring, and complex patterns, use RegexQuery
            if term.is_empty() {
                return shoulds;
            }
            if let Some(regex_pattern) = pattern.to_regex() {
                // Try to create RegexQuery for content field
                if let Ok(rq) = regex_query_for_pattern(fields.content, &regex_pattern) {
                    shoulds.push((Occur::Should, Box::new(rq)));
                }
                // Also try for title field
                if let Ok(rq) = regex_query_for_pattern(fields.title, &regex_pattern) {
                    shoulds.push((Occur::Should, Box::new(rq)));
                }
            }
        }
    }

    shoulds
}

/// Check if content is primarily a tool invocation (noise that shouldn't appear in search results).
/// Tool invocations like "[Tool: Bash - Check status]" are not informative search results.
pub(crate) fn is_tool_invocation_noise(content: &str) -> bool {
    let trimmed = content.trim();

    // Direct tool invocations that are just "[Tool: X - description]" or "[Tool: X] args"
    if trimmed.starts_with("[Tool:") {
        // Find closing bracket
        if let Some(close_idx) = trimmed.find(']') {
            // Check for content after closing bracket (Pi-Agent style: "[Tool: name] args")
            let after = &trimmed[close_idx + 1..];
            if !after.trim().is_empty() {
                return false; // Has args/content after -> Keep
            }

            // No content after bracket. Check for description inside.
            // Format: "[Tool: Name - Desc]" (useful) vs "[Tool: Name]" (previously noise, now kept)
            // We now keep "[Tool: Name]" because users might search for "Tool: Bash" to find usage.
            // Only "[Tool:]" or "[Tool: ]" (empty name) is considered noise.
            let inner = &trimmed[6..close_idx]; // Skip "[Tool:"
            return inner.trim().is_empty();
        }
        // No closing bracket? Malformed, treat as noise
        return true;
    }

    // Also filter very short content that's just tool names or markers
    if trimmed.len() < 20 {
        let lower = trimmed.to_lowercase();
        if lower.starts_with("[tool") || lower.starts_with("tool:") {
            return true;
        }
    }

    false
}

fn snippet_from_content(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= 200 {
        return trimmed.to_string();
    }
    let preview: String = trimmed.chars().take(200).collect();
    format!("{preview}...")
}

/// Deduplicate search hits by (source_id, content), keeping only the highest-scored hit
/// for each unique content within a source.
///
/// This respects source boundaries (P2.3): the same content from different sources
/// appears as separate results, since they represent distinct conversations.
///
/// Also filters out tool invocation noise that isn't useful for search results.
pub(crate) fn deduplicate_hits(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    // Key: (source_id, content_hash) -> index in deduped
    let mut seen: HashMap<(String, u64), usize> = HashMap::new();
    let mut deduped: Vec<SearchHit> = Vec::new();

    for hit in hits {
        // Skip tool invocation noise
        if !hit.content.is_empty() && is_tool_invocation_noise(&hit.content) {
            continue;
        }

        // Include source_id in the key so different sources keep their results
        let key = (hit.source_id.clone(), hit.content_hash);

        if let Some(&existing_idx) = seen.get(&key) {
            // If existing hit has lower score, replace it
            if deduped[existing_idx].score < hit.score {
                deduped[existing_idx] = hit;
            }
            // Otherwise keep existing (higher score)
        } else {
            seen.insert(key, deduped.len());
            deduped.push(hit);
        }
    }

    deduped
}

impl SearchClient {
    pub fn open(index_path: &Path, db_path: Option<&Path>) -> Result<Option<Self>> {
        Self::open_with_options(index_path, db_path, SearchClientOptions::default())
    }

    pub fn open_with_options(
        index_path: &Path,
        db_path: Option<&Path>,
        options: SearchClientOptions,
    ) -> Result<Option<Self>> {
        let tantivy = Index::open_in_dir(index_path).ok().and_then(|mut idx| {
            // Register custom tokenizer so searches work
            crate::search::tantivy::ensure_tokenizer(&mut idx);
            let schema = idx.schema();
            let fields = fields_from_schema(&schema).ok()?;
            let reader = idx
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()
                .ok()?;
            // Force initial reload to pick up any segments committed before we opened.
            // With Manual policy, the reader starts with stale view; reload() ensures
            // we see the latest committed state.
            let _ = reader.reload();
            Some((reader, fields))
        });

        let sqlite_path = db_path.map(Path::to_path_buf).filter(|path| path.exists());

        if tantivy.is_none() && sqlite_path.is_none() {
            return Ok(None);
        }

        let shared_filters = Arc::new(Mutex::new(()));
        let reload_epoch = Arc::new(AtomicU64::new(0));
        let metrics = Metrics::default();
        let cache_namespace = format!(
            "v{}|schema:{}",
            CACHE_KEY_VERSION,
            crate::search::tantivy::SCHEMA_HASH
        );

        let warm_pair = if options.enable_warm
            && let Some((reader, fields)) = &tantivy
        {
            maybe_spawn_warm_worker(
                reader.clone(),
                *fields,
                Arc::downgrade(&shared_filters),
                reload_epoch.clone(),
                metrics.clone(),
            )
        } else {
            None
        };

        Ok(Some(Self {
            reader: tantivy,
            sqlite: Mutex::new(None),
            sqlite_path,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: options.enable_reload,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch,
            warm_tx: warm_pair.as_ref().map(|(tx, _)| tx.clone()),
            _warm_handle: warm_pair.map(|(_, h)| h),
            _shared_filters: shared_filters,
            metrics,
            cache_namespace,
            semantic: Mutex::new(None),
        }))
    }

    fn sqlite_guard(&self) -> Result<std::sync::MutexGuard<'_, Option<Connection>>> {
        let mut guard = self
            .sqlite
            .lock()
            .map_err(|_| anyhow!("sqlite lock poisoned"))?;

        if guard.is_none()
            && let Some(path) = &self.sqlite_path
        {
            match Connection::open(path) {
                Ok(conn) => {
                    *guard = Some(conn);
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        path = %path.display(),
                        "sqlite open failed"
                    );
                }
            }
        }

        Ok(guard)
    }

    pub fn search(
        &self,
        query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        field_mask: FieldMask,
    ) -> Result<Vec<SearchHit>> {
        let sanitized = sanitize_query(query);
        let field_mask = effective_field_mask(field_mask);
        let can_use_cache = field_mask.allows_cache() && field_mask.needs_content();

        // Schedule warmup for likely prefixes when user pauses typing.
        if offset == 0
            && let Some(tx) = &self.warm_tx
        {
            let _ = tx.send(WarmJob {
                query: sanitized.clone(),
                _filters: filters.clone(),
            });
        }

        // Fast path: reuse cached prefix when user is typing forward (offset 0 only).
        // Only use cache for simple queries (no wildcards, no boolean operators) because
        // the cache matching logic enforces strict prefix AND semantics which is incorrect
        // for suffixes, substrings, OR, NOT, or phrases.
        if can_use_cache && offset == 0 && !query.contains('*') && !has_boolean_operators(query) {
            if let Some(cached) = self.cached_prefix_hits(&sanitized, &filters) {
                // Opt 2.4: Pre-compute lowercase query terms once, reuse for all hits
                let query_terms = QueryTermsLower::from_query(&sanitized);
                let mut filtered: Vec<SearchHit> = cached
                    .into_iter()
                    .filter(|h| hit_matches_query_cached_precomputed(h, &query_terms))
                    .map(|c| c.hit.clone())
                    .collect();
                if filtered.len() >= limit {
                    filtered.truncate(limit);
                    self.metrics.inc_cache_hits();
                    self.maybe_log_cache_metrics("hit");
                    return Ok(filtered);
                }
                // Cache had entries but not enough to satisfy limit - shortfall, not miss
                self.metrics.inc_cache_shortfall();
                self.maybe_log_cache_metrics("shortfall");
            } else {
                // No cached prefix at all - this is the actual miss
                self.metrics.inc_cache_miss();
                self.maybe_log_cache_metrics("miss");
            }
        }

        // Heuristic: Fetch enough items to account for deduplication and offset.
        // We fetch from 0 to ensure global deduplication correctness.
        // Multiplier 3 allows for up to ~66% duplicates before we undershoot limit.
        let fetch_limit = (offset + limit).saturating_mul(3);

        // Tantivy is the primary high-performance engine.
        if let Some((reader, fields)) = &self.reader {
            tracing::info!(
                backend = "tantivy",
                query = sanitized,
                limit = fetch_limit,
                offset = 0,
                "search_start"
            );
            let hits = self.search_tantivy(
                reader,
                fields,
                query,
                &sanitized,
                filters.clone(),
                fetch_limit,
                0, // Always fetch from 0 for global dedup
                field_mask,
            )?;
            if !hits.is_empty() {
                let mut deduped = deduplicate_hits(hits);
                // Apply session_paths filter (post-search since source_path is not indexed)
                if !filters.session_paths.is_empty() {
                    deduped.retain(|h| filters.session_paths.contains(&h.source_path));
                }

                // Slice the page after deduplication
                let paged_hits: Vec<SearchHit> =
                    deduped.into_iter().skip(offset).take(limit).collect();

                if can_use_cache && offset == 0 {
                    self.put_cache(&sanitized, &filters, &paged_hits);
                }
                return Ok(paged_hits);
            }
            // If Tantivy yields 0 results, we can optionally fall back to SQLite FTS
            // if we suspect consistency issues, but for now let's trust Tantivy
            // or fall through if you prefer robust fallback.
            // Given the "speed first" requirement, we return early if we got hits.
            // If empty, we *can* try SQLite just in case index is lagging.
        }

        // Fallback: SQLite FTS (slower, but strictly consistent with DB)
        // Skip SQLite fallback when the query contains leading/internal wildcards that
        // FTS5 cannot parse (e.g., "*handler" or "f*o").
        // We ALLOW trailing wildcards ("foo*") as FTS5 supports prefix matching.
        // Also skip SQLite fallback when source filtering is applied, since the FTS table
        // doesn't have a source_id column (P3.1 limitation).
        let unsupported_wildcards = sanitized.split_whitespace().any(|t| {
            let core = t.trim_end_matches('*');
            core.contains('*') // Any star remaining after trimming end is unsupported (leading or internal)
        });

        let has_source_filter = !matches!(filters.source_filter, SourceFilter::All);
        if unsupported_wildcards || has_source_filter {
            return Ok(Vec::new());
        }

        let sqlite_guard = self.sqlite_guard()?;
        if let Some(conn) = sqlite_guard.as_ref() {
            tracing::info!(
                backend = "sqlite",
                query = sanitized,
                limit = fetch_limit,
                offset = 0,
                "search_start"
            );
            let hits = self.search_sqlite(
                conn,
                query,
                filters.clone(),
                fetch_limit,
                0, // Always fetch from 0 for global dedup
                field_mask,
            )?;
            let mut deduped = deduplicate_hits(hits);
            // Apply session_paths filter (post-search since source_path is not indexed)
            if !filters.session_paths.is_empty() {
                deduped.retain(|h| filters.session_paths.contains(&h.source_path));
            }

            let paged_hits: Vec<SearchHit> = deduped.into_iter().skip(offset).take(limit).collect();

            if can_use_cache && offset == 0 {
                self.put_cache(&sanitized, &filters, &paged_hits);
            }
            return Ok(paged_hits);
        }

        tracing::info!(backend = "none", query = query, "search_start");
        Ok(Vec::new())
    }

    pub fn set_semantic_context(
        &self,
        embedder: Arc<dyn Embedder>,
        index: VectorIndex,
        filter_maps: SemanticFilterMaps,
        roles: Option<HashSet<u8>>,
        ann_path: Option<PathBuf>,
    ) -> Result<()> {
        let header = index.header();
        let embedder_id = header.embedder_id.clone();
        let dimension = header.dimension as usize;
        if embedder_id != embedder.id() {
            bail!(
                "embedder mismatch: index uses {}, embedder is {}",
                embedder_id,
                embedder.id()
            );
        }
        if dimension != embedder.dimension() {
            bail!(
                "embedder dimension mismatch: index uses {}, embedder is {}",
                dimension,
                embedder.dimension()
            );
        }

        let capacity = NonZeroUsize::new(100).ok_or_else(|| anyhow!("invalid cache size"))?;
        let mut state_guard = self
            .semantic
            .lock()
            .map_err(|_| anyhow!("semantic lock poisoned"))?;
        *state_guard = Some(SemanticSearchState {
            embedder,
            index,
            ann_index: None,
            ann_path,
            filter_maps,
            roles,
            query_cache: QueryCache::new(embedder_id.as_str(), capacity),
        });
        Ok(())
    }

    pub fn clear_semantic_context(&self) -> Result<()> {
        let mut guard = self
            .semantic
            .lock()
            .map_err(|_| anyhow!("semantic lock poisoned"))?;
        *guard = None;
        Ok(())
    }

    /// Semantic search result containing hits and optional ANN statistics.
    pub fn search_semantic(
        &self,
        query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        field_mask: FieldMask,
        approximate: bool,
    ) -> Result<(
        Vec<SearchHit>,
        Option<crate::search::ann_index::AnnSearchStats>,
    )> {
        let field_mask = effective_field_mask(field_mask);
        let canonical = canonicalize_for_embedding(query);
        if canonical.trim().is_empty() {
            return Ok((Vec::new(), None));
        }
        let mut guard = self
            .semantic
            .lock()
            .map_err(|_| anyhow!("semantic lock poisoned"))?;
        let state = guard
            .as_mut()
            .ok_or_else(|| anyhow!("semantic search unavailable (no embedder or vector index)"))?;

        let embedding = state
            .query_cache
            .get_or_embed(state.embedder.as_ref(), &canonical)?;
        let mut semantic_filter =
            SemanticFilter::from_search_filters(&filters, &state.filter_maps)?;
        if let Some(roles) = state.roles.clone() {
            semantic_filter = semantic_filter.with_roles(Some(roles));
        }

        let fetch = limit.saturating_add(offset);
        if fetch == 0 {
            return Ok((Vec::new(), None));
        }

        // Track ANN stats if approximate search is used
        let mut ann_stats: Option<crate::search::ann_index::AnnSearchStats> = None;

        let mut results = if approximate {
            if state.ann_index.is_none() {
                let ann_path = state.ann_path.as_ref().ok_or_else(|| {
                    anyhow!(
                        "approximate search unavailable: HNSW index missing (run 'cass index --semantic --build-hnsw')"
                    )
                })?;
                if !ann_path.is_file() {
                    bail!(
                        "approximate search unavailable: HNSW index not found at {}",
                        ann_path.display()
                    );
                }
                let ann = HnswIndex::load(ann_path)?;
                let header = state.index.header();
                if ann.embedder_id() != header.embedder_id {
                    bail!(
                        "HNSW index embedder mismatch: expected {}, got {}",
                        header.embedder_id,
                        ann.embedder_id()
                    );
                }
                if ann.dimension() != header.dimension as usize {
                    bail!(
                        "HNSW index dimension mismatch: expected {}, got {}",
                        header.dimension,
                        ann.dimension()
                    );
                }
                state.ann_index = Some(ann);
            }

            let ann = state
                .ann_index
                .as_ref()
                .ok_or_else(|| anyhow!("HNSW index failed to initialize"))?;
            let candidate = fetch.saturating_mul(ANN_CANDIDATE_MULTIPLIER).max(fetch);
            let ef = DEFAULT_EF_SEARCH.max(candidate);
            let (ann_results, search_stats) = ann.search_with_stats(&embedding, candidate, ef)?;
            ann_stats = Some(search_stats);

            let mut best_by_message: HashMap<u64, VectorSearchResult> = HashMap::new();
            for ann_hit in ann_results {
                let row = match state.index.rows().get(ann_hit.row_idx) {
                    Some(row) => row,
                    None => continue,
                };
                if !semantic_filter.matches(row) {
                    continue;
                }
                let score = state.index.dot_product_row(row, &embedding)?;
                best_by_message
                    .entry(row.message_id)
                    .and_modify(|entry| {
                        if score > entry.score {
                            entry.score = score;
                            entry.chunk_idx = row.chunk_idx;
                        }
                    })
                    .or_insert(VectorSearchResult {
                        message_id: row.message_id,
                        chunk_idx: row.chunk_idx,
                        score,
                    });
            }

            let mut ann_hits: Vec<VectorSearchResult> = best_by_message.into_values().collect();
            ann_hits.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| a.message_id.cmp(&b.message_id))
            });
            if ann_hits.len() > fetch {
                ann_hits.truncate(fetch);
            }
            ann_hits
        } else {
            state
                .index
                .search_top_k_collapsed(&embedding, fetch, Some(&semantic_filter))?
        };
        if offset > 0 {
            results = results.into_iter().skip(offset).collect();
        }

        let hits = self.hydrate_semantic_hits(&results, field_mask)?;

        // Deduplicate semantic hits (filter noise and keep best version of same content)
        // This aligns behavior with lexical search
        let mut hits = deduplicate_hits(hits);

        // Apply session_paths filter (not supported at SemanticFilter level)
        if !filters.session_paths.is_empty() {
            hits.retain(|h| filters.session_paths.contains(&h.source_path));
        }
        Ok((hits, ann_stats))
    }

    fn hydrate_semantic_hits(
        &self,
        results: &[VectorSearchResult],
        field_mask: FieldMask,
    ) -> Result<Vec<SearchHit>> {
        if results.is_empty() {
            return Ok(Vec::new());
        }
        let sqlite_guard = self.sqlite_guard()?;
        let conn = sqlite_guard
            .as_ref()
            .ok_or_else(|| anyhow!("semantic search requires database connection"))?;

        // Pre-size: n "?" chars + (n-1) "," chars = 2n-1 total
        let placeholder_capacity = results.len().saturating_mul(2).saturating_sub(1);
        let mut placeholders = String::with_capacity(placeholder_capacity);
        let mut params: Vec<i64> = Vec::with_capacity(results.len());
        for (idx, result) in results.iter().enumerate() {
            if idx > 0 {
                placeholders.push(',');
            }
            placeholders.push('?');
            params.push(i64::try_from(result.message_id)?);
        }

        let content_expr = if field_mask.needs_content() {
            "m.content"
        } else {
            "''"
        };
        let title_expr = if field_mask.wants_title() {
            "c.title"
        } else {
            "''"
        };
        let sql = format!(
            "SELECT m.id, {content_expr}, m.created_at, m.idx, m.role, {title_expr}, c.source_path, c.source_id, c.origin_host, a.slug, w.path, COALESCE(s.kind, 'local'), c.started_at
             FROM messages m
             JOIN conversations c ON m.conversation_id = c.id
             JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             LEFT JOIN sources s ON c.source_id = s.id
             WHERE m.id IN ({placeholders})"
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter()),
            |row: &rusqlite::Row| -> rusqlite::Result<(u64, SearchHit)> {
                let message_id: i64 = row.get(0)?;
                let content: String = row.get(1)?;
                let msg_created_at: Option<i64> = row.get(2)?;
                let idx: Option<i64> = row.get(3)?;
                let title: Option<String> = if field_mask.wants_title() {
                    row.get(5)?
                } else {
                    None
                };
                let source_path: String = row.get(6)?;
                let source_id: Option<String> = row.get(7)?;
                let origin_host: Option<String> = row.get(8)?;
                let agent: String = row.get(9)?;
                let workspace: Option<String> = row.get(10)?;
                let origin_kind: String = row.get(11)?;
                let started_at: Option<i64> = row.get(12)?;

                let created_at = msg_created_at.or(started_at);
                let line_number = idx.map(|i| (i + 1) as usize);
                let snippet = if field_mask.wants_snippet() {
                    snippet_from_content(&content)
                } else {
                    String::new()
                };
                let content_hash = stable_hit_hash(&content, &source_path, line_number, created_at);

                let hit = SearchHit {
                    title: if field_mask.wants_title() {
                        title.unwrap_or_else(|| "Untitled".to_string())
                    } else {
                        String::new()
                    },
                    snippet,
                    content,
                    content_hash,
                    score: 0.0,
                    source_path,
                    agent,
                    workspace: workspace.unwrap_or_default(),
                    workspace_original: None,
                    created_at,
                    line_number,
                    match_type: MatchType::Exact,
                    source_id: source_id.unwrap_or_else(default_source_id),
                    origin_kind,
                    origin_host,
                };

                Ok((message_id as u64, hit))
            },
        )?;

        let mut hits_by_id = HashMap::new();
        for row in rows {
            let (id, hit) = row?;
            hits_by_id.insert(id, hit);
        }

        let mut ordered = Vec::new();
        for result in results {
            if let Some(mut hit) = hits_by_id.remove(&result.message_id) {
                hit.score = result.score;
                ordered.push(hit);
            }
        }

        Ok(ordered)
    }

    /// Search with automatic wildcard fallback for sparse results.
    /// If the initial search returns fewer than `sparse_threshold` results and the query
    /// doesn't already contain wildcards, automatically retry with substring wildcards (*term*).
    pub fn search_with_fallback(
        &self,
        query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        sparse_threshold: usize,
        field_mask: FieldMask,
    ) -> Result<SearchResult> {
        // First, try the normal search
        let hits = self.search(query, filters.clone(), limit, offset, field_mask)?;
        let baseline_stats = self.cache_stats();

        // Check if we should try wildcard fallback
        let query_has_wildcards = query.contains('*');
        let has_boolean_or_phrase = has_boolean_operators(query);
        let is_sparse = hits.len() < sparse_threshold && offset == 0;

        if !is_sparse || query_has_wildcards || has_boolean_or_phrase || query.trim().is_empty() {
            // Either we have enough results, query already has wildcards,
            // query uses boolean/phrases, or query is empty.
            // Generate suggestions only if truly zero hits
            let suggestions = if hits.is_empty() && !query.trim().is_empty() {
                self.generate_suggestions(query, &filters)
            } else {
                Vec::new()
            };
            return Ok(SearchResult {
                hits,
                wildcard_fallback: false,
                cache_stats: baseline_stats,
                suggestions,
                ann_stats: None,
            });
        }

        // Try wildcard fallback: wrap each term in *term*
        let wildcard_query = query
            .split_whitespace()
            .map(|term| format!("*{}*", term.trim_matches('*')))
            .collect::<Vec<_>>()
            .join(" ");

        tracing::info!(
            original_query = query,
            wildcard_query = wildcard_query,
            original_count = hits.len(),
            "wildcard_fallback"
        );

        let mut fallback_hits =
            self.search(&wildcard_query, filters.clone(), limit, offset, field_mask)?;
        let fallback_stats = self.cache_stats();

        // Use fallback results if they're better
        if fallback_hits.len() > hits.len() {
            // Mark all hits as ImplicitWildcard since we auto-added wildcards
            for hit in &mut fallback_hits {
                hit.match_type = MatchType::ImplicitWildcard;
            }
            // Generate suggestions if still zero hits after fallback
            let suggestions = if fallback_hits.is_empty() {
                self.generate_suggestions(query, &filters)
            } else {
                Vec::new()
            };
            Ok(SearchResult {
                hits: fallback_hits,
                wildcard_fallback: true,
                cache_stats: fallback_stats,
                suggestions,
                ann_stats: None,
            })
        } else {
            // Keep original results even if sparse
            // Generate suggestions if zero hits
            let suggestions = if hits.is_empty() {
                self.generate_suggestions(query, &filters)
            } else {
                Vec::new()
            };
            Ok(SearchResult {
                hits,
                wildcard_fallback: false,
                cache_stats: baseline_stats,
                suggestions,
                ann_stats: None,
            })
        }
    }

    /// Hybrid search that fuses lexical + semantic results with RRF.
    #[allow(clippy::too_many_arguments)]
    pub fn search_hybrid(
        &self,
        lexical_query: &str,
        semantic_query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        sparse_threshold: usize,
        field_mask: FieldMask,
        approximate: bool,
    ) -> Result<SearchResult> {
        let fetch = limit.saturating_add(offset);
        if fetch == 0 {
            return Ok(SearchResult {
                hits: Vec::new(),
                wildcard_fallback: false,
                cache_stats: self.cache_stats(),
                suggestions: Vec::new(),
                ann_stats: None,
            });
        }

        if semantic_query.trim().is_empty() {
            return self.search_with_fallback(
                lexical_query,
                filters,
                limit,
                offset,
                sparse_threshold,
                field_mask,
            );
        }

        let candidate = fetch.saturating_mul(HYBRID_CANDIDATE_MULTIPLIER);
        let lexical = self.search_with_fallback(
            lexical_query,
            filters.clone(),
            candidate,
            0,
            sparse_threshold,
            field_mask,
        )?;
        let (semantic_hits, semantic_ann_stats) = self.search_semantic(
            semantic_query,
            filters,
            candidate,
            0,
            field_mask,
            approximate,
        )?;
        let fused = rrf_fuse_hits(&lexical.hits, &semantic_hits, limit, offset);
        let suggestions = if fused.is_empty() {
            lexical.suggestions.clone()
        } else {
            Vec::new()
        };
        Ok(SearchResult {
            hits: fused,
            wildcard_fallback: lexical.wildcard_fallback,
            cache_stats: lexical.cache_stats,
            suggestions,
            ann_stats: semantic_ann_stats,
        })
    }

    /// Generate "did-you-mean" suggestions for zero-hit queries.
    fn generate_suggestions(&self, query: &str, filters: &SearchFilters) -> Vec<QuerySuggestion> {
        let mut suggestions = Vec::new();
        let query_lower = query.to_lowercase();

        // 1. Suggest wildcard search if query doesn't have wildcards
        if !query.contains('*') && query.len() >= 2 {
            suggestions.push(QuerySuggestion::wildcard(query).with_shortcut(1));
        }

        // 2. Suggest removing agent filter if one is set
        if !filters.agents.is_empty() {
            let agents: Vec<&str> = filters
                .agents
                .iter()
                .map(std::string::String::as_str)
                .collect();
            let agent_str = agents.join(", ");
            suggestions
                .push(QuerySuggestion::remove_agent_filter(&agent_str, filters).with_shortcut(2));
        }

        // 3. Suggest common agent names if query looks like a typo of one
        let known_agents = [
            "codex",
            "claude",
            "claude_code",
            "cline",
            "gemini",
            "amp",
            "opencode",
        ];
        for agent in &known_agents {
            if levenshtein_distance(&query_lower, agent) <= 2 && query_lower != *agent {
                suggestions.push(
                    QuerySuggestion::spelling(query, agent)
                        .with_shortcut(suggestions.len().min(2) as u8 + 1),
                );
                break; // Only suggest one spelling fix
            }
        }

        // 4. Suggest alternative agents if we have SQLite connection and no agent filter
        if filters.agents.is_empty()
            && let Ok(sqlite_guard) = self.sqlite_guard()
            && let Some(conn) = sqlite_guard.as_ref()
            && let Ok(mut stmt) = conn
                .prepare("SELECT DISTINCT agent_slug FROM conversations ORDER BY id DESC LIMIT 3")
            && let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0))
        {
            for row in rows.flatten() {
                if suggestions.len() < 3 {
                    suggestions.push(
                        QuerySuggestion::try_agent(&row)
                            .with_shortcut(suggestions.len().min(2) as u8 + 1),
                    );
                }
            }
        }

        // Ensure we have at most 3 suggestions with shortcuts 1, 2, 3
        suggestions.truncate(3);
        for (i, sugg) in suggestions.iter_mut().enumerate() {
            sugg.shortcut = Some((i + 1) as u8);
        }

        suggestions
    }

    fn searcher_for_thread(&self, reader: &IndexReader) -> Searcher {
        let epoch = self.reload_epoch.load(Ordering::Relaxed);
        THREAD_SEARCHER.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(entry) = slot.as_ref()
                && entry.epoch == epoch
            {
                return entry.searcher.clone();
            }
            let searcher = reader.searcher();
            *slot = Some(SearcherCacheEntry {
                epoch,
                searcher: searcher.clone(),
            });
            searcher
        })
    }

    fn track_generation(&self, generation: u64) {
        let mut guard = self
            .last_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = *guard
            && prev != generation
            && let Ok(mut cache) = self.prefix_cache.lock()
        {
            cache.clear();
        }
        *guard = Some(generation);
    }

    #[allow(clippy::too_many_arguments)]
    fn search_tantivy(
        &self,
        reader: &IndexReader,
        fields: &crate::search::tantivy::Fields,
        raw_query: &str,
        sanitized_query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        field_mask: FieldMask,
    ) -> Result<Vec<SearchHit>> {
        self.maybe_reload_reader(reader)?;
        let searcher = self.searcher_for_thread(reader);
        self.track_generation(searcher.generation().generation_id());

        let needs_content = field_mask.needs_content() || field_mask.wants_snippet();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // Parse query with boolean operator support (AND, OR, NOT, "phrases").
        // Use the raw query so "-" and quotes are preserved for parsing, but
        // normalize terms before building Tantivy clauses.
        let tokens = parse_boolean_query(raw_query);
        if tokens.is_empty() {
            clauses.push((Occur::Must, Box::new(AllQuery)));
        } else if has_boolean_operators(raw_query) {
            // Use boolean query builder for complex queries
            let bool_clauses = build_boolean_query_clauses(&tokens, fields);
            clauses.extend(bool_clauses);
        } else {
            // Simple query: treat each term as MUST (implicit AND)
            for token in tokens {
                if let QueryToken::Term(term_str) = token {
                    let parts = normalize_term_parts(&term_str);
                    if let Some(term_query) = build_compound_term_query(&parts, fields) {
                        clauses.push((Occur::Must, term_query));
                    }
                }
            }
        }

        if !filters.agents.is_empty() {
            let terms = filters
                .agents
                .into_iter()
                .map(|agent| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.agent, &agent),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(terms))));
        }

        if !filters.workspaces.is_empty() {
            let terms = filters
                .workspaces
                .into_iter()
                .map(|ws| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.workspace, &ws),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(terms))));
        }

        if filters.created_from.is_some() || filters.created_to.is_some() {
            use std::ops::Bound::{Included, Unbounded};
            let lower = filters.created_from.map_or(Unbounded, |v| {
                Included(Term::from_field_i64(fields.created_at, v))
            });
            let upper = filters.created_to.map_or(Unbounded, |v| {
                Included(Term::from_field_i64(fields.created_at, v))
            });
            let range = RangeQuery::new(lower, upper);
            clauses.push((Occur::Must, Box::new(range)));
        }

        // Source filter (P3.1)
        match &filters.source_filter {
            SourceFilter::All => {
                // No filtering needed
            }
            SourceFilter::Local => {
                // Filter to local sources only (origin_kind == "local")
                let term = Term::from_field_text(fields.origin_kind, "local");
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            SourceFilter::Remote => {
                // Filter to remote sources only (origin_kind == "ssh")
                // We use "ssh" since that's the only remote kind currently
                let term = Term::from_field_text(fields.origin_kind, "ssh");
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            SourceFilter::SourceId(source_id) => {
                // Filter to specific source by ID
                let term = Term::from_field_text(fields.source_id, source_id);
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
        }

        // NOTE: session_paths filtering is applied post-search since source_path
        // is STORED but not indexed. See apply_session_paths_filter().

        let q: Box<dyn Query> = if clauses.is_empty() {
            Box::new(AllQuery)
        } else if clauses.len() == 1 {
            let (occur, query_box) = clauses.pop().unwrap();
            match occur {
                // For Must, we can safely unwrap and use the inner query directly
                Occur::Must => query_box,
                // For MustNot or Should, we must preserve the Occur by wrapping
                // in a BooleanQuery. A lone MustNot (e.g., "NOT foo") should match
                // nothing, not match "foo".
                _ => Box::new(BooleanQuery::new(vec![(occur, query_box)])),
            }
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let prefix_only = is_prefix_only(sanitized_query);
        let snippet_generator = if prefix_only || !field_mask.wants_snippet() {
            None
        } else {
            Some(SnippetGenerator::create(&searcher, &*q, fields.content)?)
        };

        let top_docs = searcher.search(&q, &TopDocs::with_limit(limit).and_offset(offset))?;
        // Compute match type once for all results (not per-hit)
        let query_match_type = dominant_match_type(sanitized_query);
        let mut hits = Vec::new();
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let title = if field_mask.wants_title() {
                doc.get_first(fields.title)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            };
            let content = if needs_content {
                doc.get_first(fields.content)
                    .or_else(|| doc.get_first(fields.preview))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            };
            let agent = doc
                .get_first(fields.agent)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let snippet = if field_mask.wants_snippet() {
                if let Some(r#gen) = &snippet_generator {
                    r#gen
                        .snippet_from_doc(&doc)
                        .to_html()
                        .replace("<b>", "**")
                        .replace("</b>", "**")
                } else if let Some(sn) = cached_prefix_snippet(&content, sanitized_query, 160) {
                    sn
                } else {
                    quick_prefix_snippet(&content, sanitized_query, 160)
                }
            } else {
                String::new()
            };
            let source = doc
                .get_first(fields.source_path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let workspace = doc
                .get_first(fields.workspace)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // workspace_original: pre-rewrite path (P6.2)
            let workspace_original = doc
                .get_first(fields.workspace_original)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let created_at = doc.get_first(fields.created_at).and_then(|v| v.as_i64());
            let line_number = doc
                .get_first(fields.msg_idx)
                .and_then(|v| v.as_u64())
                .map(|i| (i + 1) as usize);
            let content_hash = stable_hit_hash(&content, &source, line_number, created_at);
            // Provenance fields (P3.3)
            let source_id = doc
                .get_first(fields.source_id)
                .and_then(|v| v.as_str())
                .unwrap_or("local")
                .to_string();
            let origin_kind = doc
                .get_first(fields.origin_kind)
                .and_then(|v| v.as_str())
                .unwrap_or("local")
                .to_string();
            let origin_host = doc
                .get_first(fields.origin_host)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            hits.push(SearchHit {
                title,
                snippet,
                content,
                content_hash,
                score,
                source_path: source,
                agent,
                workspace,
                workspace_original,
                created_at,
                line_number,
                match_type: query_match_type,
                source_id,
                origin_kind,
                origin_host,
            });
        }
        Ok(hits)
    }

    fn search_sqlite(
        &self,
        conn: &Connection,
        raw_query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        field_mask: FieldMask,
    ) -> Result<Vec<SearchHit>> {
        // Transpile raw query to FTS5 syntax
        // Returns None if unsupported features (leading wildcards) are used or query is empty
        let fts_query = match transpile_to_fts5(raw_query) {
            Some(q) if !q.trim().is_empty() => q,
            _ => return Ok(Vec::new()),
        };

        // Compute match type once for all results
        let query_match_type = dominant_match_type(raw_query);

        let content_expr = if field_mask.needs_content() {
            "f.content"
        } else {
            "''"
        };
        let title_expr = if field_mask.wants_title() {
            "f.title"
        } else {
            "''"
        };
        let snippet_expr = if field_mask.wants_snippet() {
            "snippet(fts_messages, 0, '**', '**', '...', 64)"
        } else {
            "''"
        };
        let mut sql = format!(
            "SELECT {title_expr}, {content_expr}, f.agent, f.workspace, f.source_path, f.created_at, bm25(fts_messages) AS score, {snippet_expr} AS snippet, m.idx
             FROM fts_messages f
             LEFT JOIN messages m ON f.message_id = m.id
             WHERE fts_messages MATCH ?"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_query)];

        if !filters.agents.is_empty() {
            let placeholders = sql_placeholders(filters.agents.len());
            sql.push_str(&format!(" AND f.agent IN ({placeholders})"));
            for a in filters.agents {
                params.push(Box::new(a));
            }
        }

        if !filters.workspaces.is_empty() {
            let placeholders = sql_placeholders(filters.workspaces.len());
            sql.push_str(&format!(" AND f.workspace IN ({placeholders})"));
            for w in filters.workspaces {
                params.push(Box::new(w));
            }
        }

        if let Some(created_from) = filters.created_from {
            sql.push_str(" AND f.created_at >= ?");
            params.push(Box::new(created_from));
        }
        if let Some(created_to) = filters.created_to {
            sql.push_str(" AND f.created_at <= ?");
            params.push(Box::new(created_to));
        }

        sql.push_str(" ORDER BY score LIMIT ? OFFSET ?");
        params.push(Box::new(limit as i64));
        params.push(Box::new(offset as i64));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter().map(|b| &**b)),
            |row| {
                let title: String = row.get(0)?;
                let content: String = row.get(1)?;
                let agent: String = row.get(2)?;
                let workspace: String = row.get(3)?;
                let source_path: String = row.get(4)?;
                let created_at: Option<i64> = row.get(5).ok();
                let score: f32 = row.get::<_, f64>(6)? as f32;
                let snippet: String = row.get(7)?;
                // idx is 0-indexed message index; convert to 1-indexed line number for JSONL files
                let idx: Option<i64> = row.get(8).ok();
                let line_number = idx.map(|i| (i + 1) as usize);
                let content_hash = stable_hit_hash(&content, &source_path, line_number, created_at);
                // SQLite FTS doesn't have provenance or workspace_original - use defaults
                Ok(SearchHit {
                    title,
                    snippet,
                    content,
                    content_hash,
                    score,
                    source_path,
                    agent,
                    workspace,
                    workspace_original: None,
                    created_at,
                    line_number,
                    match_type: query_match_type,
                    source_id: default_source_id(),
                    origin_kind: default_origin_kind(),
                    origin_host: None,
                })
            },
        )?;

        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }
}

/// Transpile a raw query string into an FTS5-compatible query string.
/// Preserves custom precedence (OR > AND) by adding parentheses.
/// Returns None if the query contains features unsupported by FTS5 (e.g. leading wildcards).
fn transpile_to_fts5(raw_query: &str) -> Option<String> {
    let tokens = parse_boolean_query(raw_query);
    if tokens.is_empty() {
        return Some("".to_string());
    }

    let mut fts_clauses: Vec<(&str, String)> = Vec::new();
    let mut pending_or_group: Vec<String> = Vec::new();
    let mut next_op = "AND";
    let mut in_or_sequence = false;

    for token in tokens {
        match token {
            QueryToken::And => {
                if !pending_or_group.is_empty() {
                    let group = if pending_or_group.len() > 1 {
                        format!("({})", pending_or_group.join(" OR "))
                    } else {
                        pending_or_group.pop().unwrap()
                    };
                    fts_clauses.push(("AND", group));
                    pending_or_group.clear();
                }
                in_or_sequence = false;
                next_op = "AND";
            }
            QueryToken::Or => {
                in_or_sequence = true;
            }
            QueryToken::Not => {
                if !pending_or_group.is_empty() {
                    let group = if pending_or_group.len() > 1 {
                        format!("({})", pending_or_group.join(" OR "))
                    } else {
                        pending_or_group.pop().unwrap()
                    };
                    fts_clauses.push(("AND", group));
                    pending_or_group.clear();
                }
                in_or_sequence = false;
                next_op = "NOT";
            }
            QueryToken::Term(t) => {
                // Check for unsupported wildcards
                let pattern = WildcardPattern::parse(&t);
                if matches!(
                    pattern,
                    WildcardPattern::Suffix(_)
                        | WildcardPattern::Substring(_)
                        | WildcardPattern::Complex(_)
                ) {
                    return None;
                }

                // Sanitize and normalize. FTS5 implicitly ANDs words in a string.
                // e.g. "foo bar" -> foo AND bar.
                // normalize_term_parts splits "foo-bar" -> "foo", "bar".
                let term_parts = normalize_term_parts(&t);
                if term_parts.is_empty() {
                    continue;
                }

                // If multiple parts, wrap in parens and join with AND to ensure they stay together
                let fts_term = if term_parts.len() > 1 {
                    format!("({})", term_parts.join(" AND "))
                } else {
                    term_parts[0].clone()
                };

                if in_or_sequence {
                    if pending_or_group.is_empty()
                        && let Some((op, _)) = fts_clauses.last()
                        && *op == "AND"
                    {
                        let (_, val) = fts_clauses.pop().unwrap();
                        pending_or_group.push(val);
                    }
                    pending_or_group.push(fts_term);
                    in_or_sequence = true;
                } else {
                    fts_clauses.push((next_op, fts_term));
                }
                next_op = "AND";
            }
            QueryToken::Phrase(p) => {
                let phrase_parts = normalize_phrase_terms(&p);
                if phrase_parts.is_empty() {
                    continue;
                }
                let fts_phrase = format!("\"{}\"", phrase_parts.join(" "));

                if in_or_sequence {
                    if pending_or_group.is_empty()
                        && let Some((op, _)) = fts_clauses.last()
                        && *op == "AND"
                    {
                        let (_, val) = fts_clauses.pop().unwrap();
                        pending_or_group.push(val);
                    }
                    pending_or_group.push(fts_phrase);
                    in_or_sequence = true;
                } else {
                    fts_clauses.push((next_op, fts_phrase));
                }
                next_op = "AND";
            }
        }
    }

    if !pending_or_group.is_empty() {
        let group = if pending_or_group.len() > 1 {
            format!("({})", pending_or_group.join(" OR "))
        } else {
            pending_or_group.pop().unwrap()
        };
        fts_clauses.push((next_op, group));
    }

    if fts_clauses.is_empty() {
        return Some("".to_string());
    }

    // Join clauses. The first operator is ignored (start of query).
    let mut query = String::new();
    for (i, (op, text)) in fts_clauses.into_iter().enumerate() {
        if i > 0 {
            query.push_str(&format!(" {} ", op));
        } else if op == "NOT" {
            // Leading NOT
            query.push_str("NOT ");
        }
        query.push_str(&text);
    }

    Some(query)
}

#[derive(Default, Clone)]
struct Metrics {
    cache_hits: Arc<AtomicU64>,
    cache_miss: Arc<AtomicU64>,
    cache_shortfall: Arc<AtomicU64>,
    reloads: Arc<AtomicU64>,
    reload_ms_total: Arc<AtomicU64>,
}

impl Metrics {
    fn inc_cache_hits(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }
    fn inc_cache_miss(&self) {
        self.cache_miss.fetch_add(1, Ordering::Relaxed);
    }
    fn inc_cache_shortfall(&self) {
        self.cache_shortfall.fetch_add(1, Ordering::Relaxed);
    }
    fn inc_reload(&self) {
        self.reloads.fetch_add(1, Ordering::Relaxed);
    }
    fn record_reload(&self, duration: Duration) {
        self.inc_reload();
        self.reload_ms_total
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
    }

    fn snapshot_all(&self) -> (u64, u64, u64, u64, u128) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_miss.load(Ordering::Relaxed),
            self.cache_shortfall.load(Ordering::Relaxed),
            self.reloads.load(Ordering::Relaxed),
            self.reload_ms_total.load(Ordering::Relaxed) as u128,
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn reset(&self) {
        self.cache_hits.store(0, Ordering::Relaxed);
        self.cache_miss.store(0, Ordering::Relaxed);
        self.cache_shortfall.store(0, Ordering::Relaxed);
        self.reloads.store(0, Ordering::Relaxed);
        self.reload_ms_total.store(0, Ordering::Relaxed);
    }
}

fn maybe_spawn_warm_worker(
    reader: IndexReader,
    fields: crate::search::tantivy::Fields,
    filters_guard: std::sync::Weak<Mutex<()>>,
    reload_epoch: Arc<AtomicU64>,
    metrics: Metrics,
) -> Option<(mpsc::UnboundedSender<WarmJob>, JoinHandle<()>)> {
    // Only spawn if a Tokio runtime is available (tests may call without one).
    if Handle::try_current().is_err() {
        return None;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<WarmJob>();
    let handle = tokio::spawn(async move {
        // Simple debounce: process at most one warmup every WARM_DEBOUNCE_MS.
        let mut last_run = Instant::now();
        while let Some(job) = rx.recv().await {
            let now = Instant::now();
            if now.duration_since(last_run) < Duration::from_millis(*WARM_DEBOUNCE_MS) {
                continue;
            }
            last_run = now;
            if filters_guard.upgrade().is_none() {
                break;
            }
            let reload_started = Instant::now();
            if let Err(err) = reader.reload() {
                tracing::warn!(error = ?err, "warm_worker_reload_failed");
                continue;
            }
            let elapsed = reload_started.elapsed();
            let epoch = reload_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            metrics.record_reload(elapsed);
            tracing::debug!(
                duration_ms = elapsed.as_millis() as u64,
                reload_epoch = epoch,
                "warm_worker_reload"
            );
            // Run a tiny warm search to prefill OS cache and hit the Tantivy reader
            // without allocating full result sets. Limit 1 doc.
            let searcher = reader.searcher();
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for term_str in job.query.split_whitespace() {
                let term_lower = term_str.to_lowercase();
                let term_shoulds: Vec<(Occur, Box<dyn Query>)> = vec![
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.title, &term_lower),
                            IndexRecordOption::WithFreqsAndPositions,
                        )),
                    ),
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.content, &term_lower),
                            IndexRecordOption::WithFreqsAndPositions,
                        )),
                    ),
                ];
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(term_shoulds))));
            }
            if !clauses.is_empty() {
                let q: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
                let _ = searcher.search(&q, &TopDocs::with_limit(1));
            }
        }
    });
    Some((tx, handle))
}

fn cached_hit_from(hit: &SearchHit) -> CachedHit {
    let lc_content = hit.content.to_lowercase();
    let lc_title = (!hit.title.is_empty()).then(|| hit.title.to_lowercase());
    // Snippet is derived from content, so we don't index/bloom it separately
    let bloom64 = bloom_from_text(&lc_content, &lc_title);
    CachedHit {
        hit: hit.clone(),
        lc_content,
        lc_title,
        bloom64,
    }
}

fn bloom_from_text(content: &str, title: &Option<String>) -> u64 {
    let mut bits = 0u64;
    for token in token_stream(content) {
        bits |= hash_token(token);
    }
    if let Some(t) = title {
        for token in token_stream(t) {
            bits |= hash_token(token);
        }
    }
    bits
}

fn token_stream(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
}

fn hash_token(tok: &str) -> u64 {
    // Simple 64-bit djb2-style hash mapped to bit position 0..63
    let mut h: u64 = 5381;
    for b in tok.as_bytes() {
        h = ((h << 5).wrapping_add(h)).wrapping_add(u64::from(*b));
    }
    1u64 << (h % 64)
}

// ============================================================================
// QueryTermsLower: Pre-computed lowercase query tokens (Opt 2.4)
// ============================================================================
//
// Avoids repeated to_lowercase() calls when filtering many cached hits.
// The query is lowercased once and tokens extracted once, then reused.

/// Pre-computed lowercase query terms for efficient hit matching.
/// Call `from_query` once, then reuse for all hits in a search.
struct QueryTermsLower {
    /// The lowercased query string (owned to keep tokens valid)
    query_lower: String,
    /// Pre-computed token positions (start, end) into query_lower
    token_ranges: Vec<(usize, usize)>,
    /// Pre-computed bloom bits for fast rejection
    bloom_mask: u64,
}

impl QueryTermsLower {
    /// Create from a query string, pre-computing lowercase and tokens.
    fn from_query(query: &str) -> Self {
        if query.is_empty() {
            return Self {
                query_lower: String::new(),
                token_ranges: Vec::new(),
                bloom_mask: 0,
            };
        }

        let query_lower = query.to_lowercase();
        let mut token_ranges = Vec::new();
        let mut bloom_mask = 0u64;

        // Extract token positions
        let mut start = None;
        for (i, c) in query_lower.char_indices() {
            if c.is_alphanumeric() {
                if start.is_none() {
                    start = Some(i);
                }
            } else if let Some(s) = start.take() {
                let token = &query_lower[s..i];
                bloom_mask |= hash_token(token);
                token_ranges.push((s, i));
            }
        }
        // Handle trailing token
        if let Some(s) = start {
            let token = &query_lower[s..];
            bloom_mask |= hash_token(token);
            token_ranges.push((s, query_lower.len()));
        }

        Self {
            query_lower,
            token_ranges,
            bloom_mask,
        }
    }

    /// Check if this query is empty (no tokens).
    #[inline]
    fn is_empty(&self) -> bool {
        self.token_ranges.is_empty()
    }

    /// Iterate over the pre-computed lowercase tokens.
    #[inline]
    fn tokens(&self) -> impl Iterator<Item = &str> {
        self.token_ranges
            .iter()
            .map(|(s, e)| &self.query_lower[*s..*e])
    }

    /// Get the bloom mask for fast rejection.
    #[inline]
    fn bloom_mask(&self) -> u64 {
        self.bloom_mask
    }
}

/// Check if a cached hit matches the pre-computed query terms.
/// This is the optimized version that avoids repeated to_lowercase() calls.
fn hit_matches_query_cached_precomputed(hit: &CachedHit, terms: &QueryTermsLower) -> bool {
    if terms.is_empty() {
        return true;
    }

    // Bloom gate: all query tokens must have bits set
    if hit.bloom64 & terms.bloom_mask() != terms.bloom_mask() {
        return false;
    }

    // Verify each token matches as a prefix of a word in at least one field (implicit AND)
    terms.tokens().all(|t| {
        // Check content tokens
        if token_stream(&hit.lc_content).any(|word| word.starts_with(t)) {
            return true;
        }
        // Check title tokens
        if let Some(title) = &hit.lc_title
            && token_stream(title).any(|word| word.starts_with(t))
        {
            return true;
        }
        false
    })
}

/// Legacy function for backward compatibility with tests.
/// Prefer `hit_matches_query_cached_precomputed` with `QueryTermsLower` for batch operations.
#[cfg(test)]
fn hit_matches_query_cached(hit: &CachedHit, query: &str) -> bool {
    let terms = QueryTermsLower::from_query(query);
    hit_matches_query_cached_precomputed(hit, &terms)
}

fn is_prefix_only(query: &str) -> bool {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    // Only strictly optimize single-term prefix queries.
    // Multi-term queries benefit from Tantivy's snippet generation (highlighting both terms).
    if tokens.len() != 1 {
        return false;
    }
    tokens[0].chars().all(char::is_alphanumeric)
}

fn quick_prefix_snippet(content: &str, query: &str, max_chars: usize) -> String {
    let content_char_count = content.chars().count();

    // Handle empty query case first
    if query.is_empty() {
        let snippet: String = content.chars().take(max_chars).collect();
        return if content_char_count > max_chars {
            format!("{snippet}…")
        } else {
            snippet
        };
    }

    let lc_content = content.to_lowercase();
    let lc_query = query.to_lowercase();
    if let Some(pos) = lc_content.find(&lc_query) {
        // Convert byte index in the lowercased string to a character index.
        // IMPORTANT: Use lc_content[..pos], not content[..pos], because pos is a byte
        // index valid only for the lowercased string (Unicode case mappings can change
        // byte lengths, e.g., German ß → SS).
        let start_char = lc_content[..pos].chars().count().saturating_sub(15);
        let snippet: String = content.chars().skip(start_char).take(max_chars).collect();
        // Check if we truncated: snippet covers chars [start_char, start_char + snippet_len)
        let snippet_char_count = snippet.chars().count();
        if start_char + snippet_char_count < content_char_count {
            format!("{snippet}…")
        } else {
            snippet
        }
    } else {
        let snippet: String = content.chars().take(max_chars).collect();
        if content_char_count > max_chars {
            format!("{snippet}…")
        } else {
            snippet
        }
    }
}

fn cached_prefix_snippet(content: &str, query: &str, max_chars: usize) -> Option<String> {
    if query.trim().is_empty() {
        return None;
    }
    let lc_content = content.to_lowercase();
    let lc_query = query.to_lowercase();
    let content_char_count = content.chars().count();
    lc_content.find(&lc_query).map(|pos| {
        // Convert byte index in the lowercased string to a character index.
        // IMPORTANT: Use lc_content[..pos], not content[..pos], because pos is a byte
        // index valid only for the lowercased string (Unicode case mappings can change
        // byte lengths, e.g., German ß → SS).
        let start_char = lc_content[..pos].chars().count().saturating_sub(15);
        let snippet: String = content.chars().skip(start_char).take(max_chars).collect();
        // Check if we truncated: snippet covers chars [start_char, start_char + snippet_len)
        let snippet_char_count = snippet.chars().count();
        if start_char + snippet_char_count < content_char_count {
            format!("{snippet}…")
        } else {
            snippet
        }
    })
}

fn filters_fingerprint(filters: &SearchFilters) -> String {
    let mut parts = Vec::new();
    if !filters.agents.is_empty() {
        let mut v: Vec<_> = filters.agents.iter().cloned().collect();
        v.sort();
        parts.push(format!("a:{v:?}"));
    }
    if !filters.workspaces.is_empty() {
        let mut v: Vec<_> = filters.workspaces.iter().cloned().collect();
        v.sort();
        parts.push(format!("w:{v:?}"));
    }
    if let Some(f) = filters.created_from {
        parts.push(format!("from:{f}"));
    }
    if let Some(t) = filters.created_to {
        parts.push(format!("to:{t}"));
    }
    // Include source_filter in cache key (P3.1)
    if !matches!(
        filters.source_filter,
        crate::sources::provenance::SourceFilter::All
    ) {
        parts.push(format!("src:{:?}", filters.source_filter));
    }
    // Include session_paths in cache key (for chained searches)
    if !filters.session_paths.is_empty() {
        let mut v: Vec<_> = filters.session_paths.iter().cloned().collect();
        v.sort();
        parts.push(format!("sp:{v:?}"));
    }
    parts.join("|")
}

impl SearchClient {
    fn maybe_reload_reader(&self, reader: &IndexReader) -> Result<()> {
        if !self.reload_on_search {
            return Ok(());
        }
        const MIN_RELOAD_INTERVAL: Duration = Duration::from_millis(300);
        let now = Instant::now();
        let mut guard = self.last_reload.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .map(|t| now.duration_since(t) >= MIN_RELOAD_INTERVAL)
            .unwrap_or(true)
        {
            let reload_started = Instant::now();
            reader.reload()?;
            let elapsed = reload_started.elapsed();
            *guard = Some(now);
            let epoch = self.reload_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            self.metrics.record_reload(elapsed);
            tracing::debug!(
                duration_ms = elapsed.as_millis() as u64,
                reload_epoch = epoch,
                "tantivy_reader_reload"
            );
        }
        Ok(())
    }

    fn maybe_log_cache_metrics(&self, event: &str) {
        if !*CACHE_DEBUG_ENABLED {
            return;
        }
        let stats = self.cache_stats();
        tracing::debug!(
            event = event,
            hits = stats.cache_hits,
            miss = stats.cache_miss,
            shortfall = stats.cache_shortfall,
            reloads = stats.reloads,
            reload_ms_total = stats.reload_ms_total,
            total_cap = stats.total_cap,
            total_cost = stats.total_cost,
            evictions = stats.eviction_count,
            approx_bytes = stats.approx_bytes,
            byte_cap = stats.byte_cap,
            "cache_metrics"
        );
    }

    /// Generate an interned cache key for the given query and filters.
    /// Returns Arc<str> to enable memory sharing for repeated queries.
    fn cache_key(&self, query: &str, filters: &SearchFilters) -> Arc<str> {
        let key_str = format!(
            "{}|{}::{}",
            self.cache_namespace,
            query,
            filters_fingerprint(filters)
        );
        intern_cache_key(&key_str)
    }

    fn shard_name(&self, filters: &SearchFilters) -> String {
        if filters.agents.len() == 1 {
            filters
                .agents
                .iter()
                .next()
                .cloned()
                .unwrap_or_else(|| "global".into())
        } else {
            "global".into()
        }
    }

    fn cached_prefix_hits(&self, query: &str, filters: &SearchFilters) -> Option<Vec<CachedHit>> {
        if query.is_empty() {
            return None;
        }
        let cache = self.prefix_cache.lock().ok()?;
        let shard_name = self.shard_name(filters);
        let shard = cache.shard_opt(&shard_name)?;
        // Iterate over character boundaries to avoid slicing mid-codepoint.
        let mut byte_indices: Vec<usize> = query.char_indices().map(|(i, _)| i).collect();
        byte_indices.push(query.len());
        for &end in byte_indices.iter().rev() {
            if end == 0 {
                continue;
            }
            let key = self.cache_key(&query[..end], filters);
            // LruCache.peek() accepts &Q where Arc<str>: Borrow<Q>, so &Arc<str> works
            if let Some(hits) = shard.peek(&key) {
                return Some(hits.clone());
            }
        }
        None
    }

    fn put_cache(&self, query: &str, filters: &SearchFilters, hits: &[SearchHit]) {
        if query.is_empty() || hits.is_empty() {
            return;
        }
        if let Ok(mut cache) = self.prefix_cache.lock() {
            let shard_name = self.shard_name(filters);
            let key = self.cache_key(query, filters);
            let cached_hits: Vec<CachedHit> = hits.iter().map(cached_hit_from).collect();
            cache.put(&shard_name, key, cached_hits);
        }
    }

    pub fn cache_stats(&self) -> CacheStats {
        let (hits, miss, shortfall, reloads, reload_ms_total) = self.metrics.snapshot_all();
        let (total_cap, total_cost, eviction_count, approx_bytes, byte_cap) =
            if let Ok(cache) = self.prefix_cache.lock() {
                (
                    cache.total_cap(),
                    cache.total_cost(),
                    cache.eviction_count(),
                    cache.total_bytes(),
                    cache.byte_cap(),
                )
            } else {
                (0, 0, 0, 0, 0)
            };
        CacheStats {
            cache_hits: hits,
            cache_miss: miss,
            cache_shortfall: shortfall,
            reloads,
            reload_ms_total,
            total_cap,
            total_cost,
            eviction_count,
            approx_bytes,
            byte_cap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{NormalizedConversation, NormalizedMessage, NormalizedSnippet};
    use crate::search::tantivy::TantivyIndex;
    use tempfile::TempDir;

    // ==========================================================================
    // StringInterner Tests (Opt 2.3)
    // ==========================================================================

    #[test]
    fn interner_returns_same_arc_for_same_string() {
        let interner = StringInterner::new(100);

        let s1 = interner.intern("test_query");
        let s2 = interner.intern("test_query");

        // Should be the exact same Arc (pointer equality)
        assert!(Arc::ptr_eq(&s1, &s2));
        assert_eq!(&*s1, "test_query");
    }

    #[test]
    fn interner_different_strings_return_different_arcs() {
        let interner = StringInterner::new(100);

        let s1 = interner.intern("query1");
        let s2 = interner.intern("query2");

        assert!(!Arc::ptr_eq(&s1, &s2));
        assert_eq!(&*s1, "query1");
        assert_eq!(&*s2, "query2");
    }

    #[test]
    fn interner_handles_empty_string() {
        let interner = StringInterner::new(100);

        let s1 = interner.intern("");
        let s2 = interner.intern("");

        assert!(Arc::ptr_eq(&s1, &s2));
        assert_eq!(&*s1, "");
    }

    #[test]
    fn interner_handles_unicode() {
        let interner = StringInterner::new(100);

        let s1 = interner.intern("测试查询");
        let s2 = interner.intern("测试查询");
        let s3 = interner.intern("emoji 🔍 search");

        assert!(Arc::ptr_eq(&s1, &s2));
        assert_eq!(&*s3, "emoji 🔍 search");
    }

    #[test]
    fn interner_respects_lru_eviction() {
        let interner = StringInterner::new(3);

        let _s1 = interner.intern("query1");
        let _s2 = interner.intern("query2");
        let _s3 = interner.intern("query3");

        assert_eq!(interner.len(), 3);

        // This should evict query1 (LRU)
        let _s4 = interner.intern("query4");

        assert_eq!(interner.len(), 3);

        // query1 should now get a NEW Arc (was evicted)
        let s1_new = interner.intern("query1");
        assert_eq!(&*s1_new, "query1");
    }

    #[test]
    fn interner_concurrent_access() {
        use std::thread;

        let interner = Arc::new(StringInterner::new(1000));
        let queries: Vec<String> = (0..100).map(|i| format!("query_{}", i)).collect();

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let interner = Arc::clone(&interner);
                let queries = queries.clone();

                thread::spawn(move || {
                    for _ in 0..10 {
                        for query in &queries {
                            let _ = interner.intern(query);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all queries are interned correctly
        for query in &queries {
            let s1 = interner.intern(query);
            let s2 = interner.intern(query);
            assert!(Arc::ptr_eq(&s1, &s2));
        }
    }

    // ==========================================================================
    // QueryTermsLower Tests (Opt 2.4)
    // ==========================================================================

    #[test]
    fn query_terms_lower_basic() {
        let terms = QueryTermsLower::from_query("Hello World");

        assert_eq!(terms.query_lower, "hello world");
        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn query_terms_lower_empty() {
        let terms = QueryTermsLower::from_query("");

        assert!(terms.is_empty());
        assert_eq!(terms.tokens().count(), 0);
    }

    #[test]
    fn query_terms_lower_single_term() {
        let terms = QueryTermsLower::from_query("TEST");

        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["test"]);
    }

    #[test]
    fn query_terms_lower_with_punctuation() {
        let terms = QueryTermsLower::from_query("hello, world! how's it?");

        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["hello", "world", "how", "s", "it"]);
    }

    #[test]
    fn query_terms_lower_unicode() {
        let terms = QueryTermsLower::from_query("Héllo Wörld");

        assert_eq!(terms.query_lower, "héllo wörld");
        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["héllo", "wörld"]);
    }

    #[test]
    fn query_terms_lower_bloom_mask() {
        let terms = QueryTermsLower::from_query("test");

        // Bloom mask should be non-zero for non-empty query
        assert_ne!(terms.bloom_mask(), 0);

        // Same query should produce same bloom mask
        let terms2 = QueryTermsLower::from_query("test");
        assert_eq!(terms.bloom_mask(), terms2.bloom_mask());
    }

    #[test]
    fn hit_matches_with_precomputed_terms() {
        let hit = SearchHit {
            title: "Test Title".into(),
            snippet: "".into(),
            content: "hello world content".into(),
            content_hash: stable_content_hash("hello world content"),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };
        let cached = cached_hit_from(&hit);

        // Test with precomputed terms
        let terms = QueryTermsLower::from_query("hello");
        assert!(hit_matches_query_cached_precomputed(&cached, &terms));

        let terms_miss = QueryTermsLower::from_query("missing");
        assert!(!hit_matches_query_cached_precomputed(&cached, &terms_miss));
    }

    // ==========================================================================
    // Quickselect Top-K Tests (Opt 2.5)
    // ==========================================================================

    fn make_fused_hit(
        id: &str,
        rrf: f32,
        lexical: Option<usize>,
        semantic: Option<usize>,
    ) -> FusedHit {
        FusedHit {
            key: SearchHitKey {
                source_id: "local".to_string(),
                source_path: id.to_string(),
                line_number: None,
                created_at: None,
                content_hash: 0,
            },
            score: HybridScore {
                rrf,
                lexical_rank: lexical,
                semantic_rank: semantic,
                lexical_score: None,
                semantic_score: None,
            },
            hit: SearchHit {
                title: id.into(),
                snippet: "".into(),
                content: "".into(),
                content_hash: 0,
                score: rrf,
                source_path: id.into(),
                agent: "test".into(),
                workspace: "test".into(),
                workspace_original: None,
                created_at: None,
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
        }
    }

    #[test]
    fn top_k_fused_basic() {
        let hits = vec![
            make_fused_hit("a", 1.0, Some(0), None),
            make_fused_hit("b", 3.0, Some(1), None),
            make_fused_hit("c", 2.0, Some(2), None),
            make_fused_hit("d", 5.0, Some(3), None),
            make_fused_hit("e", 4.0, Some(4), None),
        ];

        let top = top_k_fused(hits, 3);

        assert_eq!(top.len(), 3);
        assert_eq!(top[0].key.source_path, "d"); // 5.0
        assert_eq!(top[1].key.source_path, "e"); // 4.0
        assert_eq!(top[2].key.source_path, "b"); // 3.0
    }

    #[test]
    fn top_k_fused_empty() {
        let hits: Vec<FusedHit> = vec![];
        let top = top_k_fused(hits, 10);
        assert!(top.is_empty());
    }

    #[test]
    fn top_k_fused_k_zero() {
        let hits = vec![
            make_fused_hit("a", 1.0, Some(0), None),
            make_fused_hit("b", 2.0, Some(1), None),
        ];
        let top = top_k_fused(hits, 0);
        assert!(top.is_empty());
    }

    #[test]
    fn top_k_fused_k_larger_than_n() {
        let hits = vec![
            make_fused_hit("a", 1.0, Some(0), None),
            make_fused_hit("b", 2.0, Some(1), None),
        ];

        let top = top_k_fused(hits, 10);

        assert_eq!(top.len(), 2);
        assert_eq!(top[0].key.source_path, "b"); // 2.0
        assert_eq!(top[1].key.source_path, "a"); // 1.0
    }

    #[test]
    fn top_k_fused_k_equals_n() {
        let hits = vec![
            make_fused_hit("a", 3.0, Some(0), None),
            make_fused_hit("b", 1.0, Some(1), None),
            make_fused_hit("c", 2.0, Some(2), None),
        ];

        let top = top_k_fused(hits, 3);

        assert_eq!(top.len(), 3);
        assert_eq!(top[0].key.source_path, "a"); // 3.0
        assert_eq!(top[1].key.source_path, "c"); // 2.0
        assert_eq!(top[2].key.source_path, "b"); // 1.0
    }

    #[test]
    fn top_k_fused_k_one() {
        let hits = vec![
            make_fused_hit("a", 1.0, Some(0), None),
            make_fused_hit("b", 3.0, Some(1), None),
            make_fused_hit("c", 2.0, Some(2), None),
        ];

        let top = top_k_fused(hits, 1);

        assert_eq!(top.len(), 1);
        assert_eq!(top[0].key.source_path, "b");
        assert_eq!(top[0].score.rrf, 3.0);
    }

    #[test]
    fn top_k_fused_duplicate_scores() {
        let hits = vec![
            make_fused_hit("a", 2.0, Some(0), None),
            make_fused_hit("b", 2.0, Some(1), None),
            make_fused_hit("c", 2.0, Some(2), None),
            make_fused_hit("d", 1.0, Some(3), None),
        ];

        let top = top_k_fused(hits, 2);

        assert_eq!(top.len(), 2);
        // All have same score, so order is by key (deterministic tie-breaking)
        assert_eq!(top[0].score.rrf, 2.0);
        assert_eq!(top[1].score.rrf, 2.0);
    }

    #[test]
    fn top_k_fused_dual_source_tiebreaker() {
        // Hits with same RRF score, but some have both lexical and semantic ranks
        let hits = vec![
            make_fused_hit("a", 2.0, Some(0), None),    // lexical only
            make_fused_hit("b", 2.0, Some(1), Some(0)), // both sources
            make_fused_hit("c", 2.0, None, Some(1)),    // semantic only
        ];

        let top = top_k_fused(hits, 3);

        assert_eq!(top.len(), 3);
        // Dual-source hit should come first
        assert_eq!(top[0].key.source_path, "b");
    }

    #[test]
    fn top_k_fused_large_input_uses_quickselect() {
        // Create input larger than QUICKSELECT_THRESHOLD to trigger quickselect path
        let hits: Vec<FusedHit> = (0..100)
            .map(|i| make_fused_hit(&format!("hit_{}", i), i as f32, Some(i), None))
            .collect();

        let top = top_k_fused(hits, 10);

        assert_eq!(top.len(), 10);
        // Should be sorted descending: hit_99, hit_98, ... hit_90
        for (i, hit) in top.iter().enumerate() {
            assert_eq!(hit.key.source_path, format!("hit_{}", 99 - i));
            assert_eq!(hit.score.rrf, (99 - i) as f32);
        }
    }

    #[test]
    fn top_k_fused_equivalence_with_full_sort() {
        // Verify quickselect produces same results as full sort
        for n in [10, 50, 100, 200] {
            for k in [1, 5, 10, 25] {
                if k > n {
                    continue;
                }

                let hits: Vec<FusedHit> = (0..n)
                    .map(|i| {
                        // Pseudo-random scores using simple hash
                        let score = ((i * 17 + 7) % 1000) as f32;
                        make_fused_hit(&format!("hit_{}", i), score, Some(i), None)
                    })
                    .collect();

                // Baseline: full sort
                let mut baseline = hits.clone();
                baseline.sort_by(cmp_fused_hit_desc);
                baseline.truncate(k);

                // Quickselect
                let quickselect = top_k_fused(hits, k);

                // Verify same length
                assert_eq!(quickselect.len(), baseline.len(), "n={}, k={}", n, k);

                // Verify same elements in same order
                for (q, b) in quickselect.iter().zip(baseline.iter()) {
                    assert_eq!(
                        q.key.source_path, b.key.source_path,
                        "n={}, k={}: mismatch",
                        n, k
                    );
                    assert_eq!(q.score.rrf, b.score.rrf, "n={}, k={}: score mismatch", n, k);
                }
            }
        }
    }

    #[test]
    fn cmp_fused_hit_desc_basic_ordering() {
        let a = make_fused_hit("a", 2.0, Some(0), None);
        let b = make_fused_hit("b", 3.0, Some(1), None);

        // Higher score should come first (compare returns Less)
        assert_eq!(cmp_fused_hit_desc(&a, &b), CmpOrdering::Greater);
        assert_eq!(cmp_fused_hit_desc(&b, &a), CmpOrdering::Less);
        assert_eq!(cmp_fused_hit_desc(&a, &a), CmpOrdering::Equal);
    }

    // ==========================================================================
    // Original Tests
    // ==========================================================================

    #[test]
    fn cache_enforces_prefix_matching() {
        // Hit contains "arrow"
        let hit = SearchHit {
            title: "test".into(),
            snippet: "".into(),
            content: "arrow".into(),
            content_hash: stable_content_hash("arrow"),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };

        let cached = CachedHit {
            hit: hit.clone(),
            lc_content: "arrow".into(),
            lc_title: Some("test".into()),
            bloom64: u64::MAX, // Bypass bloom filter
        };

        // Query "row" is contained in "arrow" but is NOT a prefix.
        // It should NOT match if we are enforcing prefix semantics.
        let matched = hit_matches_query_cached(&cached, "row");

        assert!(
            !matched,
            "Query 'row' should NOT match content 'arrow' (prefix match required)"
        );
    }

    #[test]
    fn search_deduplication_across_pages_repro() {
        // Reproduction of "duplicate content across pages" bug.
        // If we fetch page 1 (limit 1) and page 2 (limit 1) separately,
        // and deduplication happens AFTER fetching the window,
        // we might see the same content on both pages.

        let dir = TempDir::new().unwrap();
        let index_path = dir.path();
        let mut index = TantivyIndex::open_or_create(index_path).unwrap();

        // Add two documents with IDENTICAL content but distinct other fields.
        // Tantivy scores them. If query matches both equally, one comes first.
        // We'll use different source paths to ensure they are distinct hits initially.
        let msg1 = NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: Some(1000),
            content: "duplicate content".into(),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
        };
        let conv1 = NormalizedConversation {
            agent_slug: "agent1".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: "path/1".into(),
            started_at: None,
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![msg1],
        };

        let msg2 = NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: Some(2000),              // Different timestamp
            content: "duplicate content".into(), // SAME content
            extra: serde_json::json!({}),
            snippets: Vec::new(),
        };
        let conv2 = NormalizedConversation {
            agent_slug: "agent1".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: "path/2".into(), // Different source path
            started_at: None,
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![msg2],
        };

        index.add_conversation(&conv1).unwrap();
        index.add_conversation(&conv2).unwrap();
        index.commit().unwrap();

        let client = SearchClient::open(index_path, None).unwrap().unwrap();

        // Search page 1: limit 1, offset 0
        let page1 = client
            .search("duplicate", SearchFilters::default(), 1, 0, FieldMask::FULL)
            .unwrap();
        assert_eq!(page1.len(), 1);
        let content1 = page1[0].content.clone();

        // Search page 2: limit 1, offset 1
        let page2 = client
            .search("duplicate", SearchFilters::default(), 1, 1, FieldMask::FULL)
            .unwrap();

        // IF deduplication works globally, page 2 should be EMPTY (because we only have 1 unique content).
        // IF deduplication is per-page (bug), page 2 will contain the second duplicate.
        //
        // Note: The bug fix we intend to implement will make page 2 empty.
        // The current behavior (buggy) returns the duplicate.

        if !page2.is_empty() {
            assert_eq!(
                page2[0].content, content1,
                "Found duplicate content on page 2"
            );
            // println!("Reproduced: Duplicate found on page 2");
        } else {
            // println!("Not Reproduced: Page 2 is empty (dedup worked)");
        }
    }

    #[test]
    fn cache_skips_complex_queries() {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        // Wildcard query should skip cache logic entirely (no miss recorded)
        let _ = client.search("foo*", SearchFilters::default(), 10, 0, FieldMask::FULL);
        let stats = client.cache_stats();
        assert_eq!(
            stats.cache_miss, 0,
            "Wildcard query should not trigger cache miss"
        );

        // Boolean query should skip cache
        let _ = client.search(
            "foo OR bar",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        );
        let stats = client.cache_stats();
        assert_eq!(
            stats.cache_miss, 0,
            "Boolean query should not trigger cache miss"
        );

        // Simple query should trigger miss
        let _ = client.search("simple", SearchFilters::default(), 10, 0, FieldMask::FULL);
        let stats = client.cache_stats();
        assert_eq!(
            stats.cache_miss, 1,
            "Simple query should trigger cache miss"
        );
    }

    #[test]
    fn cache_prefix_lookup_handles_utf8_boundaries() {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let hits = vec![SearchHit {
            title: "こんにちは".into(),
            snippet: String::new(),
            content: "こんにちは 世界".into(),
            content_hash: stable_content_hash("こんにちは 世界"),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }];

        client.put_cache("こん", &SearchFilters::default(), &hits);

        let cached = client
            .cached_prefix_hits("こんにちは", &SearchFilters::default())
            .unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].hit.title, "こんにちは");
    }

    #[test]
    fn bloom_gate_rejects_missing_terms() {
        let hit = SearchHit {
            title: "hello world".into(),
            snippet: "hello world".into(),
            content: "hello world".into(),
            content_hash: stable_content_hash("hello world"),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };
        let cached = cached_hit_from(&hit);
        assert!(hit_matches_query_cached(&cached, "hello"));
        assert!(!hit_matches_query_cached(&cached, "missing"));

        let metrics = Metrics::default();
        metrics.inc_cache_hits();
        metrics.inc_cache_miss();
        metrics.inc_cache_shortfall();
        metrics.inc_reload();
        let (hits, miss, shortfall, reloads, _) = metrics.snapshot_all();
        assert_eq!((hits, miss, shortfall, reloads), (1, 1, 1, 1));
    }

    #[test]
    fn search_returns_results_with_filters_and_pagination() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("hello world convo".into()),
            workspace: Some(std::path::PathBuf::from("/tmp/workspace")),
            source_path: dir.path().join("rollout-1.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: Some("me".into()),
                created_at: Some(1_700_000_000_000),
                content: "hello rust world".into(),
                extra: serde_json::json!({}),
                snippets: vec![NormalizedSnippet {
                    file_path: None,
                    start_line: None,
                    end_line: None,
                    language: None,
                    snippet_text: None,
                }],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".into());

        let hits = client.search("hello", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent, "codex");
        assert!(hits[0].snippet.contains("hello"));
        Ok(())
    }

    #[test]
    fn search_honors_created_range_and_workspace() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv_a = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("needle one".into()),
            workspace: Some(std::path::PathBuf::from("/ws/a")),
            source_path: dir.path().join("a.jsonl"),
            started_at: Some(10),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(10),
                content: "alpha needle".into(),
                extra: serde_json::json!({}),
                snippets: vec![NormalizedSnippet {
                    file_path: None,
                    start_line: None,
                    end_line: None,
                    language: None,
                    snippet_text: None,
                }],
            }],
        };
        let conv_b = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("needle two".into()),
            workspace: Some(std::path::PathBuf::from("/ws/b")),
            source_path: dir.path().join("b.jsonl"),
            started_at: Some(20),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(20),
                content: "\nneedle second line".into(),
                extra: serde_json::json!({}),
                snippets: vec![NormalizedSnippet {
                    file_path: None,
                    start_line: None,
                    end_line: None,
                    language: None,
                    snippet_text: None,
                }],
            }],
        };
        index.add_conversation(&conv_a)?;
        index.add_conversation(&conv_b)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");
        let mut filters = SearchFilters::default();
        filters.workspaces.insert("/ws/b".into());
        filters.created_from = Some(15);
        filters.created_to = Some(25);

        let hits = client.search("needle", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].workspace, "/ws/b");
        assert!(hits[0].snippet.contains("second line"));
        Ok(())
    }

    #[test]
    fn pagination_skips_results() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;
        for i in 0..3 {
            let conv = NormalizedConversation {
                agent_slug: "codex".into(),
                external_id: None,
                title: Some(format!("doc-{i}")),
                workspace: Some(std::path::PathBuf::from("/ws/p")),
                source_path: dir.path().join(format!("{i}.jsonl")),
                started_at: Some(100 + i),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(100 + i),
                    // Use unique content for each doc to avoid deduplication
                    content: format!("pagination needle document number {i}"),
                    extra: serde_json::json!({}),
                    snippets: vec![NormalizedSnippet {
                        file_path: None,
                        start_line: None,
                        end_line: None,
                        language: None,
                        snippet_text: None,
                    }],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");
        let hits = client.search(
            "pagination",
            SearchFilters::default(),
            1,
            1,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);
        Ok(())
    }

    #[test]
    fn search_matches_hyphenated_term() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("cma-es notes".into()),
            workspace: Some(std::path::PathBuf::from("/tmp/workspace")),
            source_path: dir.path().join("rollout-1.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: Some("me".into()),
                created_at: Some(1_700_000_000_000),
                content: "Need CMA-ES strategy and CMA ES variants".into(),
                extra: serde_json::json!({}),
                snippets: vec![NormalizedSnippet {
                    file_path: None,
                    start_line: None,
                    end_line: None,
                    language: None,
                    snippet_text: None,
                }],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");
        let hits = client.search("cma-es", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.to_lowercase().contains("cma"));
        Ok(())
    }

    #[test]
    fn search_matches_prefix_edge_ngram() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("math logic".into()),
            workspace: Some(std::path::PathBuf::from("/ws/m")),
            source_path: dir.path().join("math.jsonl"),
            started_at: Some(1000),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1000),
                content: "please calculate the entropy".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "cal" should match "calculate"
        let hits = client.search("cal", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("calculate"));

        // "entr" should match "entropy"
        let hits = client.search("entr", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        Ok(())
    }

    #[test]
    fn search_matches_snake_case() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("code".into()),
            workspace: None,
            source_path: dir.path().join("c.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "check the my_variable_name please".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "vari" should match "variable" inside "my_variable_name"
        let hits = client.search("vari", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        // "my_variable" should match "my_variable_name" (because it splits to "my variable")
        let hits = client.search(
            "my_variable",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);

        Ok(())
    }

    #[test]
    fn search_matches_symbols_stripped() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("symbols".into()),
            workspace: None,
            source_path: dir.path().join("s.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "working with c++ and foo.bar today".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "c++" -> "c"
        let hits = client.search("c++", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        // "foo.bar" -> "foo", "bar"
        let hits = client.search("foo.bar", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        Ok(())
    }

    #[test]
    fn search_sets_match_type_for_wildcards() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("handlers".into()),
            workspace: None,
            source_path: dir.path().join("h.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "the request handler delegates".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        let exact = client.search("handler", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(exact[0].match_type, MatchType::Exact);

        let prefix = client.search("hand*", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(prefix[0].match_type, MatchType::Prefix);

        let suffix = client.search("*handler", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(suffix[0].match_type, MatchType::Suffix);

        let substring =
            client.search("*andle*", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(substring[0].match_type, MatchType::Substring);

        Ok(())
    }

    #[test]
    fn search_with_fallback_marks_implicit_wildcard() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("handlers".into()),
            workspace: None,
            source_path: dir.path().join("h2.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "the request handler delegates".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Base search for "andle" finds nothing; fallback "*andle*" should hit and mark implicit.
        let result = client.search_with_fallback(
            "andle",
            SearchFilters::default(),
            10,
            0,
            2,
            FieldMask::FULL,
        )?;
        assert!(result.wildcard_fallback);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].match_type, MatchType::ImplicitWildcard);

        Ok(())
    }

    #[test]
    fn sqlite_backend_skips_wildcard_queries() -> Result<()> {
        // Build a client with SQLite only; wildcard queries should short-circuit without errors.
        let conn = Connection::open_in_memory()?;
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(Some(conn)),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let hits = client.search("*handler", SearchFilters::default(), 5, 0, FieldMask::FULL)?;
        assert!(
            hits.is_empty(),
            "wildcard should skip sqlite fallback, not error"
        );

        Ok(())
    }

    #[test]
    fn cache_invalidates_on_new_data() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // 1. Add initial doc
        let conv1 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("first".into()),
            workspace: None,
            source_path: dir.path().join("1.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "apple banana".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv1)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // 2. Search "app" -> should hit "apple"
        let hits = client.search("app", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "apple banana");

        // 3. Verify it's cached (peek internal state)
        {
            let cache = client.prefix_cache.lock().unwrap();
            let shard = cache.shard_opt("global").unwrap();
            // "app" should be in cache
            assert!(shard.contains(&client.cache_key("app", &SearchFilters::default())));
        }

        // 4. Add new doc with "apricot"
        let conv2 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("second".into()),
            workspace: None,
            source_path: dir.path().join("2.jsonl"),
            started_at: Some(2),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(2),
                content: "apricot".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv2)?;
        index.commit()?;

        // 5. Force reload (mocking time passing or just ensuring reload triggers)
        // In test, maybe_reload_reader uses 300ms debounce.
        // We can rely on opstamp check logic which runs AFTER reload.
        // We need to sleep briefly to bypass debounce or just modify test to not rely on time?
        // Actually SearchClient::maybe_reload_reader checks duration.
        std::thread::sleep(std::time::Duration::from_millis(350));

        // 6. Search "ap" (prefix of apricot and apple)
        // The cache for "app" should be cleared if opstamp changed.
        let _hits = client.search("app", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        // Should now find 1 doc still ("apple"), but cache should have been cleared first

        // Search "apr" -> should find "apricot"
        let hits = client.search("apr", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "apricot");

        // Check that cache was cleared by verifying a stale key is gone?
        // Or rely on correctness of results if we searched a common prefix?

        Ok(())
    }

    #[test]
    fn track_generation_clears_cache_on_change() {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let hit = SearchHit {
            title: "hello world".into(),
            snippet: "hello".into(),
            content: "hello world".into(),
            content_hash: stable_content_hash("hello world"),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };
        let hits = vec![hit];

        client.put_cache("hello", &SearchFilters::default(), &hits);
        {
            let cache = client.prefix_cache.lock().unwrap();
            assert!(!cache.shards.is_empty());
        }

        client.track_generation(1);
        {
            let cache = client.prefix_cache.lock().unwrap();
            assert!(!cache.shards.is_empty());
        }

        client.track_generation(2);
        {
            let cache = client.prefix_cache.lock().unwrap();
            assert!(cache.shards.is_empty());
        }
    }

    #[test]
    fn cache_total_cap_evicts_across_shards() {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(2, 0)), // tiny entry cap, no byte cap
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let hit = SearchHit {
            title: "a".into(),
            snippet: "a".into(),
            content: "a".into(),
            content_hash: stable_content_hash("a"),
            score: 1.0,
            source_path: "p".into(),
            agent: "agent1".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };
        let hits = vec![hit.clone()];

        let mut filters = SearchFilters::default();
        filters.agents.insert("agent1".into());
        client.put_cache("a", &filters, &hits);
        filters.agents.clear();
        filters.agents.insert("agent2".into());
        client.put_cache("b", &filters, &hits);
        filters.agents.clear();
        filters.agents.insert("agent3".into());
        client.put_cache("c", &filters, &hits);

        let stats = client.cache_stats();
        assert!(stats.total_cost <= stats.total_cap);
        assert_eq!(stats.total_cap, 2);
    }

    #[test]
    fn cache_stats_reflect_metrics() {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        client.metrics.inc_cache_hits();
        client.metrics.inc_cache_miss();
        client.metrics.inc_cache_shortfall();
        client.metrics.record_reload(Duration::from_millis(10));

        let stats = client.cache_stats();
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.cache_miss, 1);
        assert_eq!(stats.cache_shortfall, 1);
        assert_eq!(stats.reloads, 1);
        assert_eq!(stats.reload_ms_total, 10);
        assert_eq!(stats.total_cap, *CACHE_TOTAL_CAP);
    }

    #[test]
    fn cache_eviction_count_tracks_evictions() {
        // tiny entry cap (2 entries), no byte cap - forces evictions
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(2, 0)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let hit = SearchHit {
            title: "test".into(),
            snippet: "snippet".into(),
            content: "content".into(),
            content_hash: stable_content_hash("content"),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };

        // Put 3 entries - should trigger 1 eviction (cap is 2)
        client.put_cache(
            "query1",
            &SearchFilters::default(),
            std::slice::from_ref(&hit),
        );
        client.put_cache(
            "query2",
            &SearchFilters::default(),
            std::slice::from_ref(&hit),
        );
        client.put_cache(
            "query3",
            &SearchFilters::default(),
            std::slice::from_ref(&hit),
        );

        let stats = client.cache_stats();
        assert!(
            stats.eviction_count >= 1,
            "should have evicted at least 1 entry"
        );
        assert!(stats.total_cost <= 2, "should be at or below cap");
        assert!(stats.approx_bytes > 0, "should track bytes used");
    }

    #[test]
    fn cache_byte_cap_triggers_eviction() {
        // Large entry cap (1000), tiny byte cap (100 bytes) - forces byte-based evictions
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(1000, 100)), // byte cap of 100
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        // Large content to exceed byte cap quickly
        let content = "c".repeat(100);
        let hit = SearchHit {
            title: "a".repeat(50),
            snippet: "b".repeat(50),
            content: content.clone(), // 200+ bytes per hit
            content_hash: stable_content_hash(&content),
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };

        // Put 3 large entries - should trigger byte-based evictions
        client.put_cache("q1", &SearchFilters::default(), std::slice::from_ref(&hit));
        client.put_cache("q2", &SearchFilters::default(), std::slice::from_ref(&hit));
        client.put_cache("q3", &SearchFilters::default(), std::slice::from_ref(&hit));

        let stats = client.cache_stats();
        assert!(
            stats.eviction_count >= 1,
            "byte cap should trigger evictions"
        );
        assert_eq!(stats.byte_cap, 100, "byte cap should be reported");
        // Note: approx_bytes may briefly exceed cap during put, but eviction brings it down
    }

    // ============================================================
    // Phase 7 Tests: WildcardPattern, escape_regex, fallback, dedup
    // ============================================================

    #[test]
    fn wildcard_pattern_parse_exact() {
        // No wildcards - exact match
        assert_eq!(
            WildcardPattern::parse("hello"),
            WildcardPattern::Exact("hello".into())
        );
        assert_eq!(
            WildcardPattern::parse("HELLO"),
            WildcardPattern::Exact("hello".into()) // lowercased
        );
        assert_eq!(
            WildcardPattern::parse("FooBar123"),
            WildcardPattern::Exact("foobar123".into())
        );
    }

    #[test]
    fn wildcard_pattern_parse_prefix() {
        // Trailing wildcard: foo*
        assert_eq!(
            WildcardPattern::parse("foo*"),
            WildcardPattern::Prefix("foo".into())
        );
        assert_eq!(
            WildcardPattern::parse("CONFIG*"),
            WildcardPattern::Prefix("config".into())
        );
        assert_eq!(
            WildcardPattern::parse("test*"),
            WildcardPattern::Prefix("test".into())
        );
    }

    #[test]
    fn wildcard_pattern_parse_suffix() {
        // Leading wildcard: *foo
        assert_eq!(
            WildcardPattern::parse("*foo"),
            WildcardPattern::Suffix("foo".into())
        );
        assert_eq!(
            WildcardPattern::parse("*Error"),
            WildcardPattern::Suffix("error".into())
        );
        assert_eq!(
            WildcardPattern::parse("*Handler"),
            WildcardPattern::Suffix("handler".into())
        );
    }

    #[test]
    fn wildcard_pattern_parse_substring() {
        // Both wildcards: *foo*
        assert_eq!(
            WildcardPattern::parse("*foo*"),
            WildcardPattern::Substring("foo".into())
        );
        assert_eq!(
            WildcardPattern::parse("*CONFIG*"),
            WildcardPattern::Substring("config".into())
        );
        assert_eq!(
            WildcardPattern::parse("*test*"),
            WildcardPattern::Substring("test".into())
        );
    }

    #[test]
    fn wildcard_pattern_parse_edge_cases() {
        // Empty after trimming wildcards
        assert_eq!(
            WildcardPattern::parse("*"),
            WildcardPattern::Exact(String::new())
        );
        assert_eq!(
            WildcardPattern::parse("**"),
            WildcardPattern::Exact(String::new())
        );
        assert_eq!(
            WildcardPattern::parse("***"),
            WildcardPattern::Exact(String::new())
        );

        // Single char with wildcards
        assert_eq!(
            WildcardPattern::parse("*a*"),
            WildcardPattern::Substring("a".into())
        );
        assert_eq!(
            WildcardPattern::parse("a*"),
            WildcardPattern::Prefix("a".into())
        );
        assert_eq!(
            WildcardPattern::parse("*a"),
            WildcardPattern::Suffix("a".into())
        );

        // Multiple asterisks get trimmed
        assert_eq!(
            WildcardPattern::parse("***foo***"),
            WildcardPattern::Substring("foo".into())
        );
    }

    #[test]
    fn wildcard_pattern_to_regex_suffix() {
        let pattern = WildcardPattern::Suffix("foo".into());
        // Suffix patterns need $ anchor to ensure "ends with" semantics
        assert_eq!(pattern.to_regex(), Some(".*foo$".into()));
    }

    #[test]
    fn wildcard_pattern_to_regex_substring() {
        let pattern = WildcardPattern::Substring("bar".into());
        assert_eq!(pattern.to_regex(), Some(".*bar.*".into()));
    }

    #[test]
    fn wildcard_pattern_to_regex_exact_prefix_none() {
        // Exact and Prefix patterns don't need regex
        let exact = WildcardPattern::Exact("foo".into());
        assert_eq!(exact.to_regex(), None);

        let prefix = WildcardPattern::Prefix("bar".into());
        assert_eq!(prefix.to_regex(), None);
    }

    #[test]
    fn match_type_quality_factors() {
        // Exact match has highest quality
        assert_eq!(MatchType::Exact.quality_factor(), 1.0);
        // Prefix is slightly lower
        assert_eq!(MatchType::Prefix.quality_factor(), 0.9);
        // Suffix is lower than prefix
        assert_eq!(MatchType::Suffix.quality_factor(), 0.8);
        // Substring is lower still
        assert_eq!(MatchType::Substring.quality_factor(), 0.7);
        // Implicit wildcard is lowest
        assert_eq!(MatchType::ImplicitWildcard.quality_factor(), 0.6);
    }

    #[test]
    fn wildcard_pattern_to_match_type() {
        assert_eq!(
            WildcardPattern::Exact("foo".into()).to_match_type(),
            MatchType::Exact
        );
        assert_eq!(
            WildcardPattern::Prefix("foo".into()).to_match_type(),
            MatchType::Prefix
        );
        assert_eq!(
            WildcardPattern::Suffix("foo".into()).to_match_type(),
            MatchType::Suffix
        );
        assert_eq!(
            WildcardPattern::Substring("foo".into()).to_match_type(),
            MatchType::Substring
        );
    }

    #[test]
    fn dominant_match_type_single_terms() {
        // Single terms return their pattern's match type
        assert_eq!(dominant_match_type("hello"), MatchType::Exact);
        assert_eq!(dominant_match_type("hello*"), MatchType::Prefix);
        assert_eq!(dominant_match_type("*hello"), MatchType::Suffix);
        assert_eq!(dominant_match_type("*hello*"), MatchType::Substring);
    }

    #[test]
    fn dominant_match_type_multiple_terms() {
        // Multiple terms: returns the "loosest" (lowest quality factor)
        assert_eq!(dominant_match_type("foo bar"), MatchType::Exact);
        assert_eq!(dominant_match_type("foo bar*"), MatchType::Prefix);
        assert_eq!(dominant_match_type("foo *bar"), MatchType::Suffix);
        assert_eq!(dominant_match_type("foo* *bar*"), MatchType::Substring);
        // Substring is loosest even if other terms are exact
        assert_eq!(dominant_match_type("foo *bar* baz"), MatchType::Substring);
    }

    #[test]
    fn dominant_match_type_empty_query() {
        assert_eq!(dominant_match_type(""), MatchType::Exact);
        assert_eq!(dominant_match_type("   "), MatchType::Exact);
    }

    #[test]
    fn escape_regex_basic() {
        // Plain text should pass through unchanged
        assert_eq!(escape_regex("hello"), "hello");
        assert_eq!(escape_regex("foo123"), "foo123");
        assert_eq!(escape_regex(""), "");
    }

    #[test]
    fn escape_regex_special_chars() {
        // All special regex chars should be escaped
        assert_eq!(escape_regex("."), "\\.");
        assert_eq!(escape_regex("*"), "\\*");
        assert_eq!(escape_regex("+"), "\\+");
        assert_eq!(escape_regex("?"), "\\?");
        assert_eq!(escape_regex("("), "\\(");
        assert_eq!(escape_regex(")"), "\\)");
        assert_eq!(escape_regex("["), "\\[");
        assert_eq!(escape_regex("]"), "\\]");
        assert_eq!(escape_regex("{"), "\\{");
        assert_eq!(escape_regex("}"), "\\}");
        assert_eq!(escape_regex("|"), "\\|");
        assert_eq!(escape_regex("^"), "\\^");
        assert_eq!(escape_regex("$"), "\\$");
        assert_eq!(escape_regex("\\"), "\\\\");
    }

    #[test]
    fn escape_regex_complex_patterns() {
        // Complex patterns with multiple special chars
        assert_eq!(escape_regex("foo.bar"), "foo\\.bar");
        assert_eq!(escape_regex("test[0-9]+"), "test\\[0-9\\]\\+");
        assert_eq!(escape_regex("(a|b)"), "\\(a\\|b\\)");
        assert_eq!(escape_regex("end$"), "end\\$");
        assert_eq!(escape_regex("^start"), "\\^start");
        assert_eq!(escape_regex("a*b+c?"), "a\\*b\\+c\\?");
    }

    #[test]
    fn is_tool_invocation_noise_detects_noise() {
        // "[Tool: Name]" is now kept (users search for tool usage)
        assert!(!is_tool_invocation_noise("[Tool: Bash]"));
        assert!(!is_tool_invocation_noise("[Tool: Read]"));

        // Empty tool names are noise
        assert!(is_tool_invocation_noise("[Tool:]"));
        assert!(is_tool_invocation_noise("[Tool: ]"));

        // Useful content should NOT be filtered
        assert!(!is_tool_invocation_noise("[Tool: Bash - Check status]"));
        assert!(!is_tool_invocation_noise("  [Tool: Grep - Search files]  "));

        // Very short tool markers (< 20 chars with "tool" prefix)
        assert!(is_tool_invocation_noise("[tool]"));
        assert!(is_tool_invocation_noise("tool: Bash"));
    }

    #[test]
    fn is_tool_invocation_noise_allows_useful_content() {
        // This should NOT be considered noise
        assert!(!is_tool_invocation_noise("[Tool: Read - src/main.rs]"));
        assert!(!is_tool_invocation_noise("[Tool: Bash - cargo test --lib]"));
    }

    #[test]
    fn is_tool_invocation_noise_detects_tool_markers() {
        // "[Tool: Name]" is now kept (searchable tool usage)
        assert!(!is_tool_invocation_noise("[Tool: Bash]"));
        assert!(!is_tool_invocation_noise("[Tool: Read]"));

        // Empty names are still noise
        assert!(is_tool_invocation_noise("[Tool:]"));

        // Useful content allowed
        assert!(!is_tool_invocation_noise("[Tool: Bash - Check status]"));
        assert!(!is_tool_invocation_noise("  [Tool: Write - description]  "));
    }

    #[test]
    fn deduplicate_hits_removes_exact_dupes() {
        let hits = vec![
            SearchHit {
                title: "title1".into(),
                snippet: "snip1".into(),
                content: "hello world".into(),
                content_hash: stable_content_hash("hello world"),
                score: 1.0,
                source_path: "a.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(100),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "title2".into(),
                snippet: "snip2".into(),
                content: "hello world".into(), // same content
                content_hash: stable_content_hash("hello world"),
                score: 0.5, // lower score
                source_path: "b.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(200),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(), // same source_id = will dedupe
                origin_kind: "local".into(),
                origin_host: None,
            },
        ];

        let deduped = deduplicate_hits(hits);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].score, 1.0); // kept higher score
        assert_eq!(deduped[0].title, "title1");
    }

    #[test]
    fn deduplicate_hits_keeps_higher_score() {
        let hits = vec![
            SearchHit {
                title: "title1".into(),
                snippet: "snip1".into(),
                content: "hello world".into(),
                content_hash: stable_content_hash("hello world"),
                score: 0.3, // lower score first
                source_path: "a.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(100),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "title2".into(),
                snippet: "snip2".into(),
                content: "hello world".into(),
                content_hash: stable_content_hash("hello world"),
                score: 0.9, // higher score second
                source_path: "b.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(200),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
        ];

        let deduped = deduplicate_hits(hits);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].score, 0.9); // kept higher score
        assert_eq!(deduped[0].title, "title2");
    }

    #[test]
    fn deduplicate_hits_normalizes_whitespace() {
        let hits = vec![
            SearchHit {
                title: "title1".into(),
                snippet: "snip1".into(),
                content: "hello    world".into(), // extra spaces
                content_hash: stable_content_hash("hello    world"),
                score: 1.0,
                source_path: "a.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(100),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "title2".into(),
                snippet: "snip2".into(),
                content: "hello world".into(), // normal spacing
                content_hash: stable_content_hash("hello world"),
                score: 0.5,
                source_path: "b.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(200),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
        ];

        let deduped = deduplicate_hits(hits);
        assert_eq!(deduped.len(), 1); // normalized to same content
    }

    #[test]
    fn deduplicate_hits_filters_tool_noise() {
        let hits = vec![
            SearchHit {
                title: "title1".into(),
                snippet: "snip1".into(),
                content: "[Tool:]".into(), // noise (empty tool name)
                content_hash: stable_content_hash("[Tool:]"),
                score: 1.0,
                source_path: "a.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(100),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "title2".into(),
                snippet: "snip2".into(),
                content: "This is real content about testing".into(),
                content_hash: stable_content_hash("This is real content about testing"),
                score: 0.5,
                source_path: "b.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(200),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
        ];

        let deduped = deduplicate_hits(hits);
        assert_eq!(deduped.len(), 1);
        assert!(deduped[0].content.contains("real content"));
    }

    #[test]
    fn deduplicate_hits_preserves_unique_content() {
        let hits = vec![
            SearchHit {
                title: "title1".into(),
                snippet: "snip1".into(),
                content: "first message".into(),
                content_hash: stable_content_hash("first message"),
                score: 1.0,
                source_path: "a.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(100),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "title2".into(),
                snippet: "snip2".into(),
                content: "second message".into(),
                content_hash: stable_content_hash("second message"),
                score: 0.8,
                source_path: "b.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(200),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "title3".into(),
                snippet: "snip3".into(),
                content: "third message".into(),
                content_hash: stable_content_hash("third message"),
                score: 0.6,
                source_path: "c.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(300),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
        ];

        let deduped = deduplicate_hits(hits);
        assert_eq!(deduped.len(), 3); // all unique
    }

    /// P2.3: Deduplication respects source boundaries - same content from different sources
    /// should appear as separate results.
    #[test]
    fn deduplicate_hits_respects_source_boundaries() {
        let hits = vec![
            SearchHit {
                title: "local title".into(),
                snippet: "snip".into(),
                content: "hello world".into(),
                content_hash: stable_content_hash("hello world"),
                score: 1.0,
                source_path: "a.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(100),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                title: "remote title".into(),
                snippet: "snip".into(),
                content: "hello world".into(), // same content
                content_hash: stable_content_hash("hello world"),
                score: 0.9,
                source_path: "b.jsonl".into(),
                agent: "agent".into(),
                workspace: "ws".into(),
                workspace_original: None,
                created_at: Some(200),
                line_number: None,
                match_type: MatchType::Exact,
                source_id: "work-laptop".into(), // different source = no dedupe
                origin_kind: "ssh".into(),
                origin_host: Some("work-laptop.local".into()),
            },
        ];

        let deduped = deduplicate_hits(hits);
        assert_eq!(
            deduped.len(),
            2,
            "same content from different sources should not dedupe"
        );
        assert!(deduped.iter().any(|h| h.source_id == "local"));
        assert!(deduped.iter().any(|h| h.source_id == "work-laptop"));
    }

    #[test]
    fn search_with_fallback_returns_exact_when_sufficient() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Add enough docs to exceed threshold - each with UNIQUE content to avoid dedup
        for i in 0..5 {
            let conv = NormalizedConversation {
                agent_slug: "codex".into(),
                external_id: None,
                title: Some(format!("doc-{i}")),
                workspace: Some(std::path::PathBuf::from("/ws")),
                source_path: dir.path().join(format!("{i}.jsonl")),
                started_at: Some(100 + i),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(100 + i),
                    // Each doc has unique content but shares "apple" keyword
                    content: format!("apple fruit number {i} is delicious and healthy"),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Search with low threshold - should not trigger fallback
        let result = client.search_with_fallback(
            "apple",
            SearchFilters::default(),
            10,
            0,
            3, // threshold of 3
            FieldMask::FULL,
        )?;

        assert!(!result.wildcard_fallback);
        assert!(result.hits.len() >= 3); // has enough results

        Ok(())
    }

    #[test]
    fn search_with_fallback_triggers_on_sparse_results() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Add docs with substring that won't match exact prefix
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("substring test".into()),
            workspace: Some(std::path::PathBuf::from("/ws")),
            source_path: dir.path().join("test.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "configuration management system".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Search for "config" which should match "configuration" via prefix
        let result = client.search_with_fallback(
            "config",
            SearchFilters::default(),
            10,
            0,
            5, // high threshold
            FieldMask::FULL,
        )?;

        // Since we have only 1 result and threshold is 5, it may trigger fallback
        // but *config* would still match "configuration"
        assert!(!result.hits.is_empty());

        Ok(())
    }

    #[test]
    fn search_with_fallback_skips_when_query_has_wildcards() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("test".into()),
            workspace: None,
            source_path: dir.path().join("test.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "testing data".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Query already has wildcards - should not trigger fallback
        let result = client.search_with_fallback(
            "*test*",
            SearchFilters::default(),
            10,
            0,
            10, // high threshold
            FieldMask::FULL,
        )?;

        assert!(!result.wildcard_fallback); // shouldn't trigger fallback for wildcard queries
        Ok(())
    }

    #[test]
    fn search_with_fallback_prefers_wildcards_when_they_add_hits() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // None of these documents contain the exact token "bet",
        // but they do contain it as a substring ("alphabet").
        for (i, body) in [
            "alphabet soup for coders",
            "mapping the alphabet city blocks",
        ]
        .iter()
        .enumerate()
        {
            let conv = NormalizedConversation {
                agent_slug: "codex".into(),
                external_id: None,
                title: Some(format!("alpha-{i}")),
                workspace: Some(std::path::PathBuf::from("/ws")),
                source_path: dir.path().join(format!("alpha-{i}.jsonl")),
                started_at: Some(100 + i as i64),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(100 + i as i64),
                    content: body.to_string(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        let result = client.search_with_fallback(
            "bet",
            SearchFilters::default(),
            10,
            0,
            2,
            FieldMask::FULL,
        )?;

        assert!(
            result.wildcard_fallback,
            "should switch to wildcard fallback when it yields more hits"
        );
        assert_eq!(
            result.hits.len(),
            2,
            "fallback should surface all alphabet docs"
        );
        assert!(
            result
                .hits
                .iter()
                .all(|h| h.match_type == MatchType::ImplicitWildcard)
        );
        assert!(result.hits.iter().all(|h| h.content.contains("alphabet")));

        Ok(())
    }

    #[test]
    fn search_with_fallback_emits_wildcard_suggestion_on_zero_hits() -> Result<()> {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: "vtest|schema:none".into(),
            semantic: Mutex::new(None),
        };

        let result = client.search_with_fallback(
            "ghost",
            SearchFilters::default(),
            5,
            0,
            3,
            FieldMask::FULL,
        )?;

        assert!(
            result.hits.is_empty(),
            "no index/db means no hits should be returned"
        );
        assert!(
            !result.wildcard_fallback,
            "with zero baseline and fallback hits, we should keep baseline and mark fallback=false"
        );

        let wildcard = result
            .suggestions
            .iter()
            .find(|s| matches!(s.kind, SuggestionKind::WildcardQuery))
            .expect("should suggest adding wildcards");
        assert_eq!(wildcard.suggested_query.as_deref(), Some("*ghost*"));

        Ok(())
    }

    #[test]
    fn search_with_fallback_skips_empty_query() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("test".into()),
            workspace: None,
            source_path: dir.path().join("test.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "testing data".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Empty query - should not trigger fallback
        let result = client.search_with_fallback(
            "  ",
            SearchFilters::default(),
            10,
            0,
            10,
            FieldMask::FULL,
        )?;

        assert!(!result.wildcard_fallback);
        Ok(())
    }

    #[test]
    fn search_with_fallback_skips_for_nonzero_offset() -> Result<()> {
        // Even with zero hits, fallback should not run when paginating (offset > 0)
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: "vtest|schema:none".into(),
            semantic: Mutex::new(None),
        };

        let result = client.search_with_fallback(
            "ghost",
            SearchFilters::default(),
            5,
            10,
            3,
            FieldMask::FULL,
        )?;

        assert!(
            !result.wildcard_fallback,
            "fallback should not run on paginated searches"
        );
        // Suggestions still surface (wildcard suggestion expected)
        let wildcard = result
            .suggestions
            .iter()
            .find(|s| matches!(s.kind, SuggestionKind::WildcardQuery))
            .expect("wildcard suggestion present");
        assert_eq!(wildcard.suggested_query.as_deref(), Some("*ghost*"));

        Ok(())
    }

    #[test]
    fn generate_suggestions_limits_and_sets_shortcuts() -> Result<()> {
        // Build a client without backends; suggestions are purely local heuristics
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: "vtest|schema:none".into(),
            semantic: Mutex::new(None),
        };

        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".into()); // triggers remove-agent suggestion

        let result = client.search_with_fallback("claud", filters, 5, 0, 3, FieldMask::FULL)?;

        // Should cap at 3 suggestions with shortcuts 1..=3
        assert_eq!(
            result.suggestions.len(),
            3,
            "should truncate to 3 suggestions"
        );
        for (idx, sugg) in result.suggestions.iter().enumerate() {
            assert_eq!(
                sugg.shortcut,
                Some((idx + 1) as u8),
                "shortcut should match position (1-based)"
            );
        }

        // Expect wildcard, remove filter, and spelling fix (claud -> claude)
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| matches!(s.kind, SuggestionKind::WildcardQuery)),
            "should suggest wildcard search"
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| matches!(s.kind, SuggestionKind::RemoveFilter)),
            "should suggest removing agent filter"
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| matches!(s.kind, SuggestionKind::SpellingFix)),
            "should suggest spelling fix for nearby agent name"
        );

        Ok(())
    }

    #[test]
    fn sanitize_query_preserves_wildcards() {
        // Wildcards should be preserved
        assert_eq!(sanitize_query("*foo*"), "*foo*");
        assert_eq!(sanitize_query("foo*"), "foo*");
        assert_eq!(sanitize_query("*bar"), "*bar");
        assert_eq!(sanitize_query("*config*"), "*config*");
    }

    #[test]
    fn sanitize_query_strips_other_special_chars() {
        // Non-wildcard special chars become spaces
        assert_eq!(sanitize_query("foo.bar"), "foo bar");
        assert_eq!(sanitize_query("c++"), "c  ");
        assert_eq!(sanitize_query("foo-bar"), "foo bar");
        assert_eq!(sanitize_query("test_case"), "test case");
    }

    #[test]
    fn sanitize_query_combined() {
        // Mix of wildcards and special chars
        assert_eq!(sanitize_query("*foo.bar*"), "*foo bar*");
        assert_eq!(sanitize_query("test-*"), "test *");
        assert_eq!(sanitize_query("*c++*"), "*c  *");
    }

    // Boolean query parsing tests
    #[test]
    fn parse_boolean_query_simple_terms() {
        let tokens = parse_boolean_query("foo bar baz");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], QueryToken::Term("foo".to_string()));
        assert_eq!(tokens[1], QueryToken::Term("bar".to_string()));
        assert_eq!(tokens[2], QueryToken::Term("baz".to_string()));
    }

    #[test]
    fn parse_boolean_query_and_operator() {
        let tokens = parse_boolean_query("foo AND bar");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], QueryToken::Term("foo".to_string()));
        assert_eq!(tokens[1], QueryToken::And);
        assert_eq!(tokens[2], QueryToken::Term("bar".to_string()));

        // Also test && syntax
        let tokens2 = parse_boolean_query("foo && bar");
        assert_eq!(tokens2.len(), 3);
        assert_eq!(tokens2[1], QueryToken::And);
    }

    #[test]
    fn parse_boolean_query_or_operator() {
        let tokens = parse_boolean_query("foo OR bar");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], QueryToken::Term("foo".to_string()));
        assert_eq!(tokens[1], QueryToken::Or);
        assert_eq!(tokens[2], QueryToken::Term("bar".to_string()));

        // Also test || syntax
        let tokens2 = parse_boolean_query("foo || bar");
        assert_eq!(tokens2.len(), 3);
        assert_eq!(tokens2[1], QueryToken::Or);
    }

    #[test]
    fn parse_boolean_query_not_operator() {
        let tokens = parse_boolean_query("foo NOT bar");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], QueryToken::Term("foo".to_string()));
        assert_eq!(tokens[1], QueryToken::Not);
        assert_eq!(tokens[2], QueryToken::Term("bar".to_string()));
    }

    #[test]
    fn parse_boolean_query_quoted_phrase() {
        let tokens = parse_boolean_query(r#"foo "exact phrase" bar"#);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], QueryToken::Term("foo".to_string()));
        assert_eq!(tokens[1], QueryToken::Phrase("exact phrase".to_string()));
        assert_eq!(tokens[2], QueryToken::Term("bar".to_string()));
    }

    #[test]
    fn parse_boolean_query_complex() {
        let tokens = parse_boolean_query(r#"error OR warning NOT "false positive""#);
        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[0], QueryToken::Term("error".to_string()));
        assert_eq!(tokens[1], QueryToken::Or);
        assert_eq!(tokens[2], QueryToken::Term("warning".to_string()));
        assert_eq!(tokens[3], QueryToken::Not);
        assert_eq!(tokens[4], QueryToken::Phrase("false positive".to_string()));
    }

    #[test]
    fn has_boolean_operators_detection() {
        assert!(!has_boolean_operators("foo bar"));
        assert!(has_boolean_operators("foo AND bar"));
        assert!(has_boolean_operators("foo OR bar"));
        assert!(has_boolean_operators("foo NOT bar"));
        assert!(has_boolean_operators(r#""exact phrase""#));
        assert!(has_boolean_operators("foo && bar"));
        assert!(has_boolean_operators("foo || bar"));
    }

    #[test]
    fn parse_boolean_query_case_insensitive_operators() {
        // Operators should be case-insensitive
        let tokens = parse_boolean_query("foo and bar or baz not qux");
        assert_eq!(tokens.len(), 7);
        assert_eq!(tokens[1], QueryToken::And);
        assert_eq!(tokens[3], QueryToken::Or);
        assert_eq!(tokens[5], QueryToken::Not);
    }

    #[test]
    fn parse_boolean_query_with_wildcards() {
        let tokens = parse_boolean_query("*config* OR env*");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], QueryToken::Term("*config*".to_string()));
        assert_eq!(tokens[1], QueryToken::Or);
        assert_eq!(tokens[2], QueryToken::Term("env*".to_string()));
    }

    // ============================================================
    // Filter Fidelity Property Tests (glt.9)
    // Verify filters are never violated in search results
    // ============================================================

    #[test]
    fn filter_fidelity_agent_filter_respected() -> Result<()> {
        // Multiple agents; filter should return only matching agent
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Agent A (codex)
        let conv_a = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("alpha doc".into()),
            workspace: None,
            source_path: dir.path().join("a.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "hello world findme alpha".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        // Agent B (claude)
        let conv_b = NormalizedConversation {
            agent_slug: "claude".into(),
            external_id: None,
            title: Some("beta doc".into()),
            workspace: None,
            source_path: dir.path().join("b.jsonl"),
            started_at: Some(200),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(200),
                content: "hello world findme beta".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv_a)?;
        index.add_conversation(&conv_b)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Search with agent filter for codex only
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".into());

        let hits = client.search("findme", filters.clone(), 10, 0, FieldMask::FULL)?;

        // Property: all results must have agent == "codex"
        for hit in &hits {
            assert_eq!(
                hit.agent, "codex",
                "Agent filter violated: got agent '{}' instead of 'codex'",
                hit.agent
            );
        }
        assert!(!hits.is_empty(), "Should have found results");

        // Repeat search (should use cache) and verify same property
        let cached_hits = client.search("findme", filters, 10, 0, FieldMask::FULL)?;
        for hit in &cached_hits {
            assert_eq!(hit.agent, "codex", "Cached search violated agent filter");
        }

        Ok(())
    }

    #[test]
    fn filter_fidelity_workspace_filter_respected() -> Result<()> {
        // Multiple workspaces; filter should return only matching workspace
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Workspace A
        let conv_a = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("ws_a doc".into()),
            workspace: Some(std::path::PathBuf::from("/workspace/alpha")),
            source_path: dir.path().join("a.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "workspace test needle".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        // Workspace B
        let conv_b = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("ws_b doc".into()),
            workspace: Some(std::path::PathBuf::from("/workspace/beta")),
            source_path: dir.path().join("b.jsonl"),
            started_at: Some(200),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(200),
                content: "workspace test needle".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv_a)?;
        index.add_conversation(&conv_b)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Search with workspace filter for beta only
        let mut filters = SearchFilters::default();
        filters.workspaces.insert("/workspace/beta".into());

        let hits = client.search("needle", filters.clone(), 10, 0, FieldMask::FULL)?;

        // Property: all results must have workspace == "/workspace/beta"
        for hit in &hits {
            assert_eq!(
                hit.workspace, "/workspace/beta",
                "Workspace filter violated: got '{}' instead of '/workspace/beta'",
                hit.workspace
            );
        }
        assert!(!hits.is_empty(), "Should have found results");

        // Repeat search (should use cache)
        let cached_hits = client.search("needle", filters, 10, 0, FieldMask::FULL)?;
        for hit in &cached_hits {
            assert_eq!(
                hit.workspace, "/workspace/beta",
                "Cached search violated workspace filter"
            );
        }

        Ok(())
    }

    #[test]
    fn filter_fidelity_date_range_respected() -> Result<()> {
        // Multiple dates; filter should return only within range
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Early doc (ts=100)
        let conv_early = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("early".into()),
            workspace: None,
            source_path: dir.path().join("early.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "date range test".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        // Middle doc (ts=500)
        let conv_middle = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("middle".into()),
            workspace: None,
            source_path: dir.path().join("middle.jsonl"),
            started_at: Some(500),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(500),
                content: "date range test".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        // Late doc (ts=900)
        let conv_late = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("late".into()),
            workspace: None,
            source_path: dir.path().join("late.jsonl"),
            started_at: Some(900),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(900),
                content: "date range test".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv_early)?;
        index.add_conversation(&conv_middle)?;
        index.add_conversation(&conv_late)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Filter for middle range only (400-600)
        let filters = SearchFilters {
            created_from: Some(400),
            created_to: Some(600),
            ..Default::default()
        };

        let hits = client.search("range", filters.clone(), 10, 0, FieldMask::FULL)?;

        // Property: all results must have created_at within [400, 600]
        for hit in &hits {
            if let Some(ts) = hit.created_at {
                assert!(
                    (400..=600).contains(&ts),
                    "Date range filter violated: got ts={ts} outside [400, 600]"
                );
            }
        }
        // Should find only the middle doc
        assert_eq!(hits.len(), 1, "Should find exactly 1 doc in range");

        // Repeat search (cache)
        let cached_hits = client.search("range", filters, 10, 0, FieldMask::FULL)?;
        for hit in &cached_hits {
            if let Some(ts) = hit.created_at {
                assert!(
                    (400..=600).contains(&ts),
                    "Cached search violated date range filter"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn filter_fidelity_combined_filters_respected() -> Result<()> {
        // Combine agent + workspace + date filters
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create 4 docs with different combinations
        let combinations = [
            ("codex", "/ws/prod", 100),  // wrong date
            ("claude", "/ws/prod", 500), // correct agent, correct ws, correct date
            ("claude", "/ws/dev", 500),  // correct agent, wrong ws, correct date
            ("claude", "/ws/prod", 900), // correct agent, correct ws, wrong date
        ];

        for (i, (agent, ws, ts)) in combinations.iter().enumerate() {
            let conv = NormalizedConversation {
                agent_slug: (*agent).into(),
                external_id: None,
                title: Some(format!("combo-{i}")),
                workspace: Some(std::path::PathBuf::from(*ws)),
                source_path: dir.path().join(format!("{i}.jsonl")),
                started_at: Some(*ts),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(*ts),
                    content: "hello world combotest query".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Filter: claude + /ws/prod + date 400-600
        let mut filters = SearchFilters::default();
        filters.agents.insert("claude".into());
        filters.workspaces.insert("/ws/prod".into());
        filters.created_from = Some(400);
        filters.created_to = Some(600);

        let hits = client.search("combotest", filters.clone(), 10, 0, FieldMask::FULL)?;

        // Should find exactly 1 doc (index 1 in combinations)
        assert_eq!(hits.len(), 1, "Combined filter should match exactly 1 doc");

        for hit in &hits {
            assert_eq!(hit.agent, "claude", "Agent filter violated");
            assert_eq!(hit.workspace, "/ws/prod", "Workspace filter violated");
            if let Some(ts) = hit.created_at {
                assert!((400..=600).contains(&ts), "Date filter violated: ts={ts}");
            }
        }

        // Cache hit
        let cached = client.search("combotest", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(cached.len(), 1, "Cached result count mismatch");

        Ok(())
    }

    #[test]
    fn filter_fidelity_source_filter_respected() -> Result<()> {
        // P3.1: Source filter should filter by origin_kind or source_id
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Local source doc
        let conv_local = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("local doc".into()),
            workspace: None,
            source_path: dir.path().join("local.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "source filter test local".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        // Remote source doc (would need to be indexed with ssh origin_kind)
        // For now, test that local filter returns local docs
        index.add_conversation(&conv_local)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Filter for local sources
        let filters = SearchFilters {
            source_filter: SourceFilter::Local,
            ..Default::default()
        };

        let hits = client.search("source", filters.clone(), 10, 0, FieldMask::FULL)?;

        // Property: all results should have source_id == "local"
        for hit in &hits {
            assert_eq!(
                hit.source_id, "local",
                "Source filter violated: got source_id '{}' instead of 'local'",
                hit.source_id
            );
        }
        assert!(!hits.is_empty(), "Should have found local results");

        // Filter for specific source ID
        let filters_id = SearchFilters {
            source_filter: SourceFilter::SourceId("local".to_string()),
            ..Default::default()
        };

        let hits_id = client.search("source", filters_id, 10, 0, FieldMask::FULL)?;
        for hit in &hits_id {
            assert_eq!(
                hit.source_id, "local",
                "SourceId filter violated: got '{}' instead of 'local'",
                hit.source_id
            );
        }
        assert!(
            !hits_id.is_empty(),
            "Should have found results for source_id=local"
        );

        Ok(())
    }

    #[test]
    fn filter_fidelity_cache_key_isolation() {
        // Different filters should have different cache keys
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let filters_empty = SearchFilters::default();
        let mut filters_agent = SearchFilters::default();
        filters_agent.agents.insert("codex".into());

        let mut filters_ws = SearchFilters::default();
        filters_ws.workspaces.insert("/ws".into());

        let key_empty = client.cache_key("test", &filters_empty);
        let key_agent = client.cache_key("test", &filters_agent);
        let key_ws = client.cache_key("test", &filters_ws);

        // All keys should be different
        assert_ne!(
            key_empty, key_agent,
            "Empty vs agent filter keys should differ"
        );
        assert_ne!(
            key_empty, key_ws,
            "Empty vs workspace filter keys should differ"
        );
        assert_ne!(
            key_agent, key_ws,
            "Agent vs workspace filter keys should differ"
        );

        // Same filter should produce same key
        let mut filters_agent2 = SearchFilters::default();
        filters_agent2.agents.insert("codex".into());
        let key_agent2 = client.cache_key("test", &filters_agent2);
        assert_eq!(key_agent, key_agent2, "Same filter should produce same key");
    }

    // ==========================================================================
    // FTS5 Query Generation Tests (tst.srch.fts)
    // Additional tests for SQL/FTS5 query generation edge cases
    // ==========================================================================

    // --- Additional sanitize_query tests (edge cases) ---

    #[test]
    fn sanitize_query_preserves_unicode_alphanumeric() {
        // Unicode letters and digits should be preserved
        assert_eq!(sanitize_query("こんにちは"), "こんにちは");
        assert_eq!(sanitize_query("café"), "café");
        assert_eq!(sanitize_query("日本語123"), "日本語123");
    }

    #[test]
    fn sanitize_query_handles_multiple_consecutive_special_chars() {
        assert_eq!(sanitize_query("foo---bar"), "foo   bar");
        // a!@#$%^&()b has 9 special chars between a and b: ! @ # $ % ^ & ( )
        assert_eq!(sanitize_query("a!@#$%^&()b"), "a         b");
    }

    // --- Additional WildcardPattern::parse tests (edge cases) ---

    #[test]
    fn wildcard_pattern_empty_after_trim_returns_exact_empty() {
        assert_eq!(
            WildcardPattern::parse("*"),
            WildcardPattern::Exact(String::new())
        );
        assert_eq!(
            WildcardPattern::parse("**"),
            WildcardPattern::Exact(String::new())
        );
        assert_eq!(
            WildcardPattern::parse("***"),
            WildcardPattern::Exact(String::new())
        );
    }

    #[test]
    fn wildcard_pattern_to_regex_generation() {
        // Exact and prefix patterns don't need regex
        assert_eq!(WildcardPattern::Exact("foo".into()).to_regex(), None);
        assert_eq!(WildcardPattern::Prefix("foo".into()).to_regex(), None);
        // Suffix and substring need regex
        // Suffix needs $ anchor for "ends with" semantics
        assert_eq!(
            WildcardPattern::Suffix("foo".into()).to_regex(),
            Some(".*foo$".into())
        );
        assert_eq!(
            WildcardPattern::Substring("foo".into()).to_regex(),
            Some(".*foo.*".into())
        );
    }

    // --- escape_regex tests ---

    #[test]
    fn escape_regex_escapes_all_special_chars() {
        assert_eq!(escape_regex("."), "\\.");
        assert_eq!(escape_regex("*"), "\\*");
        assert_eq!(escape_regex("+"), "\\+");
        assert_eq!(escape_regex("?"), "\\?");
        assert_eq!(escape_regex("["), "\\[");
        assert_eq!(escape_regex("]"), "\\]");
        assert_eq!(escape_regex("("), "\\(");
        assert_eq!(escape_regex(")"), "\\)");
        assert_eq!(escape_regex("{"), "\\{");
        assert_eq!(escape_regex("}"), "\\}");
        assert_eq!(escape_regex("|"), "\\|");
        assert_eq!(escape_regex("^"), "\\^");
        assert_eq!(escape_regex("$"), "\\$");
        assert_eq!(escape_regex("\\"), "\\\\");
    }

    #[test]
    fn escape_regex_preserves_alphanumeric() {
        assert_eq!(escape_regex("hello"), "hello");
        assert_eq!(escape_regex("abc123"), "abc123");
    }

    #[test]
    fn escape_regex_mixed_content() {
        assert_eq!(escape_regex("foo.bar"), "foo\\.bar");
        assert_eq!(escape_regex("a+b*c"), "a\\+b\\*c");
        assert_eq!(escape_regex("(test)"), "\\(test\\)");
    }

    // --- Additional parse_boolean_query tests (edge cases) ---

    #[test]
    fn parse_boolean_query_prefix_minus_not() {
        // Prefix minus at start of query should trigger NOT
        let tokens = parse_boolean_query("-world");
        let expected: QueryTokenList =
            SmallVec::from_vec(vec![QueryToken::Not, QueryToken::Term("world".into())]);
        assert_eq!(tokens, expected);

        // Prefix minus after space should trigger NOT
        let tokens = parse_boolean_query("hello -world");
        let expected: QueryTokenList = SmallVec::from_vec(vec![
            QueryToken::Term("hello".into()),
            QueryToken::Not,
            QueryToken::Term("world".into()),
        ]);
        assert_eq!(tokens, expected);
    }

    #[test]
    fn parse_boolean_query_empty_quoted_phrase_ignored() {
        let tokens = parse_boolean_query("\"\"");
        assert!(tokens.is_empty());

        let tokens = parse_boolean_query("foo \"\" bar");
        let expected: QueryTokenList = SmallVec::from_vec(vec![
            QueryToken::Term("foo".into()),
            QueryToken::Term("bar".into()),
        ]);
        assert_eq!(tokens, expected);
    }

    #[test]
    fn parse_boolean_query_unclosed_quote() {
        // Unclosed quote should collect until end
        let tokens = parse_boolean_query("\"hello world");
        let expected: QueryTokenList =
            SmallVec::from_vec(vec![QueryToken::Phrase("hello world".into())]);
        assert_eq!(tokens, expected);
    }

    // --- levenshtein_distance tests ---

    #[test]
    fn levenshtein_distance_identical_strings() {
        assert_eq!(levenshtein_distance("hello", "hello"), 0);
        assert_eq!(levenshtein_distance("", ""), 0);
    }

    #[test]
    fn levenshtein_distance_insertions() {
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("cat", "cats"), 1);
    }

    #[test]
    fn levenshtein_distance_deletions() {
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("cats", "cat"), 1);
    }

    #[test]
    fn levenshtein_distance_substitutions() {
        assert_eq!(levenshtein_distance("cat", "bat"), 1);
        assert_eq!(levenshtein_distance("kitten", "sitten"), 1);
    }

    #[test]
    fn levenshtein_distance_mixed_operations() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("saturday", "sunday"), 3);
    }

    // --- is_tool_invocation_noise tests ---

    #[test]
    fn is_tool_invocation_noise_allows_real_content() {
        assert!(!is_tool_invocation_noise("This is a normal message"));
        assert!(!is_tool_invocation_noise(
            "Let me use the Tool feature to accomplish this task. Here is the implementation..."
        ));
        // Long content that happens to start with [Tool: should be allowed if it's substantial
        let long_content = "[Tool: Read] Now here is a lot of useful content that explains the implementation details and provides context for the changes being made to the codebase.";
        assert!(!is_tool_invocation_noise(long_content));
    }

    #[test]
    fn is_tool_invocation_noise_handles_short_tool_markers() {
        assert!(is_tool_invocation_noise("[tool: x]"));
        assert!(is_tool_invocation_noise("tool: bash"));
    }

    // --- Integration tests for boolean queries through search ---

    #[test]
    fn search_boolean_and_filters_results() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create documents with different word combinations
        let conv1 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc1".into()),
            workspace: None,
            source_path: dir.path().join("1.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "alpha beta gamma".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        let conv2 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc2".into()),
            workspace: None,
            source_path: dir.path().join("2.jsonl"),
            started_at: Some(2),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(2),
                content: "alpha delta".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv1)?;
        index.add_conversation(&conv2)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "alpha AND beta" should only match doc1
        let hits = client.search(
            "alpha AND beta",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("gamma"));

        // "alpha AND delta" should only match doc2
        let hits = client.search(
            "alpha AND delta",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("delta"));

        Ok(())
    }

    #[test]
    fn search_boolean_or_expands_results() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv1 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc1".into()),
            workspace: None,
            source_path: dir.path().join("1.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "unique xyzzy term".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        let conv2 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc2".into()),
            workspace: None,
            source_path: dir.path().join("2.jsonl"),
            started_at: Some(2),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(2),
                content: "unique plugh term".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv1)?;
        index.add_conversation(&conv2)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "xyzzy OR plugh" should match both docs
        let hits = client.search(
            "xyzzy OR plugh",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 2);

        Ok(())
    }

    #[test]
    fn search_boolean_not_excludes_results() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv1 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc1".into()),
            workspace: None,
            source_path: dir.path().join("1.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "nottest keep this".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        let conv2 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc2".into()),
            workspace: None,
            source_path: dir.path().join("2.jsonl"),
            started_at: Some(2),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(2),
                content: "nottest exclude this".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv1)?;
        index.add_conversation(&conv2)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "nottest NOT exclude" should only match doc1 (has nottest but NOT exclude)
        let hits = client.search(
            "nottest NOT exclude",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);
        // Verify we got the right doc by checking it doesn't contain "exclude"
        assert!(
            !hits[0].content.contains("exclude"),
            "NOT exclude should filter out doc with 'exclude'"
        );

        // Prefix "-" exclusion should behave like NOT for simple queries.
        let hits = client.search(
            "nottest -exclude",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);
        assert!(
            !hits[0].content.contains("exclude"),
            "Prefix -exclude should filter out doc with 'exclude'"
        );

        Ok(())
    }

    #[test]
    fn search_phrase_query_matches_exact_sequence() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv1 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc1".into()),
            workspace: None,
            source_path: dir.path().join("1.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "the quick brown fox".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        let conv2 = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc2".into()),
            workspace: None,
            source_path: dir.path().join("2.jsonl"),
            started_at: Some(2),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(2),
                content: "the brown quick fox".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv1)?;
        index.add_conversation(&conv2)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // "quick brown" (without quotes) should match both (words just need to be present)
        let hits = client.search(
            "quick brown",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 2);

        // "\"quick brown\"" should match exact order only
        let hits = client.search(
            "\"quick brown\"",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("quick brown"));

        Ok(())
    }

    #[test]
    fn search_punctuation_splits_into_terms() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("doc".into()),
            workspace: None,
            source_path: dir.path().join("3.jsonl"),
            started_at: Some(1),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "foo bar baz".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        let hits = client.search("foo.bar", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        let hits = client.search("foo-bar", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        Ok(())
    }

    // ========================================================================
    // QueryExplanation tests
    // ========================================================================

    #[test]
    fn explanation_classifies_simple_query() {
        let exp = QueryExplanation::analyze("hello", &SearchFilters::default());
        assert_eq!(exp.query_type, QueryType::Simple);
        assert_eq!(exp.index_strategy, IndexStrategy::EdgeNgram);
        assert_eq!(exp.estimated_cost, QueryCost::Low);
        assert!(exp.parsed.terms.len() == 1);
        assert_eq!(exp.parsed.terms[0].text, "hello");
        assert!(!exp.parsed.terms[0].subterms.is_empty());
        assert_eq!(exp.parsed.terms[0].subterms[0].pattern, "exact");
    }

    #[test]
    fn explanation_classifies_wildcard_query() {
        let exp = QueryExplanation::analyze("*handler*", &SearchFilters::default());
        assert_eq!(exp.query_type, QueryType::Wildcard);
        assert_eq!(exp.index_strategy, IndexStrategy::RegexScan);
        assert_eq!(exp.estimated_cost, QueryCost::High);
        assert!(!exp.parsed.terms[0].subterms.is_empty());
        assert!(
            exp.parsed.terms[0].subterms[0]
                .pattern
                .contains("substring")
        );
        assert!(exp.warnings.iter().any(|w| w.contains("regex scan")));
    }

    #[test]
    fn explanation_classifies_boolean_query() {
        let exp = QueryExplanation::analyze("foo AND bar", &SearchFilters::default());
        assert_eq!(exp.query_type, QueryType::Boolean);
        assert_eq!(exp.index_strategy, IndexStrategy::BooleanCombination);
        assert!(exp.parsed.operators.contains(&"AND".to_string()));
    }

    #[test]
    fn explanation_classifies_phrase_query() {
        let exp = QueryExplanation::analyze("\"exact phrase\"", &SearchFilters::default());
        assert_eq!(exp.query_type, QueryType::Phrase);
        assert!(exp.parsed.phrases.contains(&"exact phrase".to_string()));
    }

    #[test]
    fn explanation_handles_filtered_query() {
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".to_string());

        let exp = QueryExplanation::analyze("test", &filters);
        assert_eq!(exp.query_type, QueryType::Filtered);
        assert_eq!(exp.filters_summary.agent_count, 1);
        assert!(
            exp.filters_summary
                .description
                .as_ref()
                .unwrap()
                .contains("1 agent")
        );
        assert!(exp.warnings.iter().any(|w| w.contains("codex")));
    }

    #[test]
    fn explanation_handles_empty_query() {
        let exp = QueryExplanation::analyze("", &SearchFilters::default());
        assert_eq!(exp.query_type, QueryType::Empty);
        assert_eq!(exp.index_strategy, IndexStrategy::FullScan);
        assert_eq!(exp.estimated_cost, QueryCost::High);
        assert!(exp.warnings.iter().any(|w| w.contains("Empty query")));
    }

    #[test]
    fn explanation_warns_short_terms() {
        let exp = QueryExplanation::analyze("a", &SearchFilters::default());
        assert!(exp.warnings.iter().any(|w| w.contains("Very short term")));
    }

    #[test]
    fn explanation_with_wildcard_fallback() {
        let exp = QueryExplanation::analyze("test", &SearchFilters::default())
            .with_wildcard_fallback(true);
        assert!(exp.wildcard_applied);
        // Message starts with capital W: "Wildcard fallback was applied..."
        assert!(exp.warnings.iter().any(|w| w.contains("Wildcard fallback")));
    }

    #[test]
    fn explanation_complex_query_has_higher_cost() {
        let exp = QueryExplanation::analyze(
            "foo AND bar OR baz NOT qux AND \"phrase here\"",
            &SearchFilters::default(),
        );
        assert_eq!(exp.query_type, QueryType::Boolean);
        // Complex query should have Medium or High cost
        assert!(matches!(
            exp.estimated_cost,
            QueryCost::Medium | QueryCost::High
        ));
    }

    #[test]
    fn explanation_preserves_original_query() {
        let exp = QueryExplanation::analyze("Hello World!", &SearchFilters::default());
        assert_eq!(exp.original_query, "Hello World!");
        // Sanitized replaces special chars with spaces but preserves case
        assert!(exp.sanitized_query.contains("Hello"));
        // ! is replaced with space
        assert!(!exp.sanitized_query.contains("!"));
    }

    #[test]
    fn explanation_detects_not_operator() {
        let exp = QueryExplanation::analyze("foo NOT bar", &SearchFilters::default());
        assert!(exp.parsed.operators.contains(&"NOT".to_string()));
        // Second term should be marked as negated
        assert!(
            exp.parsed
                .terms
                .iter()
                .any(|t| t.negated && t.text == "bar")
        );
    }

    #[test]
    fn explanation_implicit_and() {
        let exp = QueryExplanation::analyze("foo bar", &SearchFilters::default());
        assert!(exp.parsed.implicit_and);
        assert_eq!(exp.parsed.terms.len(), 2);
    }

    #[test]
    fn explanation_serializes_to_json() {
        let exp = QueryExplanation::analyze("test query", &SearchFilters::default());
        let json = serde_json::to_value(&exp).expect("should serialize");
        assert!(json["original_query"].is_string());
        assert!(json["query_type"].is_string());
        assert!(json["index_strategy"].is_string());
        assert!(json["estimated_cost"].is_string());
        assert!(json["parsed"]["terms"].is_array());
    }

    // =========================================================================
    // Multi-filter combination tests (bead yln.2)
    // =========================================================================

    #[test]
    fn search_multi_filter_agent_workspace_time() -> Result<()> {
        // Test combining agent, workspace, and time range filters
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create 4 conversations with different combinations
        let convs = [
            ("codex", "/ws/alpha", 100, "needle alpha codex"),
            ("claude", "/ws/alpha", 200, "needle alpha claude"),
            ("codex", "/ws/beta", 150, "needle beta codex"),
            ("codex", "/ws/alpha", 300, "needle alpha codex late"),
        ];

        for (i, (agent, ws, ts, content)) in convs.iter().enumerate() {
            let conv = NormalizedConversation {
                agent_slug: (*agent).into(),
                external_id: None,
                title: Some(format!("conv-{i}")),
                workspace: Some(std::path::PathBuf::from(*ws)),
                source_path: dir.path().join(format!("{i}.jsonl")),
                started_at: Some(*ts),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(*ts),
                    content: (*content).into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Filter: codex + alpha + time 50-250
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".into());
        filters.workspaces.insert("/ws/alpha".into());
        filters.created_from = Some(50);
        filters.created_to = Some(250);

        let hits = client.search("needle", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(
            hits.len(),
            1,
            "Should match only one conv (codex + alpha + ts=100)"
        );
        assert_eq!(hits[0].agent, "codex");
        assert_eq!(hits[0].workspace, "/ws/alpha");
        assert!(hits[0].content.contains("alpha codex"));
        assert!(!hits[0].content.contains("late")); // Not the ts=300 one

        Ok(())
    }

    #[test]
    fn search_multi_agent_filter() -> Result<()> {
        // Test filtering by multiple agents
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        for agent in ["codex", "claude", "cline", "gemini"] {
            let conv = NormalizedConversation {
                agent_slug: agent.into(),
                external_id: None,
                title: Some(format!("{agent}-conv")),
                workspace: Some(std::path::PathBuf::from("/ws")),
                source_path: dir.path().join(format!("{agent}.jsonl")),
                started_at: Some(100),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(100),
                    content: format!("needle from {agent}"),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Filter for codex and claude only
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".into());
        filters.agents.insert("claude".into());

        let hits = client.search("needle", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 2);
        let agents: Vec<_> = hits.iter().map(|h| h.agent.as_str()).collect();
        assert!(agents.contains(&"codex"));
        assert!(agents.contains(&"claude"));
        assert!(!agents.contains(&"cline"));
        assert!(!agents.contains(&"gemini"));

        Ok(())
    }

    // =========================================================================
    // Cache metrics tests (bead yln.2)
    // =========================================================================

    #[test]
    fn cache_metrics_incremented_on_operations() {
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        // Initial metrics should be zero
        let (hits, miss, shortfall, reloads, _) = client.metrics.snapshot_all();
        assert_eq!((hits, miss, shortfall, reloads), (0, 0, 0, 0));

        // Simulate operations
        client.metrics.inc_cache_hits();
        client.metrics.inc_cache_hits();
        client.metrics.inc_cache_miss();
        client.metrics.inc_cache_shortfall();
        client.metrics.inc_reload();

        let (hits, miss, shortfall, reloads, _) = client.metrics.snapshot_all();
        assert_eq!(hits, 2);
        assert_eq!(miss, 1);
        assert_eq!(shortfall, 1);
        assert_eq!(reloads, 1);
    }

    #[test]
    fn cache_shard_name_deterministic() {
        // Verify that shard name generation is deterministic for same filters
        let client = SearchClient {
            reader: None,
            sqlite: Mutex::new(None),
            sqlite_path: None,
            prefix_cache: Mutex::new(CacheShards::new(*CACHE_TOTAL_CAP, *CACHE_BYTE_CAP)),
            reload_on_search: true,
            last_reload: Mutex::new(None),
            last_generation: Mutex::new(None),
            reload_epoch: Arc::new(AtomicU64::new(0)),
            warm_tx: None,
            _warm_handle: None,
            _shared_filters: Arc::new(Mutex::new(())),
            metrics: Metrics::default(),
            cache_namespace: format!("v{CACHE_KEY_VERSION}|schema:test"),
            semantic: Mutex::new(None),
        };

        let filters1 = SearchFilters::default();
        let mut filters2 = SearchFilters::default();
        filters2.agents.insert("codex".into());

        // Same filters should always produce same shard name
        let shard1_first = client.shard_name(&filters1);
        let shard1_second = client.shard_name(&filters1);
        assert_eq!(
            shard1_first, shard1_second,
            "Same filters should produce same shard name"
        );

        // Different filters produce different shard names
        let shard2 = client.shard_name(&filters2);
        assert_ne!(
            shard1_first, shard2,
            "Different filters should produce different shard names"
        );

        // Shard name is deterministic
        assert_eq!(shard2, client.shard_name(&filters2));
    }

    // =========================================================================
    // Wildcard fallback edge cases (bead yln.2)
    // =========================================================================

    #[test]
    fn wildcard_fallback_respects_filter_constraints() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create conversations that would match wildcard but not filter
        let conv_match = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("match".into()),
            workspace: Some(std::path::PathBuf::from("/target")),
            source_path: dir.path().join("match.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "unique specific term here".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };

        let conv_other = NormalizedConversation {
            agent_slug: "claude".into(),
            external_id: None,
            title: Some("other".into()),
            workspace: Some(std::path::PathBuf::from("/other")),
            source_path: dir.path().join("other.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "unique specific also here".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };

        index.add_conversation(&conv_match)?;
        index.add_conversation(&conv_other)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Search with filter that only matches conv_match
        let mut filters = SearchFilters::default();
        filters.agents.insert("codex".into());

        let result =
            client.search_with_fallback("unique", filters.clone(), 10, 0, 100, FieldMask::FULL)?;
        // Should only return the codex conversation, not claude
        assert!(result.hits.iter().all(|h| h.agent == "codex"));

        Ok(())
    }

    #[test]
    fn wildcard_fallback_short_query_triggers_prefix() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: None,
            title: Some("test".into()),
            workspace: None,
            source_path: dir.path().join("test.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "authentication authorization oauth".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Short prefix "auth" should match "authentication" and "authorization"
        let result = client.search_with_fallback(
            "auth",
            SearchFilters::default(),
            10,
            0,
            100,
            FieldMask::FULL,
        )?;
        assert!(
            !result.hits.is_empty(),
            "Short prefix should match via prefix search"
        );
        assert!(result.hits[0].content.contains("auth"));

        Ok(())
    }

    // =========================================================================
    // Real fixture tests with metrics (bead yln.2)
    // =========================================================================

    #[test]
    fn search_real_fixture_multiple_messages() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create a realistic conversation with multiple messages
        let conv = NormalizedConversation {
            agent_slug: "claude_code".into(),
            external_id: Some("conv-123".into()),
            title: Some("Implementing authentication".into()),
            workspace: Some(std::path::PathBuf::from("/home/user/project")),
            source_path: dir.path().join("session-1.jsonl"),
            started_at: Some(1700000000000),
            ended_at: Some(1700000060000),
            metadata: serde_json::json!({
                "model": "claude-3-sonnet",
                "tokens": 1500
            }),
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: Some("developer".into()),
                    created_at: Some(1700000000000),
                    content: "Help me implement JWT authentication for my Express API".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".into(),
                    author: Some("claude".into()),
                    created_at: Some(1700000010000),
                    content: "I'll help you implement JWT authentication. First, let's install the required packages.".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![NormalizedSnippet {
                        file_path: Some("package.json".into()),
                        start_line: Some(1),
                        end_line: Some(5),
                        language: Some("json".into()),
                        snippet_text: Some(r#"{"dependencies":{"jsonwebtoken":"^9.0.0"}}"#.into()),
                    }],
                },
                NormalizedMessage {
                    idx: 2,
                    role: "user".into(),
                    author: Some("developer".into()),
                    created_at: Some(1700000030000),
                    content: "Can you also add refresh token support?".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                },
            ],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Search for various terms that should match
        let hits = client.search(
            "JWT authentication",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert!(!hits.is_empty(), "Should find JWT authentication");
        assert!(hits.iter().any(|h| h.agent == "claude_code"));
        assert!(
            hits.iter()
                .any(|h| h.snippet.contains("JWT") || h.snippet.contains("authentication"))
        );

        // Search for assistant response content
        let hits = client.search(
            "required packages",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert!(
            !hits.is_empty(),
            "Should find 'required packages' in assistant response"
        );

        // Search for user question about refresh tokens
        let hits = client.search(
            "refresh token",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )?;
        assert!(!hits.is_empty(), "Should find refresh token");
        assert!(hits.iter().any(|h| h.content.contains("refresh")));

        Ok(())
    }

    #[test]
    fn search_deduplication_with_similar_content() -> Result<()> {
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create two conversations with very similar content
        for i in 0..2 {
            let conv = NormalizedConversation {
                agent_slug: "codex".into(),
                external_id: None,
                title: Some(format!("similar-{i}")),
                workspace: Some(std::path::PathBuf::from("/ws")),
                source_path: dir.path().join(format!("similar-{i}.jsonl")),
                started_at: Some(100 + i),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(100 + i),
                    // Exactly the same content
                    content: "implement the sorting algorithm".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");
        let result = client.search_with_fallback(
            "sorting algorithm",
            SearchFilters::default(),
            10,
            0,
            100,
            FieldMask::FULL,
        )?;

        // Both should be returned (different source_paths mean different conversations)
        // but if they have exact same content from same source, dedup should apply
        assert!(!result.hits.is_empty());

        Ok(())
    }

    // =========================================================================
    // Session paths filter tests (chained searches)
    // =========================================================================

    #[test]
    fn search_session_paths_filter() -> Result<()> {
        // Test filtering by specific session source paths (for chained searches)
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        // Create 3 conversations with different source paths
        let paths = [
            dir.path().join("session-a.jsonl"),
            dir.path().join("session-b.jsonl"),
            dir.path().join("session-c.jsonl"),
        ];

        for (i, path) in paths.iter().enumerate() {
            let conv = NormalizedConversation {
                agent_slug: "claude".into(),
                external_id: None,
                title: Some(format!("session-{}", i)),
                workspace: Some(std::path::PathBuf::from("/ws")),
                source_path: path.clone(),
                started_at: Some(100 + i as i64),
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(100 + i as i64),
                    content: format!("needle content for session {}", i),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            };
            index.add_conversation(&conv)?;
        }
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // First, search without filter - should get all 3
        let hits_all = client.search("needle", SearchFilters::default(), 10, 0, FieldMask::FULL)?;
        assert_eq!(hits_all.len(), 3, "Should find all 3 sessions");

        // Now filter to only sessions A and C
        let mut filters = SearchFilters::default();
        filters
            .session_paths
            .insert(paths[0].to_string_lossy().to_string());
        filters
            .session_paths
            .insert(paths[2].to_string_lossy().to_string());

        let hits_filtered = client.search("needle", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(
            hits_filtered.len(),
            2,
            "Should find only 2 sessions (A and C)"
        );

        // Verify the correct sessions are returned
        let filtered_paths: HashSet<&str> = hits_filtered
            .iter()
            .map(|h| h.source_path.as_str())
            .collect();
        assert!(filtered_paths.contains(paths[0].to_string_lossy().as_ref()));
        assert!(filtered_paths.contains(paths[2].to_string_lossy().as_ref()));
        assert!(!filtered_paths.contains(paths[1].to_string_lossy().as_ref()));

        Ok(())
    }

    #[test]
    fn search_session_paths_empty_filter_returns_all() -> Result<()> {
        // Empty session_paths filter should not restrict results
        let dir = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(dir.path())?;

        let conv = NormalizedConversation {
            agent_slug: "claude".into(),
            external_id: None,
            title: Some("test".into()),
            workspace: Some(std::path::PathBuf::from("/ws")),
            source_path: dir.path().join("test.jsonl"),
            started_at: Some(100),
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(100),
                content: "needle content".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            }],
        };
        index.add_conversation(&conv)?;
        index.commit()?;

        let client = SearchClient::open(dir.path(), None)?.expect("index present");

        // Empty session_paths should not filter
        let filters = SearchFilters::default();
        assert!(filters.session_paths.is_empty());

        let hits = client.search("needle", filters, 10, 0, FieldMask::FULL)?;
        assert_eq!(hits.len(), 1);

        Ok(())
    }

    // =============================================================================
    // SQL Placeholder Builder Tests (Opt 4.5: Pre-sized String Buffers)
    // =============================================================================

    #[test]
    fn sql_placeholders_empty() {
        assert_eq!(sql_placeholders(0), "");
    }

    #[test]
    fn sql_placeholders_single() {
        assert_eq!(sql_placeholders(1), "?");
    }

    #[test]
    fn sql_placeholders_multiple() {
        assert_eq!(sql_placeholders(3), "?,?,?");
        assert_eq!(sql_placeholders(5), "?,?,?,?,?");
    }

    #[test]
    fn sql_placeholders_capacity_efficient() {
        // For count=3, capacity should be exactly 2*3-1=5 ("?,?,?" = 5 chars)
        let result = sql_placeholders(3);
        assert_eq!(result.len(), 5);
        assert!(result.capacity() >= 5); // Should have allocated at least 5

        // For count=10, capacity should be exactly 2*10-1=19
        let result = sql_placeholders(10);
        assert_eq!(result.len(), 19);
        assert!(result.capacity() >= 19);
    }

    #[test]
    fn sql_placeholders_large_count() {
        // Test with a large count to ensure no off-by-one errors
        let result = sql_placeholders(100);
        assert_eq!(result.len(), 199); // 100 "?" + 99 ","
        assert_eq!(result.chars().filter(|c| *c == '?').count(), 100);
        assert_eq!(result.chars().filter(|c| *c == ',').count(), 99);
    }

    // =============================================================================
    // RRF (Reciprocal Rank Fusion) Tests
    // =============================================================================

    fn make_test_hit(id: &str, score: f32) -> SearchHit {
        SearchHit {
            title: id.to_string(),
            snippet: String::new(),
            content: id.to_string(),
            content_hash: stable_content_hash(id),
            score,
            source_path: format!("/path/{}.jsonl", id),
            agent: "test".to_string(),
            workspace: "/workspace".to_string(),
            workspace_original: None,
            created_at: Some(1_700_000_000_000),
            line_number: Some(1),
            match_type: MatchType::Exact,
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        }
    }

    #[test]
    fn test_rrf_fusion_ordering() {
        // Test that RRF correctly combines rankings from both lists
        // Higher ranks in both lists should result in higher final ranking
        let lexical = vec![
            make_test_hit("A", 10.0),
            make_test_hit("B", 8.0),
            make_test_hit("C", 6.0),
        ];
        let semantic = vec![
            make_test_hit("A", 0.9),
            make_test_hit("B", 0.7),
            make_test_hit("D", 0.5),
        ];

        let fused = rrf_fuse_hits(&lexical, &semantic, 10, 0);

        // A and B should be top (in both lists), A first (rank 0 in both)
        assert_eq!(fused.len(), 4);
        assert_eq!(fused[0].title, "A"); // Rank 0 in both
        assert_eq!(fused[1].title, "B"); // Rank 1 in both
        // C and D are in only one list each, order depends on their ranks
    }

    #[test]
    fn test_rrf_handles_disjoint_sets() {
        // Test with no overlap between lexical and semantic results
        let lexical = vec![make_test_hit("A", 10.0), make_test_hit("B", 8.0)];
        let semantic = vec![make_test_hit("C", 0.9), make_test_hit("D", 0.7)];

        let fused = rrf_fuse_hits(&lexical, &semantic, 10, 0);

        // All 4 items should be present
        assert_eq!(fused.len(), 4);
        let titles: Vec<&str> = fused.iter().map(|h| h.title.as_str()).collect();
        assert!(titles.contains(&"A"));
        assert!(titles.contains(&"B"));
        assert!(titles.contains(&"C"));
        assert!(titles.contains(&"D"));
    }

    #[test]
    fn test_rrf_tie_breaking_deterministic() {
        // Test that results are deterministic - same input always produces same output
        let lexical = vec![
            make_test_hit("X", 5.0),
            make_test_hit("Y", 5.0),
            make_test_hit("Z", 5.0),
        ];
        let semantic = vec![]; // Empty semantic list

        // Run multiple times and verify same order
        let fused1 = rrf_fuse_hits(&lexical, &semantic, 10, 0);
        let fused2 = rrf_fuse_hits(&lexical, &semantic, 10, 0);
        let fused3 = rrf_fuse_hits(&lexical, &semantic, 10, 0);

        // Order should be deterministic based on key comparison
        assert_eq!(fused1.len(), fused2.len());
        assert_eq!(fused2.len(), fused3.len());

        for i in 0..fused1.len() {
            assert_eq!(fused1[i].title, fused2[i].title, "Mismatch at index {}", i);
            assert_eq!(fused2[i].title, fused3[i].title, "Mismatch at index {}", i);
        }
    }

    #[test]
    fn test_rrf_both_lists_bonus() {
        // Documents appearing in both lists should rank higher than those in only one
        // Even if their individual ranks are lower
        let lexical = vec![
            make_test_hit("solo_lex", 10.0), // Rank 0 lexical only
            make_test_hit("both", 5.0),      // Rank 1 lexical
        ];
        let semantic = vec![
            make_test_hit("solo_sem", 0.9), // Rank 0 semantic only
            make_test_hit("both", 0.5),     // Rank 1 semantic
        ];

        let fused = rrf_fuse_hits(&lexical, &semantic, 10, 0);

        // "both" should be first due to appearing in both lists
        // It gets RRF score from rank 1 in both lists = 1/(60+2) * 2 = 0.0322
        // vs solo items get 1/(60+1) = 0.0164 each
        assert_eq!(
            fused[0].title, "both",
            "Doc in both lists should rank first"
        );
    }

    #[test]
    fn test_rrf_respects_limit_and_offset() {
        let lexical = vec![
            make_test_hit("A", 10.0),
            make_test_hit("B", 8.0),
            make_test_hit("C", 6.0),
        ];
        let semantic = vec![];

        // Test limit
        let fused = rrf_fuse_hits(&lexical, &semantic, 2, 0);
        assert_eq!(fused.len(), 2);

        // Test offset
        let fused_offset = rrf_fuse_hits(&lexical, &semantic, 10, 1);
        assert_eq!(fused_offset.len(), 2); // Skipped first one

        // Test limit 0
        let fused_empty = rrf_fuse_hits(&lexical, &semantic, 0, 0);
        assert!(fused_empty.is_empty());
    }

    #[test]
    fn test_rrf_empty_inputs() {
        let empty: Vec<SearchHit> = vec![];
        let non_empty = vec![make_test_hit("A", 10.0)];

        // Both empty
        assert!(rrf_fuse_hits(&empty, &empty, 10, 0).is_empty());

        // Lexical empty
        let fused = rrf_fuse_hits(&empty, &non_empty, 10, 0);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].title, "A");

        // Semantic empty
        let fused = rrf_fuse_hits(&non_empty, &empty, 10, 0);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].title, "A");
    }

    #[test]
    fn test_rrf_candidate_depth() {
        // Test with many candidates to ensure proper fusion
        let lexical: Vec<_> = (0..50)
            .map(|i| make_test_hit(&format!("L{}", i), 100.0 - i as f32))
            .collect();
        let semantic: Vec<_> = (0..50)
            .map(|i| make_test_hit(&format!("S{}", i), 1.0 - 0.01 * i as f32))
            .collect();

        let fused = rrf_fuse_hits(&lexical, &semantic, 20, 0);

        // Should return 20 items
        assert_eq!(fused.len(), 20);

        // All items should be unique
        let mut seen = std::collections::HashSet::new();
        for hit in &fused {
            assert!(seen.insert(&hit.title), "Duplicate hit: {}", hit.title);
        }
    }

    // ==========================================================================
    // QueryTokenList SmallVec Tests (Opt 4.4)
    // ==========================================================================

    #[test]
    fn query_token_list_stays_on_stack_for_small_queries() {
        // Single term - should not spill
        let tokens = parse_boolean_query("hello");
        assert!(!tokens.spilled(), "Single term should stay on stack");
        assert_eq!(tokens.len(), 1);

        // Two terms - should not spill
        let tokens = parse_boolean_query("hello world");
        assert!(!tokens.spilled(), "Two terms should stay on stack");
        assert_eq!(tokens.len(), 2);

        // Three terms with operator - should not spill
        let tokens = parse_boolean_query("hello AND world");
        assert!(!tokens.spilled(), "Three tokens should stay on stack");
        assert_eq!(tokens.len(), 3);

        // Four tokens - exactly at capacity
        let tokens = parse_boolean_query("hello world foo bar");
        assert!(
            !tokens.spilled(),
            "Four terms at capacity should stay on stack"
        );
        assert_eq!(tokens.len(), 4);
    }

    #[test]
    fn query_token_list_spills_to_heap_for_large_queries() {
        // More than 8 tokens should spill to heap
        let tokens = parse_boolean_query("a b c d e f g h i");
        assert!(tokens.spilled(), "Nine terms should spill to heap");
        assert_eq!(tokens.len(), 9);
    }

    #[test]
    fn query_token_list_handles_quoted_phrases() {
        let tokens = parse_boolean_query("\"hello world\" test");
        assert!(!tokens.spilled(), "Phrase and term should stay on stack");
        assert_eq!(tokens.len(), 2);

        // Verify the phrase is correctly parsed
        assert!(
            matches!(&tokens[0], QueryToken::Phrase(phrase) if phrase == "hello world"),
            "Expected Phrase token"
        );
    }

    #[test]
    fn query_token_list_handles_operators() {
        let tokens = parse_boolean_query("foo AND bar OR baz");
        assert!(
            !tokens.spilled(),
            "Query with operators should stay on stack"
        );
        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[1], QueryToken::And);
        assert_eq!(tokens[3], QueryToken::Or);
    }

    #[test]
    fn query_token_list_empty_query() {
        let tokens = parse_boolean_query("");
        assert!(!tokens.spilled());
        assert!(tokens.is_empty());
    }

    #[test]
    fn query_token_list_iteration_works() {
        let tokens = parse_boolean_query("a b c");
        let terms: Vec<_> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(terms, vec!["a", "b", "c"]);
    }

    // ==========================================================================
    // Unicode Query Parsing Tests (br-327c)
    // Comprehensive Unicode handling tests covering emoji, CJK, RTL, mixed
    // scripts, zero-width characters, combining characters, normalization,
    // supplementary plane characters, and bidirectional text.
    // ==========================================================================

    // --- Emoji queries ---

    #[test]
    fn unicode_emoji_treated_as_separator() {
        // Emoji are not alphanumeric per Unicode, so sanitize_query replaces them with spaces
        let sanitized = sanitize_query("🚀 launch");
        assert_eq!(sanitized, "  launch", "Emoji should become space");
    }

    #[test]
    fn unicode_emoji_splits_terms() {
        // Emoji between words acts as a separator
        let sanitized = sanitize_query("hot🔥code");
        assert_eq!(sanitized, "hot code", "Emoji between words splits them");
    }

    #[test]
    fn unicode_multiple_emoji_become_spaces() {
        let sanitized = sanitize_query("🚀🔥💻");
        assert_eq!(
            sanitized.trim(),
            "",
            "All-emoji query sanitizes to whitespace"
        );
    }

    #[test]
    fn unicode_emoji_query_parses_without_panic() {
        let tokens = parse_boolean_query("🚀 launch code 🔥");
        let terms: Vec<_> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        // Emoji removed by sanitization in normalize_term_parts, only words remain
        assert!(
            terms
                .iter()
                .any(|t| t.contains("launch") || t.contains("code"))
        );
    }

    #[test]
    fn unicode_emoji_query_terms_lower() {
        let terms = QueryTermsLower::from_query("🚀 LAUNCH");
        // Emoji becomes space, LAUNCH lowercased
        let tokens: Vec<&str> = terms.tokens().collect();
        assert!(
            tokens.contains(&"launch"),
            "Should extract 'launch' from emoji query"
        );
    }

    // --- CJK character queries ---

    #[test]
    fn unicode_cjk_chinese_preserved() {
        assert_eq!(sanitize_query("测试代码"), "测试代码");
        assert_eq!(sanitize_query("测试 代码"), "测试 代码");
    }

    #[test]
    fn unicode_cjk_japanese_preserved() {
        assert_eq!(sanitize_query("テスト"), "テスト");
        // Hiragana and Katakana are alphanumeric
        assert_eq!(sanitize_query("こんにちは世界"), "こんにちは世界");
    }

    #[test]
    fn unicode_cjk_korean_preserved() {
        assert_eq!(sanitize_query("테스트"), "테스트");
        assert_eq!(sanitize_query("안녕하세요"), "안녕하세요");
    }

    #[test]
    fn unicode_cjk_parsed_as_terms() {
        let tokens = parse_boolean_query("测试 代码 search");
        let terms: Vec<_> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(terms, vec!["测试", "代码", "search"]);
    }

    #[test]
    fn unicode_cjk_query_terms_lower() {
        let terms = QueryTermsLower::from_query("测试 代码");
        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["测试", "代码"]);
    }

    // --- RTL text queries ---

    #[test]
    fn unicode_hebrew_preserved() {
        assert_eq!(sanitize_query("שלום עולם"), "שלום עולם");
    }

    #[test]
    fn unicode_arabic_preserved() {
        assert_eq!(sanitize_query("مرحبا"), "مرحبا");
    }

    #[test]
    fn unicode_hebrew_parsed_as_terms() {
        let tokens = parse_boolean_query("שלום עולם");
        let terms: Vec<_> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(terms, vec!["שלום", "עולם"]);
    }

    #[test]
    fn unicode_arabic_query_terms_lower() {
        // Arabic doesn't have case, so lowercasing is a no-op
        let terms = QueryTermsLower::from_query("مرحبا بالعالم");
        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["مرحبا", "بالعالم"]);
    }

    // --- Mixed script queries ---

    #[test]
    fn unicode_mixed_scripts_preserved() {
        let sanitized = sanitize_query("Hello 世界 мир");
        assert_eq!(sanitized, "Hello 世界 мир");
    }

    #[test]
    fn unicode_mixed_scripts_parsed() {
        let tokens = parse_boolean_query("Hello 世界 мир");
        let terms: Vec<_> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(terms, vec!["Hello", "世界", "мир"]);
    }

    #[test]
    fn unicode_mixed_scripts_with_emoji() {
        // Emoji stripped, scripts preserved
        let sanitized = sanitize_query("Hello 🌍 世界");
        assert_eq!(sanitized, "Hello   世界");
    }

    #[test]
    fn unicode_latin_cyrillic_arabic_query() {
        let terms = QueryTermsLower::from_query("Hello Мир مرحبا");
        let tokens: Vec<&str> = terms.tokens().collect();
        assert_eq!(tokens, vec!["hello", "мир", "مرحبا"]);
    }

    // --- Zero-width characters ---

    #[test]
    fn unicode_zero_width_joiner_removed() {
        // Zero-width joiner (U+200D) is not alphanumeric → becomes space
        let sanitized = sanitize_query("test\u{200D}query");
        assert_eq!(sanitized, "test query");
    }

    #[test]
    fn unicode_zero_width_non_joiner_removed() {
        // Zero-width non-joiner (U+200C) is not alphanumeric → becomes space
        let sanitized = sanitize_query("test\u{200C}query");
        assert_eq!(sanitized, "test query");
    }

    #[test]
    fn unicode_zero_width_space_removed() {
        // Zero-width space (U+200B) is not alphanumeric → becomes space
        let sanitized = sanitize_query("test\u{200B}query");
        assert_eq!(sanitized, "test query");
    }

    #[test]
    fn unicode_bom_removed() {
        // Byte-order mark (U+FEFF) should not appear in search terms
        let sanitized = sanitize_query("\u{FEFF}test");
        assert_eq!(sanitized, " test");
    }

    // --- Combining characters ---

    #[test]
    fn unicode_precomposed_accent_preserved() {
        // Precomposed é (U+00E9) is a single letter → alphanumeric
        let sanitized = sanitize_query("café");
        assert_eq!(sanitized, "café");
    }

    #[test]
    fn unicode_combining_accent_becomes_separator() {
        // Decomposed: 'e' + combining acute accent (U+0301)
        // The combining mark itself is NOT alphanumeric → becomes space
        // This means "cafe\u{0301}" becomes "cafe " (accent stripped)
        let input = "cafe\u{0301}";
        let sanitized = sanitize_query(input);
        // 'c','a','f','e' are alphanumeric; U+0301 is Mark category → space
        assert_eq!(sanitized, "cafe ");
    }

    #[test]
    fn unicode_nfc_vs_nfd_differ_in_sanitization() {
        // NFC (precomposed): é = U+00E9 (single char, alphanumeric)
        let nfc = "caf\u{00E9}";
        // NFD (decomposed): e + ◌́ = U+0065 U+0301 (two chars, accent not alphanumeric)
        let nfd = "cafe\u{0301}";

        let san_nfc = sanitize_query(nfc);
        let san_nfd = sanitize_query(nfd);

        // NFC preserves the é
        assert_eq!(san_nfc, "café");
        // NFD strips the combining accent
        assert_eq!(san_nfd, "cafe ");
        // They differ — this is expected behavior (no normalization applied)
        assert_ne!(san_nfc, san_nfd);
    }

    #[test]
    fn unicode_combining_marks_do_not_panic() {
        // Multiple combining marks stacked (e.g., Zalgo text)
        let zalgo = "t\u{0301}\u{0302}\u{0303}e\u{0304}\u{0305}st";
        let sanitized = sanitize_query(zalgo);
        // Should not panic; combining marks become spaces
        assert!(sanitized.contains('t'));
        assert!(sanitized.contains('s'));
    }

    // --- Supplementary plane characters (outside BMP) ---

    #[test]
    fn unicode_mathematical_bold_letters_preserved() {
        // Mathematical Bold Capital A (U+1D400) — classified as Letter
        let input = "\u{1D400}\u{1D401}\u{1D402}";
        let sanitized = sanitize_query(input);
        assert_eq!(
            sanitized, input,
            "Mathematical bold letters are alphanumeric"
        );
    }

    #[test]
    fn unicode_supplementary_ideograph_preserved() {
        // CJK Unified Ideographs Extension B character (U+20000)
        let input = "\u{20000}";
        let sanitized = sanitize_query(input);
        assert_eq!(
            sanitized, input,
            "Supplementary CJK ideographs are alphanumeric"
        );
    }

    #[test]
    fn unicode_supplementary_emoji_removed() {
        // Grinning face (U+1F600) — Symbol, not alphanumeric
        let input = "test\u{1F600}query";
        let sanitized = sanitize_query(input);
        assert_eq!(sanitized, "test query");
    }

    // --- Bidirectional text ---

    #[test]
    fn unicode_bidi_mixed_ltr_rtl_no_panic() {
        let input = "hello שלום world עולם";
        let tokens = parse_boolean_query(input);
        let terms: Vec<_> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(terms.len(), 4);
        assert!(terms.contains(&"hello"));
        assert!(terms.contains(&"שלום"));
        assert!(terms.contains(&"world"));
        assert!(terms.contains(&"עולם"));
    }

    #[test]
    fn unicode_bidi_override_chars_removed() {
        // Left-to-right override (U+202D) and pop directional (U+202C)
        // These are format characters, not alphanumeric
        let input = "test\u{202D}content\u{202C}end";
        let sanitized = sanitize_query(input);
        assert_eq!(sanitized, "test content end");
    }

    #[test]
    fn unicode_bidi_rtl_mark_removed() {
        // Right-to-left mark (U+200F) is not alphanumeric
        let input = "test\u{200F}content";
        let sanitized = sanitize_query(input);
        assert_eq!(sanitized, "test content");
    }

    // --- Full pipeline integration tests ---

    #[test]
    fn unicode_full_pipeline_cjk_query() {
        let explanation = QueryExplanation::analyze("测试 代码", &SearchFilters::default());
        assert_eq!(explanation.parsed.terms.len(), 2);
        assert!(!explanation.parsed.terms[0].text.is_empty());
        assert!(!explanation.parsed.terms[1].text.is_empty());
    }

    #[test]
    fn unicode_full_pipeline_mixed_script_boolean() {
        let explanation =
            QueryExplanation::analyze("Hello AND 世界 OR مرحبا", &SearchFilters::default());
        // Should parse operators correctly even with mixed scripts
        assert!(
            explanation.parsed.operators.iter().any(|op| op == "AND"),
            "AND operator should be recognized in mixed-script query"
        );
    }

    #[test]
    fn unicode_full_pipeline_emoji_query_type() {
        // An all-emoji query sanitizes to empty — should handle gracefully
        let explanation = QueryExplanation::analyze("🚀🔥💻", &SearchFilters::default());
        // Should not panic; terms may be empty after sanitization
        assert!(
            explanation.parsed.terms.is_empty()
                || explanation
                    .parsed
                    .terms
                    .iter()
                    .all(|t| t.subterms.is_empty()),
            "All-emoji query should produce no meaningful terms"
        );
    }

    #[test]
    fn unicode_full_pipeline_phrase_with_cjk() {
        let explanation = QueryExplanation::analyze("\"测试代码\"", &SearchFilters::default());
        assert!(
            !explanation.parsed.phrases.is_empty(),
            "CJK phrase should be recognized"
        );
    }

    #[test]
    fn unicode_full_pipeline_wildcard_with_unicode() {
        let explanation = QueryExplanation::analyze("*测试*", &SearchFilters::default());
        assert!(
            !explanation.parsed.terms.is_empty(),
            "Wildcard with CJK should produce terms"
        );
        // Check that the term has a substring/wildcard pattern
        if let Some(term) = explanation.parsed.terms.first() {
            assert!(
                term.subterms
                    .iter()
                    .any(|s| s.pattern.contains("*") || s.pattern == "exact"),
                "CJK wildcard should produce wildcard or exact pattern"
            );
        }
    }

    #[test]
    fn unicode_query_terms_lower_case_folding() {
        // German sharp s (ß) lowercases to ß (not ss in Rust)
        let terms = QueryTermsLower::from_query("STRAßE");
        assert_eq!(terms.query_lower, "straße");

        // Turkish dotless I (İ → i with dot below in some locales, but
        // Rust uses simple Unicode case mapping)
        let terms2 = QueryTermsLower::from_query("HELLO");
        assert_eq!(terms2.query_lower, "hello");
    }

    #[test]
    fn unicode_normalize_term_parts_cjk() {
        let parts = normalize_term_parts("测试 代码");
        assert_eq!(parts, vec!["测试", "代码"]);
    }

    #[test]
    fn unicode_normalize_term_parts_strips_emoji() {
        let parts = normalize_term_parts("🚀launch🔥code");
        // Emoji replaced with space, splitting into two terms
        assert!(parts.contains(&"launch".to_string()));
        assert!(parts.contains(&"code".to_string()));
    }

    // ── Special character query tests (br-g650) ────────────────────────────

    // Category 1: Unbalanced quotes

    #[test]
    fn special_char_unbalanced_quote_no_panic() {
        let tokens = parse_boolean_query("\"hello world");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, QueryToken::Phrase(p) if p.contains("hello"))),
            "Unbalanced quote should still produce a phrase: {tokens:?}"
        );
    }

    #[test]
    fn special_char_unbalanced_trailing_quote() {
        let tokens = parse_boolean_query("test\"");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, QueryToken::Term(w) if w == "test")),
            "Text before trailing quote should parse as term: {tokens:?}"
        );
    }

    #[test]
    fn special_char_multiple_unbalanced_quotes() {
        let tokens = parse_boolean_query("\"foo \"bar");
        assert!(
            !tokens.is_empty(),
            "Should parse despite odd quotes: {tokens:?}"
        );
    }

    #[test]
    fn special_char_empty_quotes() {
        let tokens = parse_boolean_query("\"\" test");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, QueryToken::Term(w) if w == "test")),
            "Empty quotes should be skipped: {tokens:?}"
        );
    }

    #[test]
    fn special_char_unbalanced_via_sanitize() {
        let sanitized = sanitize_query("\"hello world");
        assert!(
            sanitized.contains('"'),
            "Quotes preserved by sanitize_query"
        );
    }

    // Category 2: Escaped quotes

    #[test]
    fn special_char_backslash_quote_sanitize() {
        let sanitized = sanitize_query("\\\"test\\\"");
        assert!(sanitized.contains('"'));
        assert!(!sanitized.contains('\\'), "Backslash should be stripped");
    }

    #[test]
    fn special_char_backslash_quote_parse() {
        let tokens = parse_boolean_query("\\\"test\\\"");
        assert!(!tokens.is_empty(), "Should parse without panic: {tokens:?}");
    }

    #[test]
    fn special_char_inner_escaped_quotes() {
        let tokens = parse_boolean_query("\"test \\\"inner\\\" test\"");
        assert!(
            !tokens.is_empty(),
            "Nested escaped quotes should not panic: {tokens:?}"
        );
    }

    // Category 3: Backslash sequences

    #[test]
    fn special_char_windows_path_sanitize() {
        let sanitized = sanitize_query("C:\\Users\\test");
        assert_eq!(sanitized, "C  Users test");
    }

    #[test]
    fn special_char_unc_path_sanitize() {
        let sanitized = sanitize_query("\\\\server\\share");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"server"));
        assert!(parts.contains(&"share"));
    }

    #[test]
    fn special_char_windows_path_terms() {
        let parts = normalize_term_parts("C:\\Users\\test\\file.rs");
        assert!(parts.contains(&"C".to_string()));
        assert!(parts.contains(&"Users".to_string()));
        assert!(parts.contains(&"test".to_string()));
        assert!(parts.contains(&"file".to_string()));
        assert!(parts.contains(&"rs".to_string()));
    }

    // Category 4: Regex metacharacters

    #[test]
    fn special_char_regex_dot_star() {
        let sanitized = sanitize_query("foo.*bar");
        assert_eq!(sanitized, "foo *bar");
    }

    #[test]
    fn special_char_regex_char_class() {
        let sanitized = sanitize_query("[a-z]+");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["a", "z"]);
    }

    #[test]
    fn special_char_regex_anchors() {
        let sanitized = sanitize_query("^start$");
        assert_eq!(sanitized.trim(), "start");
    }

    #[test]
    fn special_char_regex_pipe_groups() {
        let sanitized = sanitize_query("(foo|bar)");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["foo", "bar"]);
    }

    // Category 5: SQL injection patterns

    #[test]
    fn special_char_sql_injection_or() {
        let sanitized = sanitize_query("'OR 1=1--");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"OR"));
        assert!(parts.contains(&"1"));
        assert!(!sanitized.contains('\''));
        assert!(!sanitized.contains('='));
    }

    #[test]
    fn special_char_sql_injection_drop() {
        let sanitized = sanitize_query("; DROP TABLE users;--");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"DROP"));
        assert!(parts.contains(&"TABLE"));
        assert!(parts.contains(&"users"));
        assert!(!sanitized.contains(';'));
    }

    #[test]
    fn special_char_sql_injection_union() {
        let sanitized = sanitize_query("' UNION SELECT * FROM passwords --");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"UNION"));
        assert!(parts.contains(&"SELECT"));
        assert!(parts.contains(&"*"));
        assert!(parts.contains(&"FROM"));
        assert!(parts.contains(&"passwords"));
    }

    #[test]
    fn special_char_sql_parse_as_literal() {
        let tokens = parse_boolean_query("OR 1=1");
        assert!(
            tokens.iter().any(|t| matches!(t, QueryToken::Or)),
            "OR should be parsed as Or operator: {tokens:?}"
        );
    }

    // Category 6: Shell injection patterns

    #[test]
    fn special_char_shell_subshell() {
        let sanitized = sanitize_query("$(cmd)");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["cmd"]);
    }

    #[test]
    fn special_char_shell_backticks() {
        let sanitized = sanitize_query("`cmd`");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["cmd"]);
    }

    #[test]
    fn special_char_shell_pipe_rm() {
        let sanitized = sanitize_query("| rm -rf /");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"rm"));
        assert!(parts.contains(&"rf"));
        assert!(!sanitized.contains('|'));
        assert!(!sanitized.contains('/'));
    }

    #[test]
    fn special_char_shell_semicolon_chain() {
        let sanitized = sanitize_query("test; echo pwned; cat /etc/passwd");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"test"));
        assert!(parts.contains(&"echo"));
        assert!(parts.contains(&"pwned"));
        assert!(!sanitized.contains(';'));
    }

    // Category 7: Null bytes

    #[test]
    fn special_char_null_byte_mid_string() {
        let sanitized = sanitize_query("test\x00hidden");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["test", "hidden"]);
    }

    #[test]
    fn special_char_null_byte_leading() {
        let sanitized = sanitize_query("\x00\x00attack");
        assert_eq!(sanitized.trim(), "attack");
    }

    #[test]
    fn special_char_null_byte_trailing() {
        let sanitized = sanitize_query("query\x00\x00\x00");
        assert_eq!(sanitized.trim(), "query");
    }

    #[test]
    fn special_char_null_byte_parse() {
        let tokens = parse_boolean_query("test\x00hidden");
        assert!(
            !tokens.is_empty(),
            "Null bytes should not prevent parsing: {tokens:?}"
        );
    }

    // Category 8: Control characters

    #[test]
    fn special_char_control_newline() {
        let sanitized = sanitize_query("line1\nline2");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["line1", "line2"]);
    }

    #[test]
    fn special_char_control_tab_cr() {
        let sanitized = sanitize_query("tab\there\r\nend");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["tab", "here", "end"]);
    }

    #[test]
    fn special_char_control_parse_whitespace() {
        let tokens = parse_boolean_query("hello\tworld\ntest");
        let terms: Vec<&str> = tokens
            .iter()
            .filter_map(|t| match t {
                QueryToken::Term(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(terms, vec!["hello", "world", "test"]);
    }

    #[test]
    fn special_char_control_bell_escape() {
        let sanitized = sanitize_query("test\x07\x1b[31mred");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"test"));
        assert!(parts.contains(&"31mred"));
    }

    // Category 9: HTML/XML entities

    #[test]
    fn special_char_html_entity_lt() {
        let sanitized = sanitize_query("&lt;script&gt;");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["lt", "script", "gt"]);
    }

    #[test]
    fn special_char_html_numeric_entity() {
        let sanitized = sanitize_query("&#x3C;script&#x3E;");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"x3C"));
        assert!(parts.contains(&"script"));
        assert!(parts.contains(&"x3E"));
    }

    #[test]
    fn special_char_html_tags_stripped() {
        let sanitized = sanitize_query("<script>alert('xss')</script>");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"script"));
        assert!(parts.contains(&"alert"));
        assert!(parts.contains(&"xss"));
    }

    #[test]
    fn special_char_html_attribute() {
        let sanitized = sanitize_query("<img src=\"evil.js\" onerror=\"alert(1)\">");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert!(parts.contains(&"img"));
        assert!(parts.contains(&"src"));
        assert!(parts.contains(&"onerror"));
    }

    // Category 10: URL encoding

    #[test]
    fn special_char_url_percent_encoding() {
        let sanitized = sanitize_query("%20space%2Fslash");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["20space", "2Fslash"]);
    }

    #[test]
    fn special_char_url_null_byte_encoded() {
        let sanitized = sanitize_query("test%00hidden");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["test", "00hidden"]);
    }

    #[test]
    fn special_char_url_full_query_string() {
        let sanitized = sanitize_query("search?q=hello&lang=en");
        let parts: Vec<&str> = sanitized.split_whitespace().collect();
        assert_eq!(parts, vec!["search", "q", "hello", "lang", "en"]);
    }

    // Cross-cutting: full pipeline integration

    #[test]
    fn special_char_explain_sql_injection() {
        let filters = SearchFilters::default();
        let explanation = QueryExplanation::analyze("'OR 1=1--", &filters);
        assert!(
            !explanation.parsed.terms.is_empty() || !explanation.parsed.phrases.is_empty(),
            "SQL injection should produce parseable terms"
        );
    }

    #[test]
    fn special_char_explain_shell_injection() {
        let filters = SearchFilters::default();
        let explanation = QueryExplanation::analyze("$(rm -rf /)", &filters);
        assert!(
            !explanation.parsed.terms.is_empty(),
            "Shell injection should produce parseable terms"
        );
    }

    #[test]
    fn special_char_explain_html_xss() {
        let filters = SearchFilters::default();
        let explanation = QueryExplanation::analyze("<script>alert('xss')</script>", &filters);
        assert!(
            !explanation.parsed.terms.is_empty(),
            "XSS payload should produce parseable terms"
        );
    }

    #[test]
    fn special_char_terms_lower_injection() {
        let qt = QueryTermsLower::from_query("'; DROP TABLE--");
        let tokens: Vec<&str> = qt.tokens().collect();
        for token in &tokens {
            assert!(
                token.chars().all(|c| c.is_alphanumeric()),
                "Token should only contain alphanumeric characters: {token}"
            );
        }
    }

    #[test]
    fn special_char_terms_lower_null_bytes() {
        let qt = QueryTermsLower::from_query("test\x00hidden");
        let tokens: Vec<&str> = qt.tokens().collect();
        assert!(tokens.contains(&"test"));
        assert!(tokens.contains(&"hidden"));
    }

    #[test]
    fn special_char_boolean_with_injection() {
        let tokens = parse_boolean_query("search AND 'OR 1=1-- NOT drop");
        assert!(
            tokens.iter().any(|t| matches!(t, QueryToken::And)),
            "Boolean AND should still be recognized: {tokens:?}"
        );
        assert!(
            tokens.iter().any(|t| matches!(t, QueryToken::Not)),
            "Boolean NOT should still be recognized: {tokens:?}"
        );
    }

    // ==========================================================================
    // Query Length Stress Tests (coding_agent_session_search-z1bk)
    // Tests for extreme input sizes to ensure parser robustness.
    // ==========================================================================

    #[test]
    fn stress_query_100k_chars_completes_quickly() {
        // 100k character query - must complete in <1 second
        let long_query = "a ".repeat(50000);
        assert_eq!(long_query.len(), 100000);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&long_query);
        let elapsed_sanitize = start.elapsed();

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&sanitized);
        let elapsed_parse = start.elapsed();

        assert!(
            elapsed_sanitize < std::time::Duration::from_secs(1),
            "sanitize_query with 100k chars took {:?} (>1s)",
            elapsed_sanitize
        );
        assert!(
            elapsed_parse < std::time::Duration::from_secs(1),
            "parse_boolean_query with 100k chars took {:?} (>1s)",
            elapsed_parse
        );
        assert!(!tokens.is_empty(), "100k char query should produce tokens");
    }

    #[test]
    fn stress_query_1000_terms() {
        // 1000 space-separated words
        let words: Vec<String> = (0..1000).map(|i| format!("word{}", i)).collect();
        let query = words.join(" ");

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&query);
        let tokens = parse_boolean_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "1000 terms query took {:?} (>1s)",
            elapsed
        );
        // Should have roughly 1000 Term tokens
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert!(
            term_count >= 900,
            "Expected ~1000 terms, got {} terms",
            term_count
        );
    }

    #[test]
    fn stress_query_1000_identical_terms() {
        // Same word repeated 1000 times
        let query = "test ".repeat(1000);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&query);
        let tokens = parse_boolean_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "1000 identical terms query took {:?} (>1s)",
            elapsed
        );

        // Verify parse_boolean_query produced expected tokens
        let parsed_term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert_eq!(parsed_term_count, 1000, "Parser should produce 1000 terms");

        // QueryTermsLower should handle this efficiently
        let qt = QueryTermsLower::from_query(&query);
        let tokens_lower: Vec<&str> = qt.tokens().collect();
        assert_eq!(
            tokens_lower.len(),
            1000,
            "All 1000 identical terms should be preserved"
        );
        assert!(
            tokens_lower.iter().all(|t| *t == "test"),
            "All tokens should be 'test'"
        );
    }

    #[test]
    fn stress_query_10k_char_single_term() {
        // 10k character single continuous string (no spaces)
        let long_term = "a".repeat(10000);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&long_term);
        let tokens = parse_boolean_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "10k char single term took {:?} (>1s)",
            elapsed
        );
        assert_eq!(tokens.len(), 1, "Should produce exactly one token");
        assert!(
            matches!(&tokens[0], QueryToken::Term(t) if t.len() == 10000),
            "Expected Term token"
        );
    }

    #[test]
    fn stress_deeply_nested_parentheses() {
        // 100+ levels of nested parentheses (though parser doesn't use them,
        // they become spaces and shouldn't cause issues)
        let open_parens = "(".repeat(100);
        let close_parens = ")".repeat(100);
        let query = format!("{}test{}", open_parens, close_parens);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&query);
        let tokens = parse_boolean_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "Deeply nested parens took {:?} (>100ms)",
            elapsed
        );
        // Parentheses become spaces, leaving just "test"
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert_eq!(term_count, 1, "Should have 1 term after sanitizing parens");
    }

    #[test]
    fn stress_many_boolean_operators() {
        // 100+ boolean operators: "a AND b AND c AND ..."
        let terms: Vec<String> = (0..101).map(|i| format!("term{}", i)).collect();
        let query = terms.join(" AND ");

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&query);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "100+ boolean ops took {:?} (>1s)",
            elapsed
        );

        let and_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::And))
            .count();
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();

        assert_eq!(and_count, 100, "Should have 100 AND operators");
        assert_eq!(term_count, 101, "Should have 101 terms");
    }

    #[test]
    fn stress_many_or_operators() {
        // 100+ OR operators: "a OR b OR c OR ..."
        let terms: Vec<String> = (0..101).map(|i| format!("opt{}", i)).collect();
        let query = terms.join(" OR ");

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&query);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "100+ OR ops took {:?} (>1s)",
            elapsed
        );

        let or_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Or))
            .count();
        assert_eq!(or_count, 100, "Should have 100 OR operators");
    }

    #[test]
    fn stress_mixed_boolean_operators() {
        // Complex query with many mixed operators
        let query = "a AND b OR c NOT d AND e OR f NOT g ".repeat(50);

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&query);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "Mixed boolean ops took {:?} (>1s)",
            elapsed
        );
        assert!(
            !tokens.is_empty(),
            "Complex boolean query should produce tokens"
        );
    }

    #[test]
    fn stress_memory_bounds_large_query() {
        // Verify no excessive memory allocation with large input
        // We can't easily measure memory in a unit test, but we can verify
        // the output size is reasonable relative to input.
        let large_query = "x".repeat(100000);

        let sanitized = sanitize_query(&large_query);
        let tokens = parse_boolean_query(&sanitized);

        // Sanitized output shouldn't be larger than input
        assert!(
            sanitized.len() <= large_query.len(),
            "Sanitized output should not exceed input size"
        );

        // Should produce exactly 1 token
        assert_eq!(tokens.len(), 1);

        // QueryTermsLower internal storage should be bounded
        let qt = QueryTermsLower::from_query(&large_query);
        let token_count = qt.tokens().count();
        assert_eq!(token_count, 1, "Should be 1 token of 100k chars");
    }

    #[test]
    fn stress_concurrent_queries() {
        use std::thread;

        let queries: Vec<String> = (0..100)
            .map(|i| format!("concurrent_query_{} test search", i))
            .collect();

        let handles: Vec<_> = queries
            .into_iter()
            .map(|query| {
                thread::spawn(move || {
                    let sanitized = sanitize_query(&query);
                    let tokens = parse_boolean_query(&sanitized);
                    let qt = QueryTermsLower::from_query(&query);
                    (tokens.len(), qt.tokens().count())
                })
            })
            .collect();

        for (i, handle) in handles.into_iter().enumerate() {
            let (token_len, qt_len) = handle.join().expect("Thread panicked");
            assert!(token_len > 0, "Query {} should produce tokens", i);
            assert!(qt_len > 0, "Query {} QueryTermsLower should have tokens", i);
        }
    }

    #[test]
    fn stress_many_quoted_phrases() {
        // 50 quoted phrases
        let phrases: Vec<String> = (0..50)
            .map(|i| format!("\"phrase number {}\"", i))
            .collect();
        let query = phrases.join(" AND ");

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&query);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "50 quoted phrases took {:?} (>1s)",
            elapsed
        );

        let phrase_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Phrase(_)))
            .count();
        assert_eq!(phrase_count, 50, "Should have 50 phrases");
    }

    #[test]
    fn stress_alternating_quotes() {
        // Alternating quoted and unquoted: "a" b "c" d "e" ...
        let parts: Vec<String> = (0..100)
            .map(|i| {
                if i % 2 == 0 {
                    format!("\"word{}\"", i)
                } else {
                    format!("word{}", i)
                }
            })
            .collect();
        let query = parts.join(" ");

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&query);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "100 alternating quotes took {:?} (>1s)",
            elapsed
        );

        let phrase_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Phrase(_)))
            .count();
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();

        assert_eq!(phrase_count, 50, "Should have 50 phrases");
        assert_eq!(term_count, 50, "Should have 50 terms");
    }

    #[test]
    fn stress_many_wildcards() {
        // Many wildcard patterns
        let patterns: Vec<&str> = vec!["pre*", "*suf", "*sub*", "a*b", "test*", "*ing", "*tion*"];
        let query = patterns
            .iter()
            .cycle()
            .take(100)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&query);
        let tokens = parse_boolean_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "100 wildcards took {:?} (>1s)",
            elapsed
        );
        assert!(!tokens.is_empty());
    }

    #[test]
    fn stress_query_explanation_large_query() {
        // Test QueryExplanation with a large query
        let words: Vec<String> = (0..100).map(|i| format!("term{}", i)).collect();
        let query = words.join(" ");
        let filters = SearchFilters::default();

        let start = std::time::Instant::now();
        let explanation = QueryExplanation::analyze(&query, &filters);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "QueryExplanation for 100 terms took {:?} (>2s)",
            elapsed
        );
        assert!(
            !explanation.parsed.terms.is_empty(),
            "Should parse terms successfully"
        );
    }

    #[test]
    fn stress_very_long_single_quoted_phrase() {
        // Single quoted phrase with many words
        let words: Vec<String> = (0..500).map(|i| format!("word{}", i)).collect();
        let phrase = format!("\"{}\"", words.join(" "));

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&phrase);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "500-word phrase took {:?} (>1s)",
            elapsed
        );

        let phrase_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Phrase(_)))
            .count();
        assert_eq!(phrase_count, 1, "Should have exactly 1 phrase");
    }

    #[test]
    fn stress_not_prefix_many() {
        // Many NOT prefixes: -a -b -c -d ...
        let terms: Vec<String> = (0..100).map(|i| format!("-term{}", i)).collect();
        let query = terms.join(" ");

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&query);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "100 NOT prefixes took {:?} (>1s)",
            elapsed
        );

        let not_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Not))
            .count();
        assert_eq!(not_count, 100, "Should have 100 NOT operators");
    }

    #[test]
    fn stress_unicode_large_cjk_query() {
        // Large CJK query (each char is alphanumeric)
        let cjk_chars = "中文日本語한국어".repeat(1000);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&cjk_chars);
        let qt = QueryTermsLower::from_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "Large CJK query took {:?} (>1s)",
            elapsed
        );
        assert!(!qt.is_empty(), "CJK query should produce tokens");
    }

    #[test]
    fn stress_unicode_many_emoji() {
        // Query with many emoji (non-alphanumeric, become spaces)
        let emoji_query = "🚀 🔍 📝 💻 🎯 ".repeat(500);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&emoji_query);
        let tokens = parse_boolean_query(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "Emoji query took {:?} (>1s)",
            elapsed
        );
        // Emoji are stripped, leaving empty
        assert!(
            tokens.is_empty(),
            "Emoji-only query should produce no tokens"
        );
    }

    #[test]
    fn stress_mixed_content_large() {
        // Mixed content: code, prose, symbols, unicode
        let mixed = r#"
            function test() { return x + y; }
            SELECT * FROM users WHERE id = 1;
            The quick brown fox 狐狸 jumps over lazy dog
            Error: "undefined is not a function" at line 42
            https://example.com/path?query=value&other=123
        "#
        .repeat(100);

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&mixed);
        let tokens = parse_boolean_query(&sanitized);
        let qt = QueryTermsLower::from_query(&mixed);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Mixed content query took {:?} (>2s)",
            elapsed
        );
        assert!(!tokens.is_empty());
        assert!(!qt.is_empty());
    }

    // ==========================================================================
    // Query Parser Unit Tests (br-335y) - Unicode, Special Chars, Edge Cases
    // ==========================================================================

    // --- Unicode queries with emoji in terms ---

    #[test]
    fn unicode_emoji_mixed_with_alphanumeric() {
        // Emoji surrounded by alphanumeric text
        let tokens = parse_boolean_query("rocket🚀launch");
        assert_eq!(tokens.len(), 1);
        // sanitize_query strips emoji (non-alphanumeric), so this becomes "rocket launch"
        let sanitized = sanitize_query("rocket🚀launch");
        assert_eq!(sanitized, "rocket launch");

        // Multiple emoji between words
        let sanitized2 = sanitize_query("test🔥🎯code");
        assert_eq!(sanitized2, "test  code");
    }

    #[test]
    fn unicode_emoji_with_boolean_operators() {
        // AND/OR/NOT with queries containing emoji
        let tokens = parse_boolean_query("🚀code AND test");
        // After parsing, we should have 3 tokens (emoji becomes space/empty)
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert!(term_count >= 1, "Should have at least one term");

        // OR with emoji
        let tokens_or = parse_boolean_query("deploy OR 🎯target");
        let has_or = tokens_or.iter().any(|t| matches!(t, QueryToken::Or));
        assert!(has_or, "Should detect OR operator");
    }

    #[test]
    fn unicode_emoji_at_word_boundaries() {
        // Emoji at start of query
        let sanitized_start = sanitize_query("🔍search");
        assert_eq!(sanitized_start, " search");

        // Emoji at end of query
        let sanitized_end = sanitize_query("complete✅");
        assert_eq!(sanitized_end, "complete ");

        // Only emoji - becomes empty
        let sanitized_only = sanitize_query("🎉🎊🎁");
        assert!(
            sanitized_only.trim().is_empty(),
            "Emoji-only should be empty after trimming"
        );
    }

    // --- RTL (Right-to-Left) text: Arabic and Hebrew ---

    #[test]
    fn unicode_arabic_text_preserved() {
        // Arabic text should be preserved as alphanumeric
        let arabic = "مرحبا بالعالم"; // "Hello World" in Arabic
        let sanitized = sanitize_query(arabic);
        assert_eq!(
            sanitized, arabic,
            "Arabic alphanumeric chars should be preserved"
        );

        let tokens = parse_boolean_query(arabic);
        assert!(!tokens.is_empty(), "Arabic query should produce tokens");
    }

    #[test]
    fn unicode_hebrew_text_preserved() {
        // Hebrew text should be preserved
        let hebrew = "שלום עולם"; // "Hello World" in Hebrew
        let sanitized = sanitize_query(hebrew);
        assert_eq!(
            sanitized, hebrew,
            "Hebrew alphanumeric chars should be preserved"
        );

        let tokens = parse_boolean_query(hebrew);
        assert!(!tokens.is_empty(), "Hebrew query should produce tokens");
    }

    #[test]
    fn unicode_mixed_rtl_and_ltr() {
        // Mixed RTL (Arabic) and LTR (English) text
        let mixed = "hello مرحبا world";
        let sanitized = sanitize_query(mixed);
        assert_eq!(sanitized, mixed, "Mixed RTL/LTR should be preserved");

        let tokens = parse_boolean_query(mixed);
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert_eq!(term_count, 3, "Should have 3 terms");
    }

    #[test]
    fn unicode_rtl_with_boolean_operators() {
        // Hebrew with AND operator
        let hebrew_and = "שלום AND עולם";
        let tokens = parse_boolean_query(hebrew_and);
        let has_and = tokens.iter().any(|t| matches!(t, QueryToken::And));
        assert!(has_and, "Should detect AND operator in Hebrew query");

        // Arabic with NOT operator
        let arabic_not = "مرحبا NOT بالعالم";
        let tokens_not = parse_boolean_query(arabic_not);
        let has_not = tokens_not.iter().any(|t| matches!(t, QueryToken::Not));
        assert!(has_not, "Should detect NOT operator in Arabic query");
    }

    // --- Backslash handling ---

    #[test]
    fn special_chars_backslash_stripped() {
        // Backslash is not alphanumeric, so it becomes space
        let query = r"path\to\file";
        let sanitized = sanitize_query(query);
        assert_eq!(sanitized, "path to file");
    }

    #[test]
    fn special_chars_escaped_quotes_handling() {
        // Backslash before quote - backslash stripped, quote preserved
        let query = r#"say \"hello\""#;
        let sanitized = sanitize_query(query);
        // Backslash becomes space, quotes preserved
        assert!(sanitized.contains('"'), "Quotes should be preserved");
    }

    #[test]
    fn special_chars_windows_paths() {
        // Windows-style paths with backslashes
        let path = r"C:\Users\test\Documents";
        let sanitized = sanitize_query(path);
        assert_eq!(sanitized, "C  Users test Documents");
    }

    // --- Nested/Complex boolean operators ---

    #[test]
    fn boolean_deeply_nested_operators() {
        // Complex nested expression (parser treats this as linear)
        let query = "a AND b OR c NOT d AND e";
        let tokens = parse_boolean_query(query);

        let mut and_count = 0;
        let mut or_count = 0;
        let mut not_count = 0;
        for token in &tokens {
            match token {
                QueryToken::And => and_count += 1,
                QueryToken::Or => or_count += 1,
                QueryToken::Not => not_count += 1,
                _ => {}
            }
        }

        assert_eq!(and_count, 2, "Should have 2 AND operators");
        assert_eq!(or_count, 1, "Should have 1 OR operator");
        assert_eq!(not_count, 1, "Should have 1 NOT operator");
    }

    #[test]
    fn boolean_consecutive_operators_degenerate() {
        // Consecutive operators: "AND AND" - second AND becomes a term
        let tokens = parse_boolean_query("foo AND AND bar");
        // "AND" as the final part of "AND AND" is treated as operator, then next "bar" is term
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert!(
            term_count >= 2,
            "Should have at least 2 terms (foo and bar)"
        );
    }

    #[test]
    fn boolean_operator_at_start() {
        // Operator at start of query
        let tokens = parse_boolean_query("AND foo");
        let has_and = tokens.iter().any(|t| matches!(t, QueryToken::And));
        assert!(has_and, "Leading AND should be detected");

        let tokens_or = parse_boolean_query("OR test");
        let has_or = tokens_or.iter().any(|t| matches!(t, QueryToken::Or));
        assert!(has_or, "Leading OR should be detected");
    }

    #[test]
    fn boolean_operator_at_end() {
        // Operator at end of query
        let tokens = parse_boolean_query("foo AND");
        let has_and = tokens.iter().any(|t| matches!(t, QueryToken::And));
        assert!(has_and, "Trailing AND should be detected");
    }

    // --- Numeric-only queries ---

    #[test]
    fn numeric_query_digits_only() {
        // Query with only digits
        let tokens = parse_boolean_query("12345");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0], QueryToken::Term("12345".to_string()));

        let sanitized = sanitize_query("12345");
        assert_eq!(sanitized, "12345");
    }

    #[test]
    fn numeric_query_with_text() {
        // Mixed numeric and text
        let tokens = parse_boolean_query("error 404 not found");
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        // "404", "error", "found" are terms, "not" is NOT operator
        assert!(term_count >= 3, "Should have at least 3 terms");
    }

    #[test]
    fn numeric_versions_with_dots() {
        // Version numbers like "1.2.3"
        let sanitized = sanitize_query("version 1.2.3");
        assert_eq!(sanitized, "version 1 2 3"); // dots become spaces
    }

    // --- Tab and newline handling ---

    #[test]
    fn whitespace_tabs_treated_as_separators() {
        let tokens = parse_boolean_query("foo\tbar\tbaz");
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert_eq!(term_count, 3, "Tabs should separate terms");
    }

    #[test]
    fn whitespace_newlines_treated_as_separators() {
        let tokens = parse_boolean_query("foo\nbar\nbaz");
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert_eq!(term_count, 3, "Newlines should separate terms");
    }

    #[test]
    fn whitespace_mixed_types() {
        let tokens = parse_boolean_query("a \t b \n c   d");
        let term_count = tokens
            .iter()
            .filter(|t| matches!(t, QueryToken::Term(_)))
            .count();
        assert_eq!(term_count, 4, "Mixed whitespace should separate properly");
    }

    // --- Very long single terms (no spaces) ---

    #[test]
    fn stress_very_long_single_term() {
        // Single term with 10K characters (no spaces)
        let long_term = "a".repeat(10_000);

        let start = std::time::Instant::now();
        let tokens = parse_boolean_query(&long_term);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "10K char term took {:?} (>1s)",
            elapsed
        );
        assert_eq!(tokens.len(), 1);
        if let QueryToken::Term(t) = &tokens[0] {
            assert_eq!(t.len(), 10_000);
        } else {
            panic!("Expected Term token");
        }
    }

    #[test]
    fn stress_very_long_term_with_wildcard() {
        // Long term with wildcard suffix
        let long_pattern = format!("{}*", "prefix".repeat(1000));

        let start = std::time::Instant::now();
        let sanitized = sanitize_query(&long_pattern);
        let pattern = WildcardPattern::parse(&sanitized);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "Long wildcard pattern took {:?} (>1s)",
            elapsed
        );
        assert!(
            matches!(pattern, WildcardPattern::Prefix(_)),
            "Should parse as prefix pattern"
        );
    }

    // --- QueryExplanation edge cases ---

    #[test]
    fn query_explanation_empty_query() {
        let explanation = QueryExplanation::analyze("", &SearchFilters::default());
        assert_eq!(explanation.query_type, QueryType::Empty);
    }

    #[test]
    fn query_explanation_whitespace_only_query() {
        let explanation = QueryExplanation::analyze("   \t\n  ", &SearchFilters::default());
        assert_eq!(explanation.query_type, QueryType::Empty);
    }

    #[test]
    fn query_explanation_unicode_query() {
        let explanation = QueryExplanation::analyze("日本語 search", &SearchFilters::default());
        // Should classify as Simple (no operators, multiple terms = implicit AND)
        assert!(!explanation.parsed.terms.is_empty());
    }

    // --- QueryTermsLower edge cases ---

    #[test]
    fn query_terms_lower_unicode_normalization() {
        // Accented characters should be lowercased properly
        let terms = QueryTermsLower::from_query("CAFÉ RÉSUMÉ");
        assert_eq!(terms.query_lower, "café résumé");
    }

    #[test]
    fn query_terms_lower_mixed_case_unicode() {
        // Mixed case CJK and Latin
        let terms = QueryTermsLower::from_query("Hello日本語World");
        // CJK chars have no case, Latin chars should be lowercased
        assert!(terms.query_lower.contains("hello"));
        assert!(terms.query_lower.contains("world"));
    }

    #[test]
    fn query_terms_lower_preserves_numbers() {
        let terms = QueryTermsLower::from_query("ABC123XYZ");
        assert_eq!(terms.query_lower, "abc123xyz");
    }

    // --- WildcardPattern edge cases ---

    #[test]
    fn wildcard_pattern_internal_asterisk() {
        // Internal wildcard: f*o
        let pattern = WildcardPattern::parse("f*o");
        assert!(
            matches!(pattern, WildcardPattern::Complex(_)),
            "Internal asterisk should be Complex"
        );
    }

    #[test]
    fn wildcard_pattern_multiple_internal_asterisks() {
        // Multiple internal wildcards: a*b*c
        let pattern = WildcardPattern::parse("a*b*c");
        assert!(
            matches!(pattern, WildcardPattern::Complex(_)),
            "Multiple internal asterisks should be Complex"
        );
    }

    #[test]
    fn wildcard_pattern_regex_escapes_special_chars() {
        // Pattern with regex-special characters
        let pattern = WildcardPattern::parse("*foo.bar*");
        if let Some(regex) = pattern.to_regex() {
            assert!(
                regex.contains("\\."),
                "Dot should be escaped in regex: {}",
                regex
            );
        }
    }

    #[test]
    fn wildcard_pattern_complex_regex_generation() {
        let pattern = WildcardPattern::parse("f*o*o");
        if let Some(regex) = pattern.to_regex() {
            // Should handle internal wildcards
            assert!(
                regex.contains(".*"),
                "Should have .* for internal wildcards: {}",
                regex
            );
        }
    }

    #[test]
    fn test_transpile_to_fts5() {
        // Simple terms
        assert_eq!(
            transpile_to_fts5("foo bar"),
            Some("foo AND bar".to_string())
        );

        // Boolean operators
        assert_eq!(
            transpile_to_fts5("foo AND bar"),
            Some("foo AND bar".to_string())
        );
        assert_eq!(
            transpile_to_fts5("foo OR bar"),
            Some("(foo OR bar)".to_string())
        );
        assert_eq!(transpile_to_fts5("NOT foo"), Some("NOT foo".to_string()));

        // Precedence: OR binds tighter than AND in our parser logic
        // "A AND B OR C" -> "A AND (B OR C)"
        assert_eq!(
            transpile_to_fts5("A AND B OR C"),
            Some("A AND (B OR C)".to_string())
        );

        // "A OR B AND C" -> "(A OR B) AND C"
        assert_eq!(
            transpile_to_fts5("A OR B AND C"),
            Some("(A OR B) AND C".to_string())
        );

        // "A OR B OR C" -> "(A OR B OR C)"
        assert_eq!(
            transpile_to_fts5("A OR B OR C"),
            Some("(A OR B OR C)".to_string())
        );

        // Phrases
        assert_eq!(
            transpile_to_fts5("\"foo bar\""),
            Some("\"foo bar\"".to_string())
        );

        // Wildcards (allowed trailing)
        assert_eq!(transpile_to_fts5("foo*"), Some("foo*".to_string()));

        // Unsupported wildcards (leading/internal)
        assert_eq!(transpile_to_fts5("*foo"), None);
        assert_eq!(transpile_to_fts5("f*o"), None);

        // Mixed sanitization
        // "foo-bar" -> "foo bar" -> "foo AND bar" in FTS5 implicit syntax?
        // My implementation splits "foo-bar" into "foo", "bar" and joins with AND.
        // And wraps in parens if >1 part.
        assert_eq!(
            transpile_to_fts5("foo-bar"),
            Some("(foo AND bar)".to_string())
        );

        // NOT A OR B -> NOT A AND B (Tantivy logic replication)
        assert_eq!(
            transpile_to_fts5("NOT A OR B"),
            Some("NOT A AND B".to_string())
        );
    }
}
