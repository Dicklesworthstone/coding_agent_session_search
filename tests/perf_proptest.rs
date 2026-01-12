use coding_agent_search::search::canonicalize::{canonicalize_for_embedding, content_hash};
use coding_agent_search::search::query::{MatchType, SearchHit, rrf_fuse_hits};
use coding_agent_search::search::vector_index::{Quantization, VectorEntry, VectorIndex};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use serial_test::serial;
use tempfile::TempDir;

const VECTOR_DIMENSION: usize = 64;
const VECTOR_COUNT: usize = 256;
const TOP_K: usize = 10;

fn build_entries(count: usize, dimension: usize) -> Vec<VectorEntry> {
    let mut entries = Vec::with_capacity(count);
    for idx in 0..count {
        let mut vector = Vec::with_capacity(dimension);
        for d in 0..dimension {
            let value = ((idx + d * 31) % 997) as f32 / 997.0;
            vector.push(value);
        }
        entries.push(VectorEntry {
            message_id: idx as u64,
            created_at_ms: idx as i64,
            agent_id: (idx % 8) as u32,
            workspace_id: 1,
            source_id: 1,
            role: 1,
            chunk_idx: 0,
            content_hash: [0u8; 32],
            vector,
        });
    }
    entries
}

fn build_index(path: &std::path::Path) -> VectorIndex {
    let entries = build_entries(VECTOR_COUNT, VECTOR_DIMENSION);
    let index = VectorIndex::build(
        "proptest-embedder",
        "rev",
        VECTOR_DIMENSION,
        Quantization::F16,
        entries,
    )
    .expect("build index");
    index.save(path).expect("save index");
    index
}

fn query_vector_strategy() -> impl Strategy<Value = Vec<f32>> {
    prop::collection::vec(-1.0f32..1.0f32, VECTOR_DIMENSION)
}

fn text_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9 ]{10,200}",
        "# [A-Z][a-z]{3,10}\\n\\n[a-z ]{20,100}",
        "```rust\\nfn [a-z]{3,8}\\(\\) \\{\\}\\n```",
        "[a-z ]{20,50}\\n\\n```\\n[a-z]{3,10}\\n```\\n\\n[a-z ]{20,50}",
    ]
}

fn make_hit(id: &str, score: f32) -> SearchHit {
    SearchHit {
        title: id.to_string(),
        snippet: String::new(),
        content: id.to_string(),
        content_hash: 0,
        score,
        source_path: format!("/tmp/{id}.jsonl"),
        agent: "test".to_string(),
        workspace: String::new(),
        workspace_original: None,
        created_at: None,
        line_number: Some(1),
        match_type: MatchType::Exact,
        source_id: "local".to_string(),
        origin_kind: "local".to_string(),
        origin_host: None,
    }
}

#[test]
#[serial]
fn vector_search_preconvert_invariant() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("test.cvvi");
    build_index(&path);

    // Load with default pre-conversion enabled.
    unsafe { std::env::remove_var("CASS_F16_PRECONVERT") };
    let preconvert = VectorIndex::load(&path).expect("load preconvert");

    // Load with pre-conversion disabled.
    unsafe { std::env::set_var("CASS_F16_PRECONVERT", "0") };
    let mmap = VectorIndex::load(&path).expect("load mmap");
    unsafe { std::env::remove_var("CASS_F16_PRECONVERT") };

    let mut runner = TestRunner::new(ProptestConfig::with_cases(32));
    runner
        .run(&query_vector_strategy(), |query| {
            let pre_hits = preconvert
                .search_top_k(&query, TOP_K, None)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let mm_hits = mmap
                .search_top_k(&query, TOP_K, None)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let pre_ids: Vec<u64> = pre_hits.iter().map(|r| r.message_id).collect();
            let mm_ids: Vec<u64> = mm_hits.iter().map(|r| r.message_id).collect();
            prop_assert_eq!(pre_ids, mm_ids);

            for (a, b) in pre_hits.iter().zip(mm_hits.iter()) {
                let diff = (a.score - b.score).abs();
                prop_assert!(
                    diff < 1e-6,
                    "score mismatch for message {}: {} vs {}",
                    a.message_id,
                    a.score,
                    b.score
                );
            }
            Ok(())
        })
        .expect("proptest runner");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn canonicalize_is_deterministic(text in text_strategy()) {
        let first = canonicalize_for_embedding(&text);
        let second = canonicalize_for_embedding(&text);
        prop_assert_eq!(first.as_str(), second.as_str());
        prop_assert_eq!(content_hash(&first), content_hash(&second));
    }

    #[test]
    fn rrf_fusion_is_deterministic(scores in prop::collection::vec(0.0f32..1000.0, 1..20)) {
        let lexical: Vec<SearchHit> = scores
            .iter()
            .enumerate()
            .map(|(i, score)| make_hit(&format!("L{i}"), *score))
            .collect();
        let semantic: Vec<SearchHit> = scores
            .iter()
            .enumerate()
            .map(|(i, score)| make_hit(&format!("S{i}"), *score * 0.5))
            .collect();

        let a = rrf_fuse_hits(&lexical, &semantic, TOP_K, 0);
        let b = rrf_fuse_hits(&lexical, &semantic, TOP_K, 0);

        let keys_a: Vec<(String, Option<usize>)> = a
            .iter()
            .map(|h| (h.source_path.clone(), h.line_number))
            .collect();
        let keys_b: Vec<(String, Option<usize>)> = b
            .iter()
            .map(|h| (h.source_path.clone(), h.line_number))
            .collect();

        prop_assert_eq!(keys_a, keys_b);
    }
}
