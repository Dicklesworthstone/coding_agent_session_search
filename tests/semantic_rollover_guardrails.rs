//! GH #458 guardrails for a future rolling-generation serving change.
//!
//! This target exercises real manifest persistence, FSVI rebuild admission,
//! and append rejection without downloading model weights. It does NOT prove
//! that the storage backfill or CLI keeps a previous generation searchable.
//! The mutable ledger is not a serving receipt. See the accompanying rollout
//! contract for the still-missing writer/reader and hydration integration.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use coding_agent_search::indexer::semantic::{EmbeddedMessage, SemanticIndexer};
use coding_agent_search::search::fastembed_embedder::MINILM_VECTOR_SPACE_REVISION;
use coding_agent_search::search::model_manager::needs_index_rebuild;
use coding_agent_search::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use coding_agent_search::search::semantic_manifest::{
    ArtifactRecord, BuildCheckpoint, HnswRecord, SemanticManifest, TierKind,
};
use coding_agent_search::search::vector_index::{
    Quantization, ROLE_USER, VectorIndex, vector_index_path,
};

const MINILM_ID: &str = "minilm-384";
const MODEL_REVISION: &str = "c9745ed1d9f207416be6d2e6f8de32d1f16199bf";
const LEGACY_MINILM_REVISION: &str =
    "native-minilm-v1:c9745ed1d9f207416be6d2e6f8de32d1f16199bf";
const CORPUS: &str = "content-v1:3:3:3";

// These are ledger fixtures, not certificates that a native ANN graph or an
// archive was validated. The FSVI admission tests below use actual index files.
fn published_ledger() -> SemanticManifest {
    let mut manifest = SemanticManifest::default();
    manifest.publish_artifact(ArtifactRecord {
        tier: TierKind::Quality,
        embedder_id: MINILM_ID.to_owned(),
        model_revision: MODEL_REVISION.to_owned(),
        schema_version: SEMANTIC_SCHEMA_VERSION,
        chunking_version: 1,
        dimension: 384,
        doc_count: 3,
        conversation_count: 3,
        db_fingerprint: CORPUS.to_owned(),
        index_path: "vector_index/index-minilm-384.fsvi".to_owned(),
        size_bytes: 4096,
        started_at_ms: 1_700_000_000_000,
        completed_at_ms: 1_700_000_060_000,
        ready: true,
    });
    manifest.publish_hnsw(HnswRecord {
        base_tier: TierKind::Quality,
        embedder_id: MINILM_ID.to_owned(),
        ef_search: 128,
        index_path: "vector_index/hnsw-minilm-384.chsw".to_owned(),
        size_bytes: 1024,
        built_at_ms: 1_700_000_070_000,
        ready: true,
    });
    manifest.refresh_backlog(3, CORPUS);
    manifest
}

fn replacement_checkpoint() -> BuildCheckpoint {
    BuildCheckpoint {
        tier: TierKind::Quality,
        embedder_id: MINILM_ID.to_owned(),
        last_offset: 1,
        docs_embedded: 8,
        conversations_processed: 1,
        total_conversations: 3,
        db_fingerprint: CORPUS.to_owned(),
        schema_version: SEMANTIC_SCHEMA_VERSION,
        chunking_version: CHUNKING_STRATEGY_VERSION,
        saved_at_ms: 1_700_000_080_000,
        last_message_id: Some(1),
        cursor_exhausted: false,
    }
}

fn reload(data_dir: &Path) -> Result<SemanticManifest> {
    SemanticManifest::load(data_dir)?.context("saved semantic ledger is missing")
}

fn write_index(
    data_dir: &Path,
    path_embedder: &str,
    header_embedder: &str,
    revision: &str,
    dimension: usize,
) -> Result<PathBuf> {
    let path = vector_index_path(data_dir, path_embedder);
    fs::create_dir_all(path.parent().context("FSVI path has no parent")?)?;
    let mut writer = VectorIndex::create_with_revision(
        &path,
        header_embedder,
        revision,
        dimension,
        Quantization::F16,
    )?;
    let mut vector = vec![0.0; dimension];
    vector[0] = 1.0;
    writer.write_record("gh458-published-document", &vector)?;
    writer.finish()?;
    Ok(path)
}

#[test]
fn gh458_checkpoint_restart_preserves_published_ledger_and_ann_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut manifest = published_ledger();
    manifest.save(dir.path())?;
    let published = manifest.quality_tier.clone();
    let ann = manifest.hnsw.clone();
    let mut checkpoint = replacement_checkpoint();
    assert_ne!(checkpoint.chunking_version, 1);

    for (offset, documents) in [(1, 8), (2, 16)] {
        // Each iteration loads a new ledger owner, as a bounded backfill
        // invocation would. Replacement progress must not rewrite publication.
        let mut resumed = reload(dir.path())?;
        checkpoint.last_offset = offset;
        checkpoint.last_message_id = Some(offset);
        checkpoint.conversations_processed = u64::try_from(offset)?;
        checkpoint.docs_embedded = documents;
        resumed.save_checkpoint(checkpoint.clone());
        resumed.refresh_backlog(3, CORPUS);
        resumed.save(dir.path())?;

        let loaded = reload(dir.path())?;
        assert_eq!(loaded.quality_tier, published);
        assert_eq!(loaded.hnsw, ann);
        assert_eq!(loaded.checkpoint.as_ref(), Some(&checkpoint));
    }
    Ok(())
}

#[test]
fn gh458_abandoning_replacement_checkpoint_preserves_published_ledger() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut manifest = published_ledger();
    let published = manifest.quality_tier.clone();
    let ann = manifest.hnsw.clone();
    manifest.save_checkpoint(replacement_checkpoint());
    manifest.save(dir.path())?;

    let mut resumed = reload(dir.path())?;
    resumed.clear_checkpoint();
    resumed.save(dir.path())?;
    let loaded = reload(dir.path())?;
    assert!(loaded.checkpoint.is_none());
    assert_eq!(loaded.quality_tier, published);
    assert_eq!(loaded.hnsw, ann);
    Ok(())
}

#[test]
fn gh458_initial_build_progress_never_invents_a_published_tier() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut manifest = SemanticManifest::default();
    let mut checkpoint = replacement_checkpoint();
    checkpoint.conversations_processed = checkpoint.total_conversations;
    assert!(!checkpoint.is_complete());
    assert_eq!(checkpoint.progress_pct(), 99);
    manifest.save_checkpoint(checkpoint);
    manifest.refresh_backlog(3, CORPUS);
    manifest.save(dir.path())?;

    let loaded = reload(dir.path())?;
    assert!(loaded.fast_tier.is_none());
    assert!(loaded.quality_tier.is_none());
    assert!(loaded.hnsw.is_none());
    assert!(!loaded.checkpoint.context("checkpoint disappeared")?.is_complete());
    Ok(())
}

#[test]
fn gh458_rebuild_contract_stays_strict_without_erasing_retained_fsvi() -> Result<()> {
    // Read compatibility must be a separate, explicit decision. In particular,
    // accepting an old generation for queries must not make it reusable by the
    // current writer merely because its weights and dimension are unchanged.
    let cases = [
        (MINILM_ID, MINILM_VECTOR_SPACE_REVISION, 384, false),
        (MINILM_ID, LEGACY_MINILM_REVISION, 384, true),
        (
            MINILM_ID,
            "native-minilm-v1:c9745ed1d9f207416be6d2e6f8de32d1f16199bf:passages-v999",
            384,
            true,
        ),
        (
            MINILM_ID,
            "onnx-minilm-v1:c9745ed1d9f207416be6d2e6f8de32d1f16199bf:passages-v2",
            384,
            true,
        ),
        (
            MINILM_ID,
            "native-minilm-v1:0000000000000000000000000000000000000000:passages-v2",
            384,
            true,
        ),
        ("different-embedder", MINILM_VECTOR_SPACE_REVISION, 384, true),
        (MINILM_ID, MINILM_VECTOR_SPACE_REVISION, 128, true),
    ];
    for (header_embedder, revision, dimension, expected_rebuild) in cases {
        let dir = tempfile::tempdir()?;
        let path = write_index(dir.path(), MINILM_ID, header_embedder, revision, dimension)?;
        let bytes = fs::read(&path)?;
        assert_eq!(
            needs_index_rebuild(dir.path()),
            expected_rebuild,
            "header={header_embedder}, revision={revision}, dimension={dimension}"
        );
        assert_eq!(fs::read(&path)?, bytes, "rebuild inspection changed the FSVI");
    }
    Ok(())
}

#[test]
fn gh458_old_passage_contract_rejects_append_without_relabeling_vectors() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", Some(dir.path()))?;
    let path = write_index(
        dir.path(),
        "fnv1a-384",
        "fnv1a-384",
        "hash-fnv1a-modular-v1",
        384,
    )?;
    let bytes = fs::read(&path)?;
    let mut vector = vec![0.0; 384];
    vector[1] = 1.0;
    let replacement = EmbeddedMessage {
        message_id: 2,
        created_at_ms: 2,
        agent_id: 1,
        workspace_id: 0,
        source_id: 0,
        role: ROLE_USER,
        chunk_idx: 0,
        content_hash: [2; 32],
        embedding: vector,
    };
    let error = indexer
        .append_to_index([replacement], dir.path())
        .expect_err("a query fallback must not authorize cross-contract append");
    assert!(format!("{error:#}").contains("rebuild semantic vectors before appending"));
    assert_eq!(fs::read(&path)?, bytes);
    let index = VectorIndex::open_read_only(&path)?;
    assert_eq!(index.record_count(), 1);
    Ok(())
}
