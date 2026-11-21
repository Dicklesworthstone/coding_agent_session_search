//! Admission for a retained, complete legacy FSVI generation. This is separate
//! from the immutable-generation `current.json` protocol, which remains strict.

use super::*;
use crate::franken_sync::compat::RowExt;
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_coverage::{
    SemanticParentReader, ensure_same_file_metadata, open_semantic_proof_file, semantic_file_sha256,
};
use crate::search::semantic_manifest::{
    ArtifactRecord, SemanticCoverageCertificate, SemanticManifest,
};
use crate::search::vector_index::{SemanticCoverageSnapshot, parse_semantic_doc_id};
use crate::storage::sqlite::SemanticArchiveState;

/// Capture a certificate and its exact artifact in one ledger read. A malformed
/// proof must never silently downgrade to an unrelated mutable builder file.
pub fn covered_semantic_record(
    data_dir: &Path,
    embedder_id: &str,
) -> Result<Option<(ArtifactRecord, SemanticCoverageCertificate)>, String> {
    Ok(select_covered_semantic_record(data_dir, embedder_id)?
        .map(|selected| (selected.record, selected.certificate)))
}

struct CoveredSemanticSelection {
    record: ArtifactRecord,
    certificate: SemanticCoverageCertificate,
    declared_ann_path: Option<PathBuf>,
    ann_declared_ready: bool,
}

fn select_covered_semantic_record(
    data_dir: &Path,
    embedder_id: &str,
) -> Result<Option<CoveredSemanticSelection>, String> {
    let Some(manifest) = SemanticManifest::load(data_dir)
        .map_err(|error| format!("semantic coverage ledger: {error}"))?
    else {
        return Ok(None);
    };
    let Some(certificate) = manifest.coverage.get(embedder_id) else {
        return Ok(None);
    };
    if !cfg!(unix) {
        return Err("certified semantic serving requires Unix file witnesses".to_owned());
    }
    let record = [manifest.fast_tier.as_ref(), manifest.quality_tier.as_ref()]
        .into_iter()
        .flatten()
        .find(|record| record.embedder_id == embedder_id && certificate.matches_artifact(record))
        .ok_or_else(|| "semantic coverage does not match a ready artifact".to_owned())?;
    if record.schema_version != SEMANTIC_SCHEMA_VERSION
        || record.chunking_version != CHUNKING_STRATEGY_VERSION
        || Some(certificate.vector_space_revision.as_str())
            != expected_vector_space_revision(embedder_id)
    {
        return Err("semantic coverage uses an incompatible producer contract".to_owned());
    }
    // The optional accelerator must come from the SAME ledger read and tier
    // as the certified vector owner. Never rediscover a conventional filename.
    // Its native graph/source binding is still admitted by SearchClient.
    let ann = manifest
        .hnsw
        .as_ref()
        .filter(|ann| ann.base_tier == record.tier && ann.embedder_id == embedder_id);
    // Revocation prevents serving, but does not remove the declared entry.
    // Diagnostics must retain presence without advertising native readiness.
    let declared_ann_path = ann.and_then(|ann| semantic_sidecar_path(data_dir, &ann.index_path));
    let ann_declared_ready = ann.is_some_and(|ann| ann.ready);
    Ok(Some(CoveredSemanticSelection {
        record: record.clone(),
        certificate: certificate.clone(),
        declared_ann_path,
        ann_declared_ready,
    }))
}

pub(super) fn archive_snapshot(
    storage: &FrankenStorage,
    certificate: &SemanticCoverageCertificate,
) -> Result<SemanticCoverageSnapshot, String> {
    // One statement observes the authority and ID bound in the same snapshot.
    let rows = storage
        .raw()
        .query(
            "SELECT (SELECT value FROM meta WHERE key = 'semantic_archive_state_v1'),
                COALESCE((SELECT MAX(id) FROM messages), 0)",
        )
        .map_err(|error| format!("semantic archive authority: {error}"))?;
    let row = rows
        .first()
        .ok_or("semantic archive authority is missing")?;
    let state_json: Option<String> = row.get_typed(0).map_err(|error| error.to_string())?;
    let state = SemanticArchiveState::parse(
        state_json
            .as_deref()
            .ok_or("semantic archive authority is missing")?,
    )
    .map_err(|error| error.to_string())?;
    let maximum: i64 = row.get_typed(1).map_err(|error| error.to_string())?;
    if state.archive_id != certificate.archive_id
        || state.mutation_seq < certificate.mutation_seq
        || maximum < 0
    {
        return Err("semantic coverage belongs to a different canonical archive".to_owned());
    }
    Ok(SemanticCoverageSnapshot::at_snapshot(
        certificate,
        state.mutation_seq,
        maximum,
    ))
}

pub(super) fn authenticate_artifact(
    data_dir: &Path,
    record: &ArtifactRecord,
    certificate: SemanticCoverageCertificate,
    dimension: usize,
    snapshot: SemanticCoverageSnapshot,
    ann_path: Option<PathBuf>,
) -> anyhow::Result<SemanticIndexArtifact> {
    anyhow::ensure!(
        cfg!(unix)
            && certificate.matches_artifact(record)
            && record.schema_version == SEMANTIC_SCHEMA_VERSION
            && record.chunking_version == CHUNKING_STRATEGY_VERSION
            && Some(certificate.vector_space_revision.as_str())
                == expected_vector_space_revision(&record.embedder_id),
        "semantic coverage uses an incompatible publication or producer contract"
    );
    anyhow::ensure!(
        record.dimension == dimension,
        "semantic coverage dimension mismatch"
    );
    let path = data_dir.join(&certificate.artifact_path);
    let wal_path = frankensearch::index::wal_path_for(&path);
    let require_no_wal = || -> anyhow::Result<()> {
        match std::fs::symlink_metadata(&wal_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => anyhow::bail!("certified semantic artifact has a mutable WAL"),
            Err(error) => Err(error.into()),
        }
    };
    require_no_wal()?;
    let mut file = open_semantic_proof_file(data_dir, &certificate.artifact_path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.len() == record.size_bytes,
        "semantic artifact size mismatch"
    );
    anyhow::ensure!(
        semantic_file_sha256(&mut file)? == certificate.artifact_sha256,
        "semantic artifact digest mismatch"
    );
    // The legacy reader opens by path. Fence that open with descriptor and
    // pathname witnesses, and retain the authenticated descriptor with it.
    ensure_same_file_metadata(&metadata, &std::fs::symlink_metadata(&path)?)?;
    let artifact =
        open_validated_semantic_artifact(&path, ann_path, &record.embedder_id, dimension)
            .map_err(anyhow::Error::msg)?;
    ensure_same_file_metadata(&metadata, &file.metadata()?)?;
    ensure_same_file_metadata(&metadata, &std::fs::symlink_metadata(&path)?)?;
    require_no_wal()?;
    let index = artifact.index();
    anyhow::ensure!(
        index.wal_record_count() == 0 && index.tombstone_count() == 0,
        "certified semantic artifact is not compact"
    );
    anyhow::ensure!(
        u64::try_from(index.record_count())? == certificate.doc_count,
        "semantic artifact document count mismatch"
    );
    for row in 0..index.record_count() {
        let identity = parse_semantic_doc_id(index.doc_id_at(row)?)
            .ok_or_else(|| anyhow::anyhow!("certified semantic artifact has invalid identity"))?;
        anyhow::ensure!(
            identity.content_hash.is_some()
                && identity.message_id > 0
                && identity.message_id <= u64::try_from(certificate.max_message_id)?,
            "certified semantic artifact lacks a bounded passage identity"
        );
    }
    let parents = SemanticParentReader::open(data_dir, &certificate)?;
    let current_pointer = SemanticCurrentPointerV1::path(data_dir);
    let current_pointer = if current_pointer.is_absolute() {
        current_pointer
    } else {
        std::env::current_dir()?.join(current_pointer)
    };
    let artifact = artifact.with_coverage(
        certificate,
        snapshot,
        parents,
        file,
        metadata,
        current_pointer,
    );
    artifact.verify_coverage_file()?;
    Ok(artifact)
}

pub(super) fn load_context(
    data_dir: &Path,
    db_path: &Path,
    embedder_id: &str,
    dimension: usize,
    strict_read_only: bool,
    embedder: impl FnOnce() -> Result<Arc<dyn Embedder>, String>,
) -> Result<Option<SemanticContext>, String> {
    let Some(selected) = select_covered_semantic_record(data_dir, embedder_id)? else {
        return Ok(None);
    };
    let CoveredSemanticSelection {
        record,
        certificate,
        declared_ann_path,
        ann_declared_ready,
    } = selected;
    let storage = if strict_read_only {
        FrankenStorage::open_strict_readonly(db_path)
    } else {
        FrankenStorage::open_readonly(db_path)
    }
    .map_err(|error| format!("semantic archive: {error}"))?;
    let snapshot = archive_snapshot(&storage, &certificate)?;
    let ann_path = if ann_declared_ready {
        declared_ann_path.filter(|path| {
            crate::search::semantic_manifest::selection::optional_ann_path_is_safe(data_dir, path)
        })
    } else {
        None
    };
    let artifact = authenticate_artifact(
        data_dir,
        &record,
        certificate,
        dimension,
        snapshot,
        ann_path,
    )
    .map_err(|error| format!("semantic coverage admission: {error:#}"))?;
    let filter_maps = SemanticFilterMaps::from_storage(&storage)
        .map_err(|error| format!("semantic filter maps: {error}"))?;
    Ok(Some(SemanticContext {
        embedder: embedder()?,
        artifacts: vec![artifact],
        quality_artifact: None,
        filter_maps,
        roles: Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT])),
    }))
}

pub(super) fn probe(data_dir: &Path, embedder_id: &str, dimension: usize) -> Result<bool, String> {
    let Some((record, certificate)) = covered_semantic_record(data_dir, embedder_id)? else {
        return Ok(false);
    };
    // Status combines this file admission with its DB-backed coverage verdict.
    let snapshot = SemanticCoverageSnapshot::at_snapshot(
        &certificate,
        certificate.mutation_seq,
        certificate.max_message_id,
    );
    authenticate_artifact(data_dir, &record, certificate, dimension, snapshot, None)
        .map_err(|error| format!("semantic coverage admission: {error:#}"))?;
    Ok(true)
}

pub(crate) struct CoveredSemanticObservation {
    pub(crate) record: ArtifactRecord,
    pub(crate) coverage: SemanticCoverageSnapshot,
    pub(crate) ann_path: Option<PathBuf>,
}

pub(crate) fn observe_covered_semantic_artifact(
    data_dir: &Path,
    storage: &FrankenStorage,
    embedder_id: &str,
) -> Result<Option<CoveredSemanticObservation>, String> {
    let Some(selected) = select_covered_semantic_record(data_dir, embedder_id)? else {
        return Ok(None);
    };
    let CoveredSemanticSelection {
        record,
        certificate,
        declared_ann_path,
        ..
    } = selected;
    let snapshot = archive_snapshot(storage, &certificate)?;
    authenticate_artifact(
        data_dir,
        &record,
        certificate,
        record.dimension,
        snapshot.clone(),
        None,
    )
    .map_err(|error| format!("semantic coverage admission: {error:#}"))?;
    Ok(Some(CoveredSemanticObservation {
        record,
        coverage: snapshot,
        ann_path: declared_ann_path,
    }))
}
