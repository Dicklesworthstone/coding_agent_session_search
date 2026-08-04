//! cass#256 / bead tg5o9: post-baseline-retirement contract.
//!
//! Historically this gate pinned the `-baseline` (`--no-default-features`)
//! build's behavior: a build without the `semantic` Cargo feature had to
//! refuse `--mode semantic` cleanly because the prebuilt Microsoft ONNX
//! Runtime was not linked. cass#308 removed ONNX entirely (pure-Rust
//! frankensearch/native backend, runtime-dispatched SIMD) and bead tg5o9
//! retired the now-vacuous `semantic` feature, so today the contract is the
//! inverse:
//!
//! 1. The embedder/reranker static identity API stays stable in every build
//!    (status/asset-state surfaces read it even when no embedder is loaded).
//! 2. Semantic availability is a *runtime* question — loading from an empty
//!    model dir fails with `EmbedderUnavailable` (model files absent), never
//!    with a compile-time feature refusal.

use coding_agent_search::search::embedder::EmbedderError;
use coding_agent_search::search::fastembed_embedder::FastEmbedder;
use coding_agent_search::search::fastembed_reranker::FastEmbedReranker;

const CARGO_MANIFEST: &str = include_str!("../Cargo.toml");
const BUILD_SCRIPT: &str = include_str!("../build.rs");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const BAKEOFF_VALIDATION_SCRIPT: &str = include_str!("../scripts/bakeoff/cass_validation_e2e.sh");
const RERANK_E2E_SCRIPT: &str = include_str!("../scripts/bakeoff/cass_rerank_e2e.sh");

#[test]
fn retired_baseline_build_machinery_does_not_return() -> Result<(), String> {
    if CARGO_MANIFEST.contains("\nsemantic = [") || CARGO_MANIFEST.contains("\nfastembed =") {
        return Err(
            "semantic support must stay native and always compiled, not return as an ONNX-era Cargo feature"
                .to_string(),
        );
    }
    if BUILD_SCRIPT.contains("emit_platform_link_hints")
        || BUILD_SCRIPT.contains("rustc-link-lib=framework=CoreML")
        || BUILD_SCRIPT.contains("CARGO_FEATURE_SEMANTIC")
    {
        return Err(
            "build.rs must not retain ORT/CoreML or semantic-feature linkage machinery".to_string(),
        );
    }
    if RELEASE_WORKFLOW.contains("cass-linux-amd64-baseline")
        || RELEASE_WORKFLOW.contains("cass-windows-amd64-baseline")
        || RELEASE_WORKFLOW.contains("matrix.cargo_flags")
        || RELEASE_WORKFLOW.contains("contains(matrix.asset_name, '-baseline')")
    {
        return Err(
            "release matrix must publish one runtime-dispatched binary per x86_64 platform"
                .to_string(),
        );
    }
    if BAKEOFF_VALIDATION_SCRIPT.contains("model.onnx")
        || RERANK_E2E_SCRIPT.contains("model.onnx")
        || !BAKEOFF_VALIDATION_SCRIPT.contains("model.safetensors")
        || !RERANK_E2E_SCRIPT.contains("model.safetensors")
    {
        return Err(
            "runnable reranker validation must require native safetensors, not retired ONNX assets"
                .to_string(),
        );
    }
    Ok(())
}

#[test]
fn cass_256_canonical_name_stable_across_features() {
    // Surface must accept the documented aliases in every build; the
    // lexical-only search path consults canonical_name regardless of
    // whether a semantic model is installed.
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
    // semantic asset state probe; drift would silently break the status
    // surface.
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

/// Semantic availability is a runtime question: `load_from_dir` against an
/// empty directory returns `EmbedderUnavailable` because the model files are
/// absent — and the reason must never claim a compile-time feature refusal
/// (the retired `-baseline` behavior).
#[test]
fn cass_256_load_from_dir_unavailable_on_empty_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = FastEmbedder::load_from_dir(tmp.path())
        .err()
        .expect("loading an empty dir must fail");
    let reason = match err {
        EmbedderError::EmbedderUnavailable { ref reason, .. } => reason.clone(),
        ref other => panic!("expected EmbedderUnavailable, got {other:?}"),
    };
    assert!(
        !reason.contains("built without"),
        "no build of cass refuses semantic at compile time anymore (tg5o9); got: {reason}"
    );
}
