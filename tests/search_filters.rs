use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use tempfile::TempDir;

mod util;

/// Agent filter should constrain results to the selected agent only.
#[test]
fn agent_filter_limits_results() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv_codex = util::ConversationFixtureBuilder::new("codex")
        .title("codex doc")
        .source_path(dir.path().join("codex.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(0, "shared_term apples")
        .build_normalized();
    let conv_claude = util::ConversationFixtureBuilder::new("claude_code")
        .title("claude doc")
        .source_path(dir.path().join("claude.jsonl"))
        .base_ts(1_700_000_000_001)
        .messages(1)
        .with_content(0, "shared_term oranges")
        .build_normalized();

    index.add_conversation(&conv_codex).unwrap();
    index.add_conversation(&conv_claude).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let mut filters = SearchFilters::default();
    filters.agents.insert("codex".into());
    let hits = client
        .search("shared_term", filters, 10, 0, FieldMask::FULL)
        .expect("search");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].agent, "codex");
    assert!(
        hits[0].title.contains("codex"),
        "expected codex conversation title"
    );
}

/// Workspace filter should limit results to matching path.
#[test]
fn workspace_filter_limits_results() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv_a = util::ConversationFixtureBuilder::new("tester")
        .workspace(dir.path().join("repo/a"))
        .source_path(dir.path().join("repo/a/session.jsonl"))
        .title("workspace a")
        .with_content(0, "workspace_term foo")
        .build_normalized();
    let conv_b = util::ConversationFixtureBuilder::new("tester")
        .workspace(dir.path().join("repo/b"))
        .source_path(dir.path().join("repo/b/session.jsonl"))
        .title("workspace b")
        .with_content(0, "workspace_term bar")
        .build_normalized();

    index.add_conversation(&conv_a).unwrap();
    index.add_conversation(&conv_b).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let mut filters = SearchFilters::default();
    filters
        .workspaces
        .insert(dir.path().join("repo/a").to_string_lossy().to_string());

    let hits = client
        .search("workspace_term", filters, 10, 0, FieldMask::FULL)
        .expect("search");

    assert_eq!(hits.len(), 1);
    assert!(hits[0].source_path.contains("repo/a"));
    let expected_ws = dir.path().join("repo/a").to_string_lossy().to_string();
    assert_eq!(hits[0].workspace, expected_ws);
}

/// Time filters should exclude content outside the window.
#[test]
fn time_filter_respects_since_until() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Three conversations at different times
    let conv_old = util::ConversationFixtureBuilder::new("tester")
        .base_ts(1_700_000_000_000) // early
        .with_content(0, "time_term one")
        .build_normalized();
    let conv_mid = util::ConversationFixtureBuilder::new("tester")
        .base_ts(1_800_000_000_000) // middle
        .with_content(0, "time_term two")
        .build_normalized();
    let conv_new = util::ConversationFixtureBuilder::new("tester")
        .base_ts(1_900_000_000_000) // latest
        .with_content(0, "time_term three")
        .build_normalized();

    index.add_conversation(&conv_old).unwrap();
    index.add_conversation(&conv_mid).unwrap();
    index.add_conversation(&conv_new).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let filters = SearchFilters {
        created_from: Some(1_750_000_000_000), // between old and mid
        created_to: Some(1_850_000_000_000),   // between mid and new
        ..SearchFilters::default()
    };

    let hits = client
        .search("time_term", filters, 10, 0, FieldMask::FULL)
        .expect("search");

    assert_eq!(hits.len(), 1, "only middle conversation should match");
    assert!(hits[0].content.contains("two"));
}

/// Minimal field mask should preserve hit ordering while omitting heavy fields.
#[test]
fn minimal_field_mask_preserves_order() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv_strong = util::ConversationFixtureBuilder::new("tester")
        .title("strong match")
        .source_path(dir.path().join("strong.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(0, "repeat repeat repeat")
        .build_normalized();
    let conv_weak = util::ConversationFixtureBuilder::new("tester")
        .title("weak match")
        .source_path(dir.path().join("weak.jsonl"))
        .base_ts(1_700_000_000_001)
        .messages(1)
        .with_content(0, "repeat")
        .build_normalized();

    index.add_conversation(&conv_strong).unwrap();
    index.add_conversation(&conv_weak).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let full_hits = client
        .search("repeat", SearchFilters::default(), 10, 0, FieldMask::FULL)
        .expect("search full");
    let minimal_hits = client
        .search(
            "repeat",
            SearchFilters::default(),
            10,
            0,
            FieldMask::new(false, false, false, false),
        )
        .expect("search minimal");

    assert_eq!(full_hits.len(), minimal_hits.len());
    let full_paths: Vec<String> = full_hits.iter().map(|h| h.source_path.clone()).collect();
    let minimal_paths: Vec<String> = minimal_hits.iter().map(|h| h.source_path.clone()).collect();
    assert_eq!(full_paths, minimal_paths, "ordering should be identical");

    for hit in minimal_hits {
        assert!(hit.content.is_empty());
        assert!(hit.snippet.is_empty());
        assert!(hit.title.is_empty());
        // Bead 7k7pl: pin the EXACT values minimal mask preserves,
        // not just "not empty". The test seeds conversations with
        // agent="tester" and source_path ∈ {strong.jsonl,
        // weak.jsonl}; a regression that returned an empty, default,
        // or wrong-field string would slip past `!is_empty()` but
        // fires here.
        assert_eq!(
            hit.agent, "tester",
            "minimal mask must preserve the seeded agent `tester`; got {:?}",
            hit.agent
        );
        let path_file_name = std::path::Path::new(&hit.source_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            matches!(path_file_name, "strong.jsonl" | "weak.jsonl"),
            "minimal mask source_path must match one of the two seeded files; \
             got {:?} (file_name={path_file_name:?})",
            hit.source_path
        );
    }
}
