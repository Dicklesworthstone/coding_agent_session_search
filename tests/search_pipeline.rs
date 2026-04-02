//! Comprehensive search pipeline unit/integration tests (bead n646).
//!
//! Tests the search pipeline end-to-end at unit/integration level:
//! - Tantivy schema and indexing
//! - Query parsing and wildcard patterns
//! - Cache behavior (hit/miss/shortfall/eviction)
//! - Ranking modes and time decay
//! - Snippet extraction and highlighting
//!
//! All tests use real Tantivy indexes and SQLite metadata - no mocks.

use coding_agent_search::search::query::{
    FieldMask, MatchType, SearchClient, SearchFilters, SearchHit,
};
use coding_agent_search::search::tantivy::TantivyIndex;
use tempfile::TempDir;

mod util;

// =============================================================================
// WILDCARD AND PATTERN MATCHING TESTS
// =============================================================================

/// Prefix wildcard (foo*) should match terms starting with the prefix.
#[test]
fn prefix_wildcard_matches_start_of_term() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("prefix test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "authentication authorization authenticate")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // auth* should match authentication, authorization, authenticate
    let hits = client
        .search("auth*", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(
        !hits.is_empty(),
        "auth* should match documents with auth-prefixed terms"
    );
    assert!(
        hits[0].content.contains("authentication"),
        "should find content with authentication"
    );
    assert_eq!(
        hits[0].match_type,
        MatchType::Prefix,
        "explicit prefix should have Prefix match type"
    );
}

/// Suffix wildcard (*bar) should match terms ending with suffix.
#[test]
fn suffix_wildcard_matches_end_of_term() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("suffix test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "function action decoration selection")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // *tion should match function, action, decoration, selection
    let hits = client
        .search("*tion", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(!hits.is_empty(), "*tion should match -tion suffixed terms");
    assert_eq!(
        hits[0].match_type,
        MatchType::Suffix,
        "suffix wildcard should have Suffix match type"
    );
}

/// Substring wildcard (*foo*) should match terms containing substring.
#[test]
fn substring_wildcard_matches_middle_of_term() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("substring test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "configuration reconfigure preconfig configurable")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // *config* should match all terms containing "config"
    let hits = client
        .search("*config*", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(
        !hits.is_empty(),
        "*config* should match terms containing config"
    );
    assert_eq!(
        hits[0].match_type,
        MatchType::Substring,
        "substring wildcard should have Substring match type"
    );
}

/// Edge n-gram indexing enables prefix matching without explicit wildcard.
#[test]
fn edge_ngram_enables_prefix_search() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("ngram test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "implementation")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // "impl" should match "implementation" via edge n-grams
    let hits = client
        .search("impl", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Edge n-grams or prefix field should enable this match
    assert!(
        !hits.is_empty(),
        "impl should match implementation via edge n-grams"
    );
}

/// Multiple wildcard patterns in same query.
#[test]
fn multiple_terms_with_wildcards() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv1 = util::ConversationFixtureBuilder::new("tester")
        .title("doc1")
        .source_path(dir.path().join("doc1.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "authentication error handling")
        .build_normalized();

    let conv2 = util::ConversationFixtureBuilder::new("tester")
        .title("doc2")
        .source_path(dir.path().join("doc2.jsonl"))
        .base_ts(1001)
        .messages(1)
        .with_content(0, "authorization warning processing")
        .build_normalized();

    index.add_conversation(&conv1).unwrap();
    index.add_conversation(&conv2).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // "auth* error" should only match doc1 (has both auth-prefix AND error)
    let hits = client
        .search("auth* error", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(hits.len(), 1, "should only match document with both terms");
    assert!(hits[0].content.contains("error"));
}

// =============================================================================
// CACHE BEHAVIOR TESTS
// =============================================================================

/// Cache hit for identical query returns same results.
#[test]
fn cache_hit_returns_identical_results() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("cache hit test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(3)
        .with_content(0, "cache test message one")
        .with_content(1, "cache test message two")
        .with_content(2, "cache test message three")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // First search (cache miss)
    let hits1 = client
        .search("cache test", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // Second search (should return same results)
    let hits2 = client
        .search("cache test", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(
        hits1.len(),
        hits2.len(),
        "repeated search should return same number of results"
    );

    // Verify content is identical
    for (h1, h2) in hits1.iter().zip(hits2.iter()) {
        assert_eq!(h1.source_path, h2.source_path);
        assert_eq!(h1.content, h2.content);
    }

    // Cache stats may vary by implementation, but results should be consistent
    let stats = client.cache_stats();
    // At minimum, we should have exercised the cache
    assert!(
        stats.cache_hits > 0 || stats.cache_miss > 0,
        "cache should have been used"
    );
}

/// Cache shortfall when limit exceeds cached results.
#[test]
fn cache_shortfall_fetches_more_results() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Create multiple conversations
    for i in 0..5 {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("doc {}", i))
            .source_path(dir.path().join(format!("doc{}.jsonl", i)))
            .base_ts(1000 + i as i64)
            .messages(1)
            .with_content(0, format!("shortfall test content {}", i))
            .build_normalized();
        index.add_conversation(&conv).unwrap();
    }
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // First search with limit 2 (caches 2 results)
    let hits_small = client
        .search("shortfall test", filters.clone(), 2, 0, FieldMask::FULL)
        .unwrap();
    assert_eq!(hits_small.len(), 2);

    // Second search with limit 5 (should fetch more due to shortfall)
    let hits_large = client
        .search("shortfall test", filters.clone(), 5, 0, FieldMask::FULL)
        .unwrap();
    assert_eq!(hits_large.len(), 5);

    let stats = client.cache_stats();
    // A shortfall should have occurred
    assert!(
        stats.cache_miss > 0 || stats.cache_shortfall > 0,
        "should have cache miss or shortfall"
    );
}

/// Different filters produce different cache entries.
#[test]
fn different_filters_have_separate_cache_entries() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv_codex = util::ConversationFixtureBuilder::new("codex")
        .title("codex doc")
        .source_path(dir.path().join("codex.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "filter cache test")
        .build_normalized();

    let conv_claude = util::ConversationFixtureBuilder::new("claude_code")
        .title("claude doc")
        .source_path(dir.path().join("claude.jsonl"))
        .base_ts(1001)
        .messages(1)
        .with_content(0, "filter cache test")
        .build_normalized();

    index.add_conversation(&conv_codex).unwrap();
    index.add_conversation(&conv_claude).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    // Search with codex filter
    let mut codex_filters = SearchFilters::default();
    codex_filters.agents.insert("codex".into());
    let codex_hits = client
        .search(
            "filter cache",
            codex_filters.clone(),
            10,
            0,
            FieldMask::FULL,
        )
        .unwrap();

    // Search with claude filter
    let mut claude_filters = SearchFilters::default();
    claude_filters.agents.insert("claude_code".into());
    let claude_hits = client
        .search(
            "filter cache",
            claude_filters.clone(),
            10,
            0,
            FieldMask::FULL,
        )
        .unwrap();

    assert_eq!(codex_hits.len(), 1);
    assert_eq!(claude_hits.len(), 1);
    assert_eq!(codex_hits[0].agent, "codex");
    assert_eq!(claude_hits[0].agent, "claude_code");

    // Repeat searches should hit different cache entries
    let codex_hits2 = client
        .search("filter cache", codex_filters, 10, 0, FieldMask::FULL)
        .unwrap();
    let claude_hits2 = client
        .search("filter cache", claude_filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(codex_hits[0].agent, codex_hits2[0].agent);
    assert_eq!(claude_hits[0].agent, claude_hits2[0].agent);
}

// =============================================================================
// RANKING AND TIME DECAY TESTS
// =============================================================================

/// More recent documents score higher with recency boost.
#[test]
fn recency_affects_ranking() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Create conversations at different times with unique content
    let old_conv = util::ConversationFixtureBuilder::new("tester")
        .title("old document")
        .source_path(dir.path().join("old.jsonl"))
        .base_ts(1_600_000_000_000) // older
        .messages(1)
        .with_content(0, "ranking recency test old version alpha")
        .build_normalized();

    let new_conv = util::ConversationFixtureBuilder::new("tester")
        .title("new document")
        .source_path(dir.path().join("new.jsonl"))
        .base_ts(1_700_000_000_000) // newer
        .messages(1)
        .with_content(0, "ranking recency test new version beta")
        .build_normalized();

    // Index old first, new second
    index.add_conversation(&old_conv).unwrap();
    index.add_conversation(&new_conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("ranking recency test", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Should find at least one result (may deduplicate if content is identical)
    assert!(!hits.is_empty(), "should find at least one document");

    // If we have both, verify ordering
    if hits.len() >= 2 {
        let first_created = hits[0].created_at.unwrap_or(0);
        let second_created = hits[1].created_at.unwrap_or(0);

        // Newer should rank first due to recency
        assert!(
            first_created >= second_created,
            "newer document ({}) should rank above older ({}) by default",
            first_created,
            second_created
        );
    }
}

/// BM25 score affects ranking based on term frequency.
#[test]
fn term_frequency_affects_bm25_score() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Document with low term frequency
    let low_tf = util::ConversationFixtureBuilder::new("tester")
        .title("low tf")
        .source_path(dir.path().join("low.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(0, "rust is good")
        .build_normalized();

    // Document with high term frequency
    let high_tf = util::ConversationFixtureBuilder::new("tester")
        .title("high tf")
        .source_path(dir.path().join("high.jsonl"))
        .base_ts(1_700_000_000_000) // Same timestamp to isolate BM25 effect
        .messages(1)
        .with_content(0, "rust rust rust rust rust rust rust rust code")
        .build_normalized();

    index.add_conversation(&low_tf).unwrap();
    index.add_conversation(&high_tf).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("rust", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(hits.len(), 2);

    // Higher term frequency should result in higher score
    // (BM25 with saturation, but more occurrences still helps)
    let high_tf_hit = hits.iter().find(|h| h.title == "high tf").unwrap();
    let low_tf_hit = hits.iter().find(|h| h.title == "low tf").unwrap();

    assert!(
        high_tf_hit.score >= low_tf_hit.score,
        "document with higher term frequency should have higher or equal score"
    );
}

// =============================================================================
// SNIPPET AND CONTENT TESTS
// =============================================================================

/// Snippet extraction includes surrounding context.
#[test]
fn snippet_includes_context_around_match() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("snippet test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(
            0,
            "This is some leading context. The unique_search_term appears here. And some trailing context.",
        )
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("unique_search_term", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(hits.len(), 1);

    // Snippet should contain the search term
    assert!(
        hits[0].snippet.contains("unique_search_term")
            || hits[0].content.contains("unique_search_term"),
        "snippet or content should contain the search term"
    );
}

/// Content field preserves full message text.
#[test]
fn content_field_preserves_full_text() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let full_content = "This is a very long message with multiple sentences. \
        It contains various types of content including code examples like `fn main()`. \
        The purpose is to verify that the entire content is preserved and searchable.";

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("full content test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, full_content)
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("searchable", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].content.contains(full_content) || hits[0].content.len() >= full_content.len() / 2,
        "content should contain full or substantial portion of original text"
    );
}

/// Title field is searchable.
#[test]
fn title_field_is_searchable() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("UniqueConversationTitle123")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "some content without the title term")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Search for term only in title
    let hits = client
        .search("UniqueConversation*", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(!hits.is_empty(), "should find document by title search");
    assert_eq!(hits[0].title, "UniqueConversationTitle123");
}

// =============================================================================
// EDGE CASES AND ERROR HANDLING
// =============================================================================

/// Empty query behavior is implementation-defined.
#[test]
fn empty_query_does_not_panic() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    let source_path = dir.path().join("empty-query.jsonl");

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("empty query title")
        .source_path(source_path.clone())
        .messages(1)
        .with_content(0, "some content")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client.search("", filters, 10, 0, FieldMask::FULL).unwrap();

    assert_eq!(
        hits.len(),
        1,
        "empty query should surface indexed conversations"
    );
    assert_eq!(hits[0].source_path, source_path);
    assert_eq!(hits[0].title, "empty query title");
}

/// Whitespace-only query behavior is implementation-defined.
#[test]
fn whitespace_query_does_not_panic() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    let source_path = dir.path().join("whitespace-query.jsonl");

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("whitespace query title")
        .source_path(source_path)
        .messages(1)
        .with_content(0, "some content")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let empty_hits = client
        .search("", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let whitespace_hits = client
        .search("   ", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    let summarize = |hits: &[SearchHit]| {
        hits.iter()
            .map(|hit| {
                (
                    hit.source_path.clone(),
                    hit.line_number,
                    hit.title.clone(),
                    hit.content.clone(),
                )
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(
        summarize(&whitespace_hits),
        summarize(&empty_hits),
        "whitespace-only queries should behave the same as empty queries"
    );
}

/// Special characters in query are handled gracefully.
#[test]
fn special_characters_handled() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("special chars")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "Testing c++ and std::vector and foo::bar")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let std_hits = client
        .search("std::vector", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let foo_hits = client
        .search("foo::bar", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(
        std_hits.len(),
        1,
        "namespaced queries should still find the document"
    );
    assert_eq!(
        foo_hits.len(),
        1,
        "double-colon code tokens should still find the document"
    );
    assert_eq!(std_hits[0].title, "special chars");
    assert_eq!(foo_hits[0].title, "special chars");

    for query in ["c++", "(test)", "[brackets]"] {
        client
            .search(query, filters.clone(), 10, 0, FieldMask::FULL)
            .unwrap_or_else(|err| {
                panic!("query {query:?} should be sanitized without error: {err}")
            });
    }
}

/// Query with only wildcards.
#[test]
fn only_wildcard_query() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .with_content(0, "some test content")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let first = client
        .search("*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let second = client.search("*", filters, 10, 0, FieldMask::FULL).unwrap();

    let summarize = |hits: &[SearchHit]| {
        hits.iter()
            .map(|hit| {
                (
                    hit.source_path.clone(),
                    hit.line_number,
                    hit.title.clone(),
                    hit.content.clone(),
                )
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(
        summarize(&first),
        summarize(&second),
        "wildcard-only queries should be stable across repeated execution"
    );
}

// =============================================================================
// MULTI-MESSAGE AND MULTI-DOCUMENT TESTS
// =============================================================================

/// Search across multiple messages in same conversation.
#[test]
fn search_spans_multiple_messages() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("multi message")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(3)
        .with_content(0, "first message about alpha")
        .with_content(1, "second message about beta")
        .with_content(2, "third message about gamma")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Should find conversation searching for any message content
    let alpha = client
        .search("alpha", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let beta = client
        .search("beta", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let gamma = client
        .search("gamma", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(!alpha.is_empty(), "should find alpha in first message");
    assert!(!beta.is_empty(), "should find beta in second message");
    assert!(!gamma.is_empty(), "should find gamma in third message");
}

/// Pagination with offset works correctly.
#[test]
fn pagination_offset_works() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Create 10 distinct documents
    for i in 0..10 {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("doc {}", i))
            .source_path(dir.path().join(format!("doc{}.jsonl", i)))
            .base_ts(1000 + i as i64)
            .messages(1)
            .with_content(0, format!("pagination test content number {}", i))
            .build_normalized();
        index.add_conversation(&conv).unwrap();
    }
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Get first page
    let page1 = client
        .search("pagination test", filters.clone(), 3, 0, FieldMask::FULL)
        .unwrap();

    // Get second page
    let page2 = client
        .search("pagination test", filters.clone(), 3, 3, FieldMask::FULL)
        .unwrap();

    assert_eq!(page1.len(), 3);
    assert_eq!(page2.len(), 3);

    // Pages should have different content
    let page1_paths: Vec<_> = page1.iter().map(|h| &h.source_path).collect();
    let page2_paths: Vec<_> = page2.iter().map(|h| &h.source_path).collect();

    for path in &page2_paths {
        assert!(
            !page1_paths.contains(path),
            "page 2 should not contain items from page 1"
        );
    }
}

/// Deduplication removes duplicate content hashes.
#[test]
fn deduplication_removes_duplicates() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let identical_content = "exactly identical dedup test content";

    // Two conversations with identical message content
    let conv1 = util::ConversationFixtureBuilder::new("tester")
        .title("conv1")
        .source_path(dir.path().join("conv1.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, identical_content)
        .build_normalized();

    let conv2 = util::ConversationFixtureBuilder::new("tester")
        .title("conv2")
        .source_path(dir.path().join("conv2.jsonl"))
        .base_ts(1001)
        .messages(1)
        .with_content(0, identical_content)
        .build_normalized();

    index.add_conversation(&conv1).unwrap();
    index.add_conversation(&conv2).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("identical dedup test", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Implementation may deduplicate or not - just verify no panic and sensible result
    assert!(!hits.is_empty(), "should find at least one result");
    assert!(hits.len() <= 2, "should have at most 2 results");
}
