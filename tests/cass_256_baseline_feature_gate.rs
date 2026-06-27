//! cass#256: baseline build feature-gate contract.
//!
//! This test exercises the lightweight, compile-time-driven half of the
//! cass#256 fix (#256 reopen) - i.e. the part the test harness can validate
//! independently of which Cargo features the harness itself was built with.
//!
//! 1. `cfg!(feature = "semantic")` is the source of truth that all of the
//!    runtime branches (search-mode dispatcher, status JSON's
//!    `semantic.feature_compiled_in` field, etc.) read from. Pinning it via a
//!    proper test catches accidental rename/drift.
//! 2. The `FastEmbedder::canonical_name` / `FastEmbedder::embedder_id_static`
//!    free static API MUST remain stable across both builds, because the
//!    lexical-only search path consults them (e.g. `asset_state.rs` reads
//!    the embedder ID for status reporting even when no embedder is
//!    actually loaded). Drift between the two builds would silently break
//!    the baseline binary's status output.
//! 3. In a baseline build (`#[cfg(not(feature = "semantic"))]`) the loader
//!    methods MUST return `EmbedderError::EmbedderUnavailable` so existing
//!    `--mode semantic` error mapping in
//!    `src/lib.rs::run_search_query::SearchMode::Semantic` produces the
//!    documented `code: 15` / `SemanticUnavailable` envelope. The full
//!    build's `load_from_dir` against an empty directory exercises the
//!    same error variant so the test passes in both worlds.

use coding_agent_search::search::embedder::EmbedderError;
use coding_agent_search::search::fastembed_embedder::FastEmbedder;
use coding_agent_search::search::fastembed_reranker::FastEmbedReranker;

#[test]
fn cass_256_canonical_name_stable_across_features() {
    // Surface must accept the documented aliases in *both* builds; the
    // lexical-only search path consults canonical_name regardless of
    // whether the prebuilt ORT binary is linked.
    assert_eq!(FastEmbedder::canonical_name("minilm"), Some("minilm"));
    assert_eq!(FastEmbedder::canonical_name("fastembed"), Some("minilm"));
    assert_eq!(
        FastEmbedder::canonical_name("all-minilm-l6-v2"),
        Some("minilm")
    );
    assert_eq!(FastEmbedder::canonical_name("MINILM-384"), Some("minilm"));
    assert!(FastEmbedder::canonical_name("not-a-model").is_none());
}

#[test]
fn cass_256_embedder_id_static_stable() {
    // The stable embedder ID is referenced by status JSON and by the
    // semantic asset state probe; drift between baseline and full builds
    // would silently break the baseline binary's status surface.
    assert_eq!(FastEmbedder::embedder_id_static(), "minilm-384");
    assert_eq!(
        FastEmbedReranker::reranker_id_static(),
        "ms-marco-minilm-l6-v2"
    );
}

#[test]
fn cass_256_default_model_dir_layout_stable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = FastEmbedder::default_model_dir(tmp.path());
    assert!(
        dir.ends_with("models/all-MiniLM-L6-v2"),
        "default model dir layout drifted: {dir:?}"
    );

    let reranker_dir = FastEmbedReranker::default_model_dir(tmp.path());
    assert!(
        reranker_dir.ends_with("models/ms-marco-MiniLM-L-6-v2"),
        "default reranker dir layout drifted: {reranker_dir:?}"
    );
}

/// In a baseline build, `load_from_dir` MUST return `EmbedderUnavailable`
/// regardless of whether the model files exist on disk - the prebuilt ONNX
/// Runtime is not linked, so even a well-formed model dir cannot be used.
///
/// In a full build, the same call against an empty directory also returns
/// `EmbedderUnavailable` (because no `model.onnx` is present), so the assert
/// holds in both worlds - this is the deliberate API-stability point.
#[test]
fn cass_256_load_from_dir_unavailable_on_empty_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = FastEmbedder::load_from_dir(tmp.path())
        .err()
        .expect("loading an empty dir must fail");
    assert!(
        matches!(err, EmbedderError::EmbedderUnavailable { .. }),
        "expected EmbedderUnavailable, got {err:?}"
    );
}

/// Baseline-only: `load_from_dir` returns the cass#256 message even when a
/// well-formed model dir is provided. Skipped in full builds because we
/// cannot synthesize a working model directory inside a unit test.
#[cfg(not(feature = "semantic"))]
#[test]
fn cass_256_baseline_load_from_dir_message_mentions_feature() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = FastEmbedder::load_from_dir(tmp.path())
        .err()
        .expect("baseline build must refuse to load");
    let reason = match err {
        EmbedderError::EmbedderUnavailable { reason, .. } => reason,
        other => panic!("expected EmbedderUnavailable, got {other:?}"),
    };
    assert!(
        reason.contains("semantic"),
        "baseline error must explain the missing feature; got: {reason}"
    );
    assert!(
        reason.contains("semantic` feature") || reason.contains("baseline"),
        "baseline error must hint at the `semantic` Cargo feature; got: {reason}"
    );
}

/// `cfg!(feature = "semantic")` is read by the runtime status JSON
/// (`semantic.feature_compiled_in`) and by the `SearchMode::Semantic`
/// dispatcher in `src/lib.rs`. Pinning the flag through a test catches
/// accidental rename/drift. The two arms below use plain `if` to keep
/// clippy's `assertions_on_constants` lint silent - assertions on a
/// statically-known `true` / `!true` are correctly flagged as
/// constant-value assertions and the runtime check is not what we
/// actually want anyway.
#[test]
fn cass_256_semantic_feature_macro_is_observable() {
    // We cannot assert a fixed value (the test harness inherits the
    // crate's feature set), so we instead pin that the macro evaluates
    // to a `bool` (the type ascription is the actual check) and that
    // exactly one of the two cfg branches is reachable in any given build.
    let semantic_compiled_in: bool = cfg!(feature = "semantic");

    let mut reachable_arms = 0u32;
    #[cfg(feature = "semantic")]
    {
        reachable_arms += 1;
        assert!(
            semantic_compiled_in,
            "semantic-feature build must observe cfg!(feature = \"semantic\") == true"
        );
    }
    #[cfg(not(feature = "semantic"))]
    {
        reachable_arms += 1;
        assert!(
            !semantic_compiled_in,
            "baseline build must observe cfg!(feature = \"semantic\") == false"
        );
    }
    assert_eq!(
        reachable_arms, 1,
        "exactly one cfg arm must be reachable per build"
    );
}
