use coding_agent_search::search::canonicalize::{canonicalize_for_embedding, content_hash};
use coding_agent_search::search::query::{MatchType, SearchHit, rrf_fuse_hits};
use coding_agent_search::search::vector_index::{
    Quantization, SearchParams, SemanticDocId, VectorIndex, parse_semantic_doc_id,
};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use serial_test::serial;
use tempfile::TempDir;

const VECTOR_DIMENSION: usize = 64;
const VECTOR_COUNT: usize = 256;
const TOP_K: usize = 10;

fn normalize_in_place(vec: &mut [f32]) {
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    let norm = norm_sq.sqrt();
    if norm > 0.0 {
        for v in vec {
            *v /= norm;
        }
    }
}

fn write_index(path: &std::path::Path) -> VectorIndex {
    let mut writer = VectorIndex::create_with_revision(
        path,
        "proptest-embedder",
        "rev",
        VECTOR_DIMENSION,
        Quantization::F16,
    )
    .expect("create writer");

    let mut vec_buf = vec![0.0f32; VECTOR_DIMENSION];
    for idx in 0..VECTOR_COUNT {
        for (d, slot) in vec_buf.iter_mut().enumerate() {
            *slot = ((idx + d * 31) % 997) as f32 / 997.0;
        }
        normalize_in_place(&mut vec_buf);
        let doc_id = SemanticDocId {
            message_id: idx as u64,
            chunk_idx: 0,
            agent_id: (idx % 8) as u32,
            workspace_id: 1,
            source_id: 1,
            role: 1,
            created_at_ms: idx as i64,
            content_hash: None,
        }
        .to_doc_id_string();
        writer
            .write_record(&doc_id, &vec_buf)
            .expect("write_record");
    }
    writer.finish().expect("finish");
    VectorIndex::open(path).expect("open index")
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
        conversation_id: None,
    }
}

#[test]
#[serial]
fn vector_search_preconvert_invariant() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("test.fsvi");
    let index = write_index(&path);

    let mut runner = TestRunner::new(ProptestConfig::with_cases(32));
    runner
        .run(&query_vector_strategy(), |query| {
            let mut q = query.clone();
            normalize_in_place(&mut q);

            let sequential = index
                .search_top_k_with_params(
                    &q,
                    TOP_K,
                    None,
                    SearchParams {
                        parallel_threshold: usize::MAX,
                        parallel_chunk_size: 128,
                        parallel_enabled: false,
                    },
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let parallel = index
                .search_top_k_with_params(
                    &q,
                    TOP_K,
                    None,
                    SearchParams {
                        parallel_threshold: 0,
                        parallel_chunk_size: 128,
                        parallel_enabled: true,
                    },
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let seq_ids: Vec<u64> = sequential
                .iter()
                .filter_map(|r| parse_semantic_doc_id(&r.doc_id).map(|p| p.message_id))
                .collect();
            let par_ids: Vec<u64> = parallel
                .iter()
                .filter_map(|r| parse_semantic_doc_id(&r.doc_id).map(|p| p.message_id))
                .collect();

            prop_assert_eq!(seq_ids, par_ids);

            for (a, b) in sequential.iter().zip(parallel.iter()) {
                let diff = (a.score - b.score).abs();
                prop_assert!(diff < 1e-6, "score mismatch: {} vs {}", a.score, b.score);
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

        let a = rrf_fuse_hits(&lexical, &semantic, "", TOP_K, 0);
        let b = rrf_fuse_hits(&lexical, &semantic, "", TOP_K, 0);

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
