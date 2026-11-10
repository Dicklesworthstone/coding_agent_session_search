//! Integration tests verifying the frankensearch search migration (bead s3ho2).
//!
//! Validates that:
//! 1. All search operations go through frankensearch (no direct tantivy imports remain)
//! 2. SemanticFilter directly implements frankensearch::core::filter::SearchFilter
//! 3. No duplicate FsSemanticFilterAdapter exists
//! 4. Vector search via frankensearch VectorIndex produces correct results
//! 5. RRF hybrid fusion uses frankensearch::rrf_fuse
//! 6. Query parsing and search pipeline work end-to-end through frankensearch

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use coding_agent_search::search::vector_index::{
    SemanticFilter, VectorIndex, parse_semantic_doc_id,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use tempfile::TempDir;

mod util;
use util::e2e_log::PhaseTracker;

// =============================================================================
// ZERO TANTIVY IMPORTS AUDIT
// =============================================================================

/// Programmatic verification that no direct `use tantivy::` imports remain in src/.
/// This test reads the source files and ensures all tantivy usage goes through
/// frankensearch re-exports.
#[test]
fn no_direct_tantivy_imports_in_src() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();

    fn scan_dir(dir: &std::path::Path, violations: &mut Vec<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan_dir(&path, violations);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                for (line_num, line) in content.lines().enumerate() {
                    let trimmed = line.trim();
                    // Skip comments
                    if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                        continue;
                    }
                    if trimmed.contains("use tantivy::") {
                        violations.push(format!(
                            "{}:{}: {}",
                            path.display(),
                            line_num + 1,
                            trimmed
                        ));
                    }
                }
            }
        }
    }

    scan_dir(&src_dir, &mut violations);

    assert!(
        violations.is_empty(),
        "Found direct tantivy imports (should use frankensearch::lexical_tantivy instead):\n{}",
        violations.join("\n")
    );
}

/// Verify Cargo.toml has no direct tantivy dependency.
#[test]
fn no_direct_tantivy_in_cargo_toml() {
    let cargo_toml = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let content = std::fs::read_to_string(cargo_toml).expect("read Cargo.toml");

    // Check [dependencies] section for a direct tantivy = line
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("tantivy") && trimmed.contains('=') {
            panic!(
                "Found direct tantivy dependency in Cargo.toml: {trimmed}\n\
                 tantivy should only be used via frankensearch re-exports"
            );
        }
    }
}

/// Freeze CASS's backend-selection contract independently of FrankenSearch's
/// generic `lexical` feature. CASS owns a foreign Tantivy schema and must keep
/// selecting the explicit compatibility lane when the facade default changes.
#[test]
fn cass_selects_only_the_explicit_tantivy_compatibility_surface() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read Cargo.toml");
    let dependency = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("frankensearch = "))
        .expect("frankensearch dependency declaration");

    assert!(
        dependency.contains("\"cass-compat\""),
        "CASS must select the explicit foreign-index compatibility feature: {dependency}"
    );
    assert!(
        !dependency.contains("\"lexical\""),
        "CASS must not select the facade's swappable generic lexical feature: {dependency}"
    );

    // gh#416: frankensearch resolves from crates.io behind an EXACT version
    // pin (the registry holds stale same-version twins from an older tree, so
    // a caret req could resolve wrong source; `=` fences that off). The old
    // form of this contract pinned a git revision — that shape is now itself
    // a defect, checked below via the absence of any git source.
    let pinned_version = dependency
        .split_once("version = \"=")
        .and_then(|(_, suffix)| suffix.split_once('"'))
        .map(|(version, _)| version.to_owned())
        .expect("frankensearch dependency must pin an exact registry version (= prefix)");
    assert!(
        !dependency.contains("git ="),
        "frankensearch must not resolve from git; crates.io publishing forbids it: {dependency}"
    );
    let lockfile = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock"),
    )
    .expect("read Cargo.lock");
    let expected_lock = format!(
        "name = \"frankensearch\"\nversion = \"{pinned_version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\""
    );
    assert!(
        lockfile.contains(&expected_lock),
        "Cargo.lock must resolve frankensearch {pinned_version} from the crates.io registry"
    );
    let build_contract =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .expect("read build.rs");
    assert!(
        build_contract.contains(&format!("expected_version: \"{pinned_version}\"")),
        "build.rs must enforce the same exact FrankenSearch version as Cargo.toml"
    );
    let readme =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
            .expect("read README.md");
    assert!(
        readme.contains(&format!("={pinned_version}"))
            && readme.contains("cass-compat")
            && readme.contains("lexical-tantivy"),
        "README dependency contract must name the pinned registry version and explicit compatibility lane"
    );
    assert!(
        lockfile.contains("name = \"frankensearch-lexical\""),
        "the CASS compatibility graph must contain the Tantivy-backed lexical crate"
    );
    // INVERTED BY THE CASS->QUILL FLIP, intent preserved.
    //
    // Before the flip Quill was not a cass dependency, so its ABSENCE proved
    // that `cass-compat` was not implicitly dragging it in. Quill is now the
    // lexical backend, so absence would mean the backend is missing entirely.
    //
    // The property worth guarding is unchanged — Quill must be acquired
    // EXPLICITLY, never as a silent transitive effect of another feature — so
    // the check is now: the crate is present AND the manifest names the `quill`
    // feature itself. Deleting the assertion would have dropped the
    // explicitness guarantee along with the stale expectation.
    assert!(
        lockfile.contains("name = \"frankensearch-quill\""),
        "the lexical backend crate must be in the graph after the CASS->Quill flip"
    );
    let manifest_text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read Cargo.toml");
    let frankensearch_dep_line = manifest_text
        .lines()
        .find(|line| line.starts_with("frankensearch = "))
        .expect("frankensearch dependency line");
    assert!(
        frankensearch_dep_line.contains("\"quill\""),
        "Quill must be selected explicitly in the frankensearch feature list, not acquired transitively"
    );

    let mut generic_imports = Vec::new();
    for root in ["src", "tests", "benches"] {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(root);
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(&path).expect("scan Rust source tree") {
                let entry = entry.expect("source directory entry");
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).expect("read Rust source");
                    let generic_namespace = ["frankensearch", "lexical"].join("::");
                    if source.match_indices(&generic_namespace).any(|(offset, _)| {
                        source
                            .as_bytes()
                            .get(offset + generic_namespace.len())
                            .is_none_or(|next| *next != b'_')
                    }) {
                        generic_imports.push(path);
                    }
                }
            }
        }
    }
    assert!(
        generic_imports.is_empty(),
        "Tantivy-native code must use frankensearch::lexical_tantivy explicitly: \
         {generic_imports:#?}"
    );
}

// =============================================================================
// SEARCHFILTER UNIFICATION
// =============================================================================

/// Verify SemanticFilter directly implements frankensearch::core::filter::SearchFilter.
/// This proves the adapter pattern (FsSemanticFilterAdapter) has been eliminated.
#[test]
fn semantic_filter_implements_search_filter_directly() {
    use frankensearch::core::filter::SearchFilter;

    let filter = SemanticFilter {
        agents: Some(HashSet::from([3])),
        workspaces: Some(HashSet::from([7])),
        sources: Some(HashSet::from([11])),
        roles: Some(HashSet::from([1])),
        created_from: Some(1_700_000_000_000),
        created_to: Some(1_700_000_000_100),
    };

    // Matching doc_id
    assert!(
        filter.matches("m|42|2|3|7|11|1|1700000000050", None),
        "filter should match doc_id with correct agent/workspace/source/role/timestamp"
    );

    // Wrong agent
    assert!(
        !filter.matches("m|42|2|99|7|11|1|1700000000050", None),
        "filter should reject wrong agent_id"
    );

    // Wrong workspace
    assert!(
        !filter.matches("m|42|2|3|99|11|1|1700000000050", None),
        "filter should reject wrong workspace_id"
    );

    // Wrong source
    assert!(
        !filter.matches("m|42|2|3|7|99|1|1700000000050", None),
        "filter should reject wrong source_id"
    );

    // Wrong role
    assert!(
        !filter.matches("m|42|2|3|7|11|9|1700000000050", None),
        "filter should reject wrong role"
    );

    // Timestamp before range
    assert!(
        !filter.matches("m|42|2|3|7|11|1|1699999999999", None),
        "filter should reject timestamp before created_from"
    );

    // Timestamp after range
    assert!(
        !filter.matches("m|42|2|3|7|11|1|1700000000200", None),
        "filter should reject timestamp after created_to"
    );

    // Invalid doc_id
    assert!(
        !filter.matches("not-a-valid-doc-id", None),
        "filter should reject invalid doc_id format"
    );
}

/// Verify unrestricted filter (all None) matches everything.
#[test]
fn unrestricted_semantic_filter_matches_all() {
    use frankensearch::core::filter::SearchFilter;

    let filter = SemanticFilter::default();

    assert!(filter.matches("m|1|0|5|10|20|0|1700000000000", None));
    assert!(filter.matches("m|999|3|99|99|99|2|1800000000000", None));
}

// =============================================================================
// DOC_ID PARSING
// =============================================================================

/// Verify parse_semantic_doc_id is the single parser (no duplicates).
#[test]
fn parse_semantic_doc_id_roundtrip() {
    let hash_hex = "aa".repeat(32);
    let doc_id = format!("m|42|2|3|7|11|1|1700000000000|{hash_hex}");
    let parsed = parse_semantic_doc_id(&doc_id).expect("should parse valid doc_id");

    assert_eq!(parsed.message_id, 42);
    assert_eq!(parsed.chunk_idx, 2);
    assert_eq!(parsed.agent_id, 3);
    assert_eq!(parsed.workspace_id, 7);
    assert_eq!(parsed.source_id, 11);
    assert_eq!(parsed.role, 1);
    assert_eq!(parsed.created_at_ms, 1_700_000_000_000);
    assert!(parsed.content_hash.is_some(), "should parse content hash");
}

/// Verify doc_id without content hash still parses.
#[test]
fn parse_semantic_doc_id_without_hash() {
    let doc_id = "m|100|0|5|10|20|1|1700000000000";
    let parsed = parse_semantic_doc_id(doc_id).expect("should parse doc_id without hash");

    assert_eq!(parsed.message_id, 100);
    assert_eq!(parsed.chunk_idx, 0);
    assert!(parsed.content_hash.is_none(), "should have no content hash");
}

/// Invalid doc_id formats return None.
#[test]
fn parse_semantic_doc_id_rejects_invalid() {
    assert!(parse_semantic_doc_id("").is_none());
    assert!(parse_semantic_doc_id("not-a-doc-id").is_none());
    assert!(parse_semantic_doc_id("x|1|2|3|4|5|6|7").is_none()); // wrong prefix
    assert!(parse_semantic_doc_id("m|abc|2|3|4|5|6|7").is_none()); // non-numeric
    assert!(parse_semantic_doc_id("m|1|2|3").is_none()); // too few fields
}

// =============================================================================
// FRANKENSEARCH VECTOR INDEX INTEGRATION
// =============================================================================

/// Verify frankensearch VectorIndex write + search roundtrip works correctly.
#[test]
fn frankensearch_vector_index_write_and_search() {
    let dir = TempDir::new().unwrap();
    let index_path = dir.path().join("vector_index").join("index-test.fsvi");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

    let hash_a = "00".repeat(32);
    let hash_b = "11".repeat(32);
    let doc_a = format!("m|101|0|1|10|100|1|1700000000001|{hash_a}");
    let doc_b = format!("m|202|0|2|20|200|1|1700000000002|{hash_b}");

    // Write two vectors
    let mut writer = VectorIndex::create_with_revision(
        &index_path,
        "test-embedder",
        "rev-1",
        2, // dimension
        frankensearch::index::Quantization::F16,
    )
    .expect("create vector index");

    writer
        .write_record(&doc_a, &[1.0, 0.0])
        .expect("write doc_a");
    writer
        .write_record(&doc_b, &[0.0, 1.0])
        .expect("write doc_b");
    writer.finish().expect("finish writing");

    // Read and search
    let index = VectorIndex::open(&index_path).expect("open vector index");

    // Search for vector similar to doc_a
    let results = index.search_top_k(&[1.0, 0.0], 5, None).expect("search");
    assert!(!results.is_empty(), "should find at least one result");

    let top = &results[0];
    let parsed = parse_semantic_doc_id(&top.doc_id).expect("parse top result doc_id");
    assert_eq!(parsed.message_id, 101, "top result should be doc_a");
}

/// Verify vector search with SemanticFilter integration.
#[test]
fn frankensearch_vector_search_with_semantic_filter() {
    use frankensearch::core::filter::SearchFilter;

    let dir = TempDir::new().unwrap();
    let index_path = dir.path().join("vector_index").join("index-filtered.fsvi");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

    let hash = "00".repeat(32);
    let doc_agent1 = format!("m|101|0|1|10|100|1|1700000000001|{hash}");
    let doc_agent2 = format!("m|202|0|2|20|200|1|1700000000002|{hash}");

    let mut writer = VectorIndex::create_with_revision(
        &index_path,
        "test-embedder",
        "rev-1",
        2,
        frankensearch::index::Quantization::F16,
    )
    .expect("create index");

    // Both vectors point in same direction so both would match
    writer
        .write_record(&doc_agent1, &[1.0, 0.0])
        .expect("write");
    writer
        .write_record(&doc_agent2, &[0.9, 0.1])
        .expect("write");
    writer.finish().expect("finish");

    let index = VectorIndex::open(&index_path).expect("open");

    // Filter to agent_id=1 only
    let filter = SemanticFilter {
        agents: Some(HashSet::from([1])),
        ..Default::default()
    };

    let results = index
        .search_top_k(&[1.0, 0.0], 5, Some(&filter as &dyn SearchFilter))
        .expect("filtered search");

    assert_eq!(results.len(), 1, "should return only agent_id=1 result");
    let parsed = parse_semantic_doc_id(&results[0].doc_id).expect("parse");
    assert_eq!(parsed.agent_id, 1);
}

// =============================================================================
// LEXICAL SEARCH THROUGH FRANKENSEARCH
// =============================================================================

/// Verify that lexical search through SearchClient works end-to-end.
/// This validates the full pipeline: `frankensearch::lexical_tantivy` types → BM25 scoring.
#[test]
fn lexical_search_through_frankensearch_pipeline() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("claude_code")
        .title("frankensearch integration test")
        .source_path(dir.path().join("session.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(3)
        .with_content(0, "The authentication module handles OAuth2 flows")
        .with_content(1, "Token refresh uses exponential backoff strategy")
        .with_content(2, "Rate limiting prevents abuse of the API endpoint")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Exact term search
    let hits = client
        .search("authentication", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    assert!(
        !hits.is_empty(),
        "should find 'authentication' via frankensearch BM25"
    );
    assert!(hits[0].content.contains("authentication"));

    // Prefix wildcard search
    let hits = client
        .search("auth*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    assert!(!hits.is_empty(), "should match auth* prefix");

    // Multi-term search
    let hits = client
        .search("token refresh", filters, 10, 0, FieldMask::FULL)
        .unwrap();
    assert!(!hits.is_empty(), "should find multi-term query");
}

fn assert_lexical_sources(
    client: &SearchClient,
    query: &str,
    filters: SearchFilters,
    expected: &[&str],
) {
    let hits = client
        .search(query, filters, 100, 0, FieldMask::FULL)
        .unwrap_or_else(|error| panic!("search {query:?}: {error:#}"));
    let actual: BTreeSet<String> = hits
        .iter()
        .map(|hit| {
            std::path::Path::new(&hit.source_path)
                .file_stem()
                .expect("fixture source filename")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let expected: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(actual, expected, "query {query:?}");
    assert_eq!(hits.len(), expected.len(), "duplicate hits for {query:?}");
}

fn durable_index_hashes(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, blake3::Hash> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| entry.expect("read index directory"))
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            !name.contains("lock")
                && (entry.path().extension().is_some_and(|ext| ext == "fslx")
                    || name.contains("manifest")
                    || name.contains("schema"))
        })
        .map(|entry| {
            let bytes = std::fs::read(entry.path()).expect("read immutable index artifact");
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                blake3::hash(&bytes),
            )
        })
        .collect()
}

/// GH #548 must be fixed in the engine actually used by CASS, including the
/// application's NFC query normalization, filtering, cache, and fresh reopen.
/// This receipt deliberately pins the released generation: a dependency update
/// that moves to a new namespace does not establish the no-reindex correction.
#[test]
fn accented_latin_queries_reopen_the_released_cass_generation() {
    use coding_agent_search::sources::provenance::SourceFilter;

    assert_eq!(frankensearch::quill::cass::CASS_SCHEMA_VERSION, "v9-quill");
    assert_eq!(
        coding_agent_search::search::tantivy::SCHEMA_HASH,
        "quill-fslx-schema-v9-hyphen-cjk-bigrams-bounded-content-prefix-preview-stored-content-external"
    );
    let dir = TempDir::new().expect("accented corpus directory");
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create CASS index");
    for (name, agent, timestamp, text) in [
        (
            "hu",
            "claude_code",
            1_700_000_000_000,
            "Kérlek nézd ellenőrizd a mentést İstanbulban",
        ),
        ("de", "codex", 1_700_000_001_000, "größere Straße München"),
        ("fr", "codex", 1_700_000_002_000, "français déjà reçu"),
        ("pl", "codex", 1_700_000_003_000, "zażółć gęślą jaźń"),
        ("cs", "codex", 1_700_000_004_000, "příliš žluťoučký kůň"),
        ("ascii", "codex", 1_700_000_005_000, "plain backup"),
    ] {
        let workspace = if name == "hu" {
            "/work/árvíz".to_owned()
        } else {
            format!("/work/{name}")
        };
        let conversation = util::ConversationFixtureBuilder::new(agent)
            .title("fixture")
            .source_path(dir.path().join(format!("{name}.jsonl")))
            .workspace(workspace)
            .base_ts(timestamp)
            .messages(1)
            .with_content(0, text)
            .build_normalized();
        index
            .add_conversation(&conversation)
            .expect("ingest accented conversation");
    }
    index.commit().expect("publish CASS corpus");
    drop(index);
    let before = durable_index_hashes(dir.path());
    assert!(
        before
            .keys()
            .any(|path| path.extension().is_some_and(|ext| ext == "fslx")),
        "receipt must include published index segments"
    );

    // No writer or indexing call is involved in either reopen. Repeated
    // queries also exercise SearchClient's cache with the corrected engine.
    for _ in 0..2 {
        let client = SearchClient::open(dir.path(), None)
            .expect("open published CASS generation")
            .expect("CASS reader exists");
        for (query, expected) in [
            ("kérlek", vec!["hu"]),
            ("KÉRLEK", vec!["hu"]),
            ("ke\u{301}rlek", vec!["hu"]),
            ("ellenőrizd", vec!["hu"]),
            ("İSTANBULBAN", vec!["hu"]),
            ("GRÖẞERE", vec!["de"]),
            ("Straße", vec!["de"]),
            ("FRANÇAIS", vec!["fr"]),
            ("zażółć", vec!["pl"]),
            ("příliš", vec!["cs"]),
            ("kérl*", vec!["hu"]),
            ("kérlek OR backup", vec!["hu", "ascii"]),
            ("kérlek OR 🚀", vec!["hu"]),
            ("kérlek OR \"\"", vec!["hu"]),
            ("kérlek OR *", vec!["hu"]),
            ("kérlek OR \"*\"", vec!["hu"]),
            ("kérlek AND backup", vec![]),
            ("kérlek AND \"\"", vec![]),
            ("kérlek AND \"   \"", vec![]),
            ("kérlek NOT \"\"", vec!["hu"]),
            ("(kérlek OR backup) -backup", vec!["hu"]),
            ("NOT kérlek", vec!["de", "fr", "pl", "cs", "ascii"]),
            ("NOT é", vec!["hu", "de", "fr", "pl", "cs", "ascii"]),
            ("NOT \"\"", vec!["hu", "de", "fr", "pl", "cs", "ascii"]),
            ("backup", vec!["ascii"]),
            ("привет", vec![]),
            ("é", vec![]),
            ("é*", vec![]),
            ("🚀", vec![]),
            ("🚀*", vec![]),
            ("\"\"", vec![]),
            ("\"   \"", vec![]),
            ("!***", vec![]),
            ("*", vec!["hu", "de", "fr", "pl", "cs", "ascii"]),
            (
                "\"*** \u{2003}\"",
                vec!["hu", "de", "fr", "pl", "cs", "ascii"],
            ),
            ("*érlek", vec![]),
            ("ké*rlek", vec![]),
            ("\"ké*rlek\"", vec![]),
            ("\"ké*rlek nézd\"", vec![]),
        ] {
            assert_lexical_sources(&client, query, SearchFilters::default(), &expected);
        }

        for filters in [
            SearchFilters {
                agents: HashSet::from(["claude_code".to_owned()]),
                ..SearchFilters::default()
            },
            SearchFilters {
                workspaces: HashSet::from(["/work/árvíz".to_owned()]),
                ..SearchFilters::default()
            },
            SearchFilters {
                created_from: Some(1_700_000_000_000),
                created_to: Some(1_700_000_000_000),
                ..SearchFilters::default()
            },
            SearchFilters {
                session_paths: HashSet::from([dir
                    .path()
                    .join("hu.jsonl")
                    .to_string_lossy()
                    .into_owned()]),
                ..SearchFilters::default()
            },
        ] {
            assert_lexical_sources(&client, "kérlek OR backup", filters, &["hu"]);
        }
        assert_lexical_sources(
            &client,
            "kérlek",
            SearchFilters {
                source_filter: SourceFilter::SourceId("absent-source".to_owned()),
                ..SearchFilters::default()
            },
            &[],
        );
        assert_eq!(
            durable_index_hashes(dir.path()),
            before,
            "query normalization must not rewrite segments, manifests, or schema markers"
        );
    }
}

/// Fragment expansion must preserve phrase positions and field boundaries.
/// Bare words retain compound-term AND semantics within one field, whereas
/// quoted phrases reject scattered fragments and reversed word ordering.
#[test]
fn accented_latin_queries_keep_phrase_and_boolean_boundaries() {
    let dir = TempDir::new().expect("accented phrase corpus directory");
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create CASS index");
    for (name, title, content) in [
        ("ordered", "fixture", "Kérlek nézd"),
        ("reversed", "fixture", "nézd kérlek"),
        ("gap", "fixture", "k rlek unrelated n zd"),
        ("scattered", "fixture", "k unrelated rlek n unrelated zd"),
        ("crossfield", "k", "rlek nézd"),
        ("prefixonly", "fixture", "king rlekin"),
        ("suffixword", "fixture", "k rlekin"),
        ("hyphen", "fixture", "Kérlek--nézd"),
        ("ascii", "fixture", "plain backup"),
    ] {
        let conversation = util::ConversationFixtureBuilder::new("claude_code")
            .title(title)
            .source_path(dir.path().join(format!("{name}.jsonl")))
            .messages(1)
            .with_content(0, content)
            .build_normalized();
        index
            .add_conversation(&conversation)
            .expect("ingest phrase conversation");
    }
    index.commit().expect("publish phrase corpus");
    drop(index);
    let client = SearchClient::open(dir.path(), None)
        .expect("open phrase corpus")
        .expect("phrase reader exists");

    let first_page = client
        .search_with_fallback("kérlek", SearchFilters::default(), 1, 0, 0, FieldMask::FULL)
        .expect("first uncached accented page");
    assert_eq!(first_page.hits.len(), 1);
    assert_eq!(first_page.total_count, Some(5));

    for (query, expected) in [
        ("\"kérlek nézd\"", vec!["ordered", "hyphen"]),
        ("\"kérlek !!! nézd\"", vec!["ordered", "hyphen"]),
        ("\"kérlek 🚀 nézd\"", vec!["ordered", "hyphen"]),
        ("\"nézd kérlek\"", vec!["reversed"]),
        (
            "kérlek",
            vec!["ordered", "reversed", "gap", "scattered", "hyphen"],
        ),
        (
            "kérl*",
            vec![
                "ordered",
                "reversed",
                "gap",
                "scattered",
                "hyphen",
                "suffixword",
            ],
        ),
        (
            "kérlek -\"kérlek nézd\"",
            vec!["reversed", "gap", "scattered"],
        ),
        (
            "(kérlek OR backup) AND -\"kérlek nézd\"",
            vec!["reversed", "gap", "scattered", "ascii"],
        ),
        ("\"kérlek é nézd\"", vec![]),
        ("\"kérlek привет nézd\"", vec![]),
        ("kérlek AND 🚀", vec![]),
        ("kérlek AND \"\"", vec![]),
        ("kérlek AND \"AND\"", vec![]),
        (
            "kérlek OR \"NOT\"",
            vec!["ordered", "reversed", "gap", "scattered", "hyphen"],
        ),
    ] {
        assert_lexical_sources(&client, query, SearchFilters::default(), &expected);
    }
    // A refused expansion must reject the entire expression; turning it into
    // a missing operand would broaden NOT and OR into unrelated matches.
    for query in [
        format!("NOT {}", "aé".repeat(257)),
        format!("{} OR backup", "aé".repeat(257)),
        format!("NOT {}é", "a".repeat(257)),
        format!("{} OR backup", "aé".repeat(5_001)),
        format!("{}kérlek{}", "(".repeat(129), ")".repeat(129)),
        format!("backup OR \"é {}\"", "a".repeat(257)),
        format!("NOT \"{} café\"", "a".repeat(257)),
    ] {
        assert_lexical_sources(&client, &query, SearchFilters::default(), &[]);
    }
    let positive = client
        .search("kérlek", SearchFilters::default(), 100, 0, FieldMask::FULL)
        .expect("positive score control")
        .into_iter()
        .map(|hit| (hit.source_path, hit.score))
        .collect::<BTreeMap<_, _>>();
    let excluded = client
        .search(
            "kérlek -\"kérlek nézd\"",
            SearchFilters::default(),
            100,
            0,
            FieldMask::FULL,
        )
        .expect("negated phrase scores");
    for hit in excluded {
        assert!(hit.score.is_finite() && hit.score > 0.0);
        assert!(
            (hit.score - positive[&hit.source_path]).abs() < 1e-6,
            "an exclusion must not add a match-all score"
        );
    }
    let sparse = client
        .search_with_fallback(
            "kérlek",
            SearchFilters::default(),
            100,
            0,
            10,
            FieldMask::FULL,
        )
        .expect("sparse accented fallback");
    assert_eq!(sparse.hits.len(), 5);
    assert!(
        !sparse.wildcard_fallback,
        "unsupported accent substring retry must retain exact hits"
    );
}

#[test]
fn accented_words_keep_their_field_boundary_in_cjk_phrase_fallback() {
    let dir = TempDir::new().expect("mixed-script corpus directory");
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create CASS index");
    for (name, title, content) in [
        ("mixed", "fixture", "Kérlek 日本語"),
        ("splitword", "k", "rlek 日本語"),
        ("prefixword", "k", "rlektor 日本語"),
        ("otherfield", "Kérlek", "日本語"),
    ] {
        let conversation = util::ConversationFixtureBuilder::new("codex")
            .title(title)
            .source_path(dir.path().join(format!("{name}.jsonl")))
            .messages(1)
            .with_content(0, content)
            .build_normalized();
        index
            .add_conversation(&conversation)
            .expect("ingest mixed-script conversation");
    }
    index.commit().expect("publish mixed-script corpus");
    drop(index);
    let client = SearchClient::open(dir.path(), None)
        .expect("open mixed-script corpus")
        .expect("mixed-script reader exists");
    for query in ["kérlek", "\"kérlek 日本語\"", "\"日本語 kérlek\""] {
        assert_lexical_sources(
            &client,
            query,
            SearchFilters::default(),
            &["mixed", "otherfield"],
        );
    }
}

/// No-mock schema-v8 compatibility receipt spanning the operations CASS needs
/// to survive the facade flip: create, ingest, commit, reopen, merge, query,
/// drop, and fresh reopen.
#[test]
// Merges two independently-built shard directories, which is the Tantivy-only
// N-directory merge. Quill's concat_merge joins segments within ONE manifest,
// so the staged-shard strategy that needed this is disabled for the Quill
// backend. Ignored rather than weakened; see the `staged_shard_plan` binding.
#[ignore = "Tantivy-only shard merge; unreachable on the Quill backend"]
fn cass_schema_v8_survives_create_merge_query_and_fresh_reopen() {
    assert_eq!(
        frankensearch::lexical_tantivy::CASS_SCHEMA_VERSION,
        "v8",
        "the compatibility receipt is specifically for CASS schema v8"
    );

    let tracker = PhaseTracker::new(
        "search_frankensearch_integration",
        "cass_schema_v8_survives_create_merge_query_and_fresh_reopen",
    );
    let root = TempDir::new().expect("compatibility fixture root");
    let left_path = root.path().join("left");
    let right_path = root.path().join("right");
    let merged_path = root.path().join("merged");

    tracker.phase(
        "create_ingest_commit",
        "build two real schema-v8 shards",
        || {
            let mut left = TantivyIndex::open_or_create(&left_path).expect("create left shard");
            let left_conversation = util::ConversationFixtureBuilder::new("claude_code")
                .title("left compatibility shard")
                .source_path(root.path().join("left.jsonl"))
                .base_ts(1_700_000_000_000)
                .messages(1)
                .with_content(0, "quill facade migration keeps oauth lexical history")
                .build_normalized();
            left.add_conversation(&left_conversation)
                .expect("ingest left shard");
            left.commit().expect("commit left shard");

            let mut right = TantivyIndex::open_or_create(&right_path).expect("create right shard");
            let right_conversation = util::ConversationFixtureBuilder::new("codex")
                .title("right compatibility shard")
                .source_path(root.path().join("right.jsonl"))
                .base_ts(1_700_000_001_000)
                .messages(1)
                .with_content(0, "tantivy schema eight survives restart and merge")
                .build_normalized();
            right
                .add_conversation(&right_conversation)
                .expect("ingest right shard");
            right.commit().expect("commit right shard");
        },
    );

    tracker.phase(
        "reopen_merge",
        "reopen both shards and merge real index files",
        || {
            let left = TantivyIndex::open_or_create(&left_path).expect("reopen left shard");
            let right = TantivyIndex::open_or_create(&right_path).expect("reopen right shard");
            assert_eq!(
                left.reader()
                    .expect("left reader")
                    .doc_count()
                    .expect("doc count"),
                1
            );
            assert_eq!(
                right
                    .reader()
                    .expect("right reader")
                    .doc_count()
                    .expect("doc count"),
                1
            );
            drop((left, right));

            let merged = TantivyIndex::merge_compatible_index_directories(
                &merged_path,
                &[left_path.as_path(), right_path.as_path()],
            )
            .expect("merge compatible schema-v8 shards");
            assert_eq!(
                merged
                    .reader()
                    .expect("merged reader")
                    .doc_count()
                    .expect("doc count"),
                2
            );
        },
    );

    tracker.phase(
        "query_restart",
        "query, drop every owner, and query after fresh reopen",
        || {
            for (query, expected_agent, expected_content) in [
                (
                    "oauth",
                    "claude_code",
                    "quill facade migration keeps oauth lexical history",
                ),
                (
                    "schema eight",
                    "codex",
                    "tantivy schema eight survives restart and merge",
                ),
            ] {
                let client = SearchClient::open(&merged_path, None)
                    .expect("open merged search client")
                    .expect("merged search client exists");
                let hits = client
                    .search(query, SearchFilters::default(), 10, 0, FieldMask::FULL)
                    .expect("query merged index");
                assert_eq!(hits.len(), 1, "query {query:?} must find its source shard");
                assert_eq!(hits[0].agent, expected_agent);
                assert_eq!(hits[0].content, expected_content);
                drop(client);

                let restarted = SearchClient::open(&merged_path, None)
                    .expect("fresh reopen merged search client")
                    .expect("freshly reopened search client exists");
                let restarted_hits = restarted
                    .search(query, SearchFilters::default(), 10, 0, FieldMask::FULL)
                    .expect("query after fresh reopen");
                assert_eq!(
                    restarted_hits.len(),
                    1,
                    "query {query:?} must survive fresh reopen"
                );
                assert_eq!(restarted_hits[0].agent, expected_agent);
                assert_eq!(restarted_hits[0].content, expected_content);
            }
        },
    );

    tracker.verbose(&format!(
        "backend=tantivy namespace=lexical_tantivy schema_version={} documents=2",
        frankensearch::lexical_tantivy::CASS_SCHEMA_VERSION
    ));
    tracker.complete();
}

/// Verify agent filter works through the frankensearch pipeline.
#[test]
fn agent_filter_through_frankensearch_pipeline() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv_claude = util::ConversationFixtureBuilder::new("claude_code")
        .title("claude session")
        .source_path(dir.path().join("claude.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(0, "debugging the database connection pool")
        .build_normalized();

    let conv_codex = util::ConversationFixtureBuilder::new("codex")
        .title("codex session")
        .source_path(dir.path().join("codex.jsonl"))
        .base_ts(1_700_000_001_000)
        .messages(1)
        .with_content(0, "debugging the cache invalidation logic")
        .build_normalized();

    index.add_conversation(&conv_claude).unwrap();
    index.add_conversation(&conv_codex).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    // Search with agent filter
    let mut filters = SearchFilters::default();
    filters.agents.insert("claude_code".to_string());

    let hits = client
        .search("debugging", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Should only find claude_code results
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(
            hit.agent, "claude_code",
            "agent filter should only return claude_code results"
        );
    }
}

// =============================================================================
// RRF FUSION VERIFICATION
// =============================================================================

/// Verify that frankensearch rrf_fuse is available and produces valid scores.
/// This tests the function signature and basic correctness, not the full hybrid
/// pipeline (which requires both lexical and semantic indexes).
#[test]
fn frankensearch_rrf_fuse_produces_valid_scores() {
    use frankensearch::{RrfConfig, ScoreSource, ScoredResult, VectorHit, rrf_fuse};

    let lexical_results = vec![
        ScoredResult {
            doc_id: "doc_a".into(),
            score: 10.0,
            source: ScoreSource::Lexical,
            index: None,
            fast_score: None,
            quality_score: None,
            lexical_score: Some(10.0),
            rerank_score: None,
            explanation: None,
            metadata: None,
        },
        ScoredResult {
            doc_id: "doc_b".into(),
            score: 5.0,
            source: ScoreSource::Lexical,
            index: None,
            fast_score: None,
            quality_score: None,
            lexical_score: Some(5.0),
            rerank_score: None,
            explanation: None,
            metadata: None,
        },
    ];

    let semantic_results = vec![
        VectorHit {
            index: 0,
            score: 0.95,
            doc_id: "doc_b".into(),
        },
        VectorHit {
            index: 1,
            score: 0.8,
            doc_id: "doc_c".into(),
        },
    ];

    let config = RrfConfig {
        k: 60.0,
        ..RrfConfig::default()
    };
    let fused = rrf_fuse(&lexical_results, &semantic_results, 100, 0, &config);

    assert!(!fused.is_empty(), "RRF fusion should produce results");

    // doc_b appears in both lists, so should have highest RRF score
    let top = &fused[0];
    assert_eq!(
        top.doc_id, "doc_b",
        "doc_b should be ranked highest (appears in both lists)"
    );

    // Verify all scores are positive
    for result in &fused {
        assert!(result.rrf_score > 0.0, "RRF scores should be positive");
    }

    // Bead 7k7pl: pin the EXACT set of doc_ids, not just presence of
    // each. The fusion contract says: given two input lists with doc_a
    // + doc_b and doc_b + doc_c, RRF must produce EXACTLY {doc_a,
    // doc_b, doc_c} — no extras (would indicate phantom docs leaking
    // from another source), no missing ids (would indicate a dedup
    // bug). Three separate `.contains()` probes accept a regression
    // that also introduces extra doc_ids.
    let doc_ids: HashSet<&str> = fused.iter().map(|r| r.doc_id.as_str()).collect();
    let expected: HashSet<&str> = ["doc_a", "doc_b", "doc_c"].into_iter().collect();
    assert_eq!(
        doc_ids, expected,
        "RRF fusion output must contain EXACTLY the union of the two input \
         lists with no phantom ids; got {:?}",
        doc_ids
    );
}

// =============================================================================
// SEARCH RESULT CONSISTENCY
// =============================================================================

/// Verify that multiple searches with the same query produce identical results.
/// This tests determinism of the frankensearch pipeline.
#[test]
fn search_results_are_deterministic() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("claude_code")
        .title("determinism test")
        .source_path(dir.path().join("session.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(5)
        .with_content(0, "error handling in the authentication module")
        .with_content(1, "authentication token validation logic")
        .with_content(2, "error recovery from network failures")
        .with_content(3, "database query optimization techniques")
        .with_content(4, "authentication flow diagram and documentation")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Run same query 3 times
    let hits1 = client
        .search("authentication", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let hits2 = client
        .search("authentication", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let hits3 = client
        .search("authentication", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Same number of results
    assert_eq!(hits1.len(), hits2.len());
    assert_eq!(hits2.len(), hits3.len());

    // Same ordering (compare source_path + line_number as stable identifiers)
    for i in 0..hits1.len() {
        assert_eq!(
            hits1[i].source_path, hits2[i].source_path,
            "result {i} source_path should be deterministic"
        );
        assert_eq!(
            hits1[i].line_number, hits2[i].line_number,
            "result {i} line_number should be deterministic"
        );
    }
}

/// Verify SearchClient produces results with expected field population.
#[test]
fn search_results_have_expected_fields() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("claude_code")
        .title("field test session")
        .source_path(dir.path().join("session.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(
            0,
            "testing that all search hit fields are populated correctly",
        )
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("testing", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(!hits.is_empty());
    let hit = &hits[0];

    assert!(!hit.content.is_empty(), "content should be populated");
    assert!(
        !hit.source_path.is_empty(),
        "source_path should be populated"
    );
    assert!(!hit.agent.is_empty(), "agent should be populated");
    assert_eq!(hit.agent, "claude_code");
    assert!(hit.score > 0.0, "score should be positive");
}
