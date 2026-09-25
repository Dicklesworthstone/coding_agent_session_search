use coding_agent_search::search::query::{FieldMask, MatchType, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use tempfile::TempDir;

mod util;

#[test]
fn implicit_wildcard_fallback_finds_substrings() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Seed index with "apple"
    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("fruit test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "I like eating an apple everyday")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // 1. Search "pple" (substring).
    // Exact match "pple" -> 0 hits.
    // Fallback to "*pple*" -> should find "apple".
    // We use sparse_threshold=1 to force fallback if < 1 result.
    let result = client
        .search_with_fallback("pple", filters.clone(), 10, 0, 1, FieldMask::FULL)
        .unwrap();
    let hits = result.hits;

    assert_eq!(hits.len(), 1, "Should find 'apple' via fallback for 'pple'");
    assert_eq!(
        hits[0].match_type,
        MatchType::ImplicitWildcard,
        "Match type should be ImplicitWildcard"
    );
}

#[test]
fn explicit_wildcard_works_without_fallback() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("wild test")
        .source_path(dir.path().join("log.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "config_file_v2.json")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Search "*fig*" -> explicit wildcard
    let hits = client
        .search("*fig*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    assert_eq!(hits.len(), 1);
    // Should be Substring because of *x*
    assert_eq!(
        hits[0].match_type,
        MatchType::Substring,
        "Explicit *term* should be Substring"
    );
}

/// 2l1b0.68: a sparse robot search that does not get the automatic wildcard
/// retry says why in `_meta.wildcard_fallback_skipped`. Above 10,000
/// documents the retry used to turn off without a trace.
#[test]
fn robot_meta_reports_why_the_automatic_wildcard_retry_was_skipped() {
    let home = TempDir::new().unwrap();
    let data_dir = home.path().join("cass-data");
    let sessions = home.path().join(".codex/sessions/2026/09/20");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        sessions.join("rollout-2026-09-20T10-00-00-wild-a.jsonl"),
        concat!(
            r#"{"timestamp":"2026-09-20T10:00:00.000Z","type":"session_meta","payload":{"id":"wild-a","cwd":"/work/wild","cli_version":"0.42.0"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-20T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"I like eating an apple everyday"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-09-20T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"noted, fruit it is"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    let cass = |max_docs: Option<&str>| {
        let mut cmd = assert_cmd::Command::new(util::cass_bin());
        cmd.env("HOME", home.path())
            .env("CODEX_HOME", home.path().join(".codex"))
            .env("CASS_DATA_DIR", &data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env_remove("CASS_AUTOMATIC_WILDCARD_FALLBACK_MAX_DOCS");
        if let Some(max_docs) = max_docs {
            cmd.env("CASS_AUTOMATIC_WILDCARD_FALLBACK_MAX_DOCS", max_docs);
        }
        cmd
    };
    cass(None)
        .args(["index", "--full", "--json", "--no-progress-events"])
        .assert()
        .success();
    let meta = |max_docs: Option<&str>, query: &str| -> serde_json::Value {
        let output = cass(max_docs)
            .args([
                "search",
                query,
                "--robot",
                "--robot-meta",
                "--mode",
                "lexical",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "search {query:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let meta = payload["_meta"].clone();
        eprintln!(
            "{}",
            serde_json::json!({
                "query": query,
                "max_docs": max_docs,
                "hits": payload["hits"].as_array().map(Vec::len),
                "wildcard_fallback": meta["wildcard_fallback"],
                "wildcard_fallback_skipped": meta["wildcard_fallback_skipped"],
            })
        );
        meta
    };

    // Under the default cap the retry runs and finds the substring.
    let ran = meta(None, "pple");
    assert_eq!(ran["wildcard_fallback"], true, "{ran}");
    assert!(ran["wildcard_fallback_skipped"].is_null(), "{ran}");

    // The same query on an index over the cap (two messages, cap 1) is
    // skipped, and says so; a cap of 0 turns the retry off.
    let capped = meta(Some("1"), "pple");
    assert_eq!(capped["wildcard_fallback"], false, "{capped}");
    assert_eq!(
        capped["wildcard_fallback_skipped"], "index_over_automatic_limit",
        "{capped}"
    );
    let disabled = meta(Some("0"), "pple");
    assert_eq!(disabled["wildcard_fallback"], false, "{disabled}");
    assert_eq!(
        disabled["wildcard_fallback_skipped"], "automatic_retry_disabled",
        "{disabled}"
    );

    // A zero-hit query with a token over the retry's length limit.
    let long = meta(None, "pneumonoultramicroscopic");
    assert_eq!(
        long["wildcard_fallback_skipped"], "long_query_term",
        "{long}"
    );
}
