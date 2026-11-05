//! Bind a completed semantic build to its committed canonical source.
//!
//! Builders keep the existing mutable per-embedder FSVI and watermarks. Serving
//! owns separately sealed files; a checkpoint or watermark is never sufficient
//! evidence that an older vector can be returned against a newer archive.

use super::*;
use crate::search::semantic_coverage::{
    SemanticParentArtifact, SemanticParentReader, ensure_same_file_metadata,
    open_semantic_proof_file, semantic_file_sha256, write_semantic_parents_with_base,
};
use crate::search::semantic_manifest::SemanticCoverageCertificate;
use crate::storage::sqlite::SemanticArchiveState;
use anyhow::ensure;
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub(crate) struct SemanticBuildSnapshot {
    archive: BackfillFilePair,
    pub(crate) state: SemanticArchiveState,
    pub(crate) max_message_id: i64,
    pub(crate) db_fingerprint: String,
    pub(crate) conversation_count: u64,
    parents: Option<SemanticParentArtifact>,
    base: Option<SemanticCoverageCertificate>,
}

impl SemanticBuildSnapshot {
    /// Capture before selecting any canonical inputs. Unsupported external
    /// producers/platforms and caller-owned transactions keep strict admission.
    pub(crate) fn capture(storage: &FrankenStorage, embedder_id: &str) -> Result<Option<Self>> {
        ensure!(
            !storage.raw().as_async().in_transaction(),
            "semantic maintenance requires committed canonical rows, not a caller-owned transaction"
        );
        if crate::search::external_embedder::is_external_identity(embedder_id) {
            return Ok(None);
        }
        let Some(archive) = BackfillFilePair::archive(storage) else {
            return Ok(None);
        };
        let Some(state) = storage.semantic_archive_state()? else {
            return Ok(None);
        };
        let count = crate::indexer::count_total_conversations_exact(storage)?;
        let snapshot = Self {
            archive,
            state,
            max_message_id: storage.max_message_id()?.unwrap_or(0),
            db_fingerprint: crate::indexer::lexical_rebuild_content_fingerprint(storage, count)?,
            conversation_count: u64::try_from(count).context("negative conversation count")?,
            parents: None,
            base: None,
        };
        snapshot.verify(storage)?;
        Ok(Some(snapshot))
    }

    pub(crate) fn verify(&self, storage: &FrankenStorage) -> Result<()> {
        ensure!(
            BackfillFilePair::archive(storage).as_ref() == Some(&self.archive),
            "canonical archive changed during semantic build; retry maintenance"
        );
        // An independent read-only observer also detects callers retaining an
        // old database snapshot. Never certify their uncommitted transaction.
        let observer = FrankenStorage::open_readonly(&self.archive.path)?;
        ensure!(
            observer.semantic_archive_state()?.as_ref() == Some(&self.state)
                && observer.max_message_id()?.unwrap_or(0) == self.max_message_id
                && BackfillFilePair::archive(&observer).as_ref() == Some(&self.archive)
                && BackfillFilePair::archive(storage).as_ref() == Some(&self.archive),
            "canonical semantic build boundary changed"
        );
        Ok(())
    }

    /// Admit a tail only from a certified, compact, byte-identical base. An
    /// established legacy tier instead upgrades through bounded canonical
    /// reconciliation; its watermark cannot certify the skipped prefix.
    pub(crate) fn admit_incremental_base(
        &mut self,
        storage: &FrankenStorage,
        data_dir: &Path,
        embedder_id: &str,
        dimension: usize,
    ) -> Result<bool> {
        self.verify(storage)?;
        let Some(manifest) = SemanticManifest::load(data_dir)? else {
            return Ok(false);
        };
        let Some(certificate) = manifest.coverage.get(embedder_id) else {
            return Ok(false);
        };
        let artifact = [&manifest.fast_tier, &manifest.quality_tier]
            .into_iter()
            .flatten()
            .find(|artifact| {
                artifact.embedder_id == embedder_id && certificate.matches_artifact(artifact)
            });
        let Some(artifact) = artifact else {
            return Ok(false);
        };
        if !certificate.matches_artifact(artifact)
            || certificate.archive_id != self.state.archive_id
            || certificate.mutation_seq > self.state.mutation_seq
            || certificate.max_message_id > self.max_message_id
            || artifact.dimension != dimension
            || artifact.schema_version != SEMANTIC_SCHEMA_VERSION
            || artifact.chunking_version != CHUNKING_STRATEGY_VERSION
            || Some(certificate.vector_space_revision.as_str())
                != expected_vector_space_revision(embedder_id)
            || storage.get_last_embedded_message_id(embedder_id)?
                != Some(certificate.max_message_id)
            || storage.semantic_invalidations_after(&SemanticArchiveState {
                archive_id: certificate.archive_id.clone(),
                mutation_seq: certificate.mutation_seq,
            })?
        {
            return Ok(false);
        }
        let mut served = open_semantic_proof_file(data_dir, &certificate.artifact_path)?;
        if semantic_file_sha256(&mut served)? != certificate.artifact_sha256 {
            return Ok(false);
        }
        SemanticParentReader::open(data_dir, certificate)?;
        let builder_path = vector_index_path(data_dir, embedder_id);
        let mut builder_file = match fs::File::open(&builder_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("inspect semantic maintenance base"),
        };
        let before = builder_file.metadata()?;
        if semantic_file_sha256(&mut builder_file)? != certificate.artifact_sha256 {
            return Ok(false);
        }
        ensure_same_file_metadata(&before, &builder_file.metadata()?)?;
        let builder = FsVectorIndex::open_read_only(&builder_path)?;
        if builder.embedder_id() != embedder_id
            || builder.embedder_revision() != certificate.vector_space_revision
            || builder.dimension() != dimension
            || builder.wal_record_count() != 0
            || u64::try_from(builder.record_count())? != certificate.doc_count
            || (0..builder.record_count()).any(|record| builder.is_deleted(record))
        {
            return Ok(false);
        }
        self.verify(storage)?;
        self.base = Some(certificate.clone());
        Ok(true)
    }

    pub(crate) fn base_artifact_sha256(&self) -> Option<&str> {
        self.base.as_ref().map(|base| base.artifact_sha256.as_str())
    }

    pub(crate) fn is_unchanged_base(&self) -> bool {
        self.base.as_ref().is_some_and(|base| {
            base.mutation_seq == self.state.mutation_seq
                && base.max_message_id == self.max_message_id
        })
    }

    /// Freeze parent provenance before embedding. Incremental publication must
    /// retain every inherited parent unchanged, including IDs below the tail.
    pub(crate) fn prepare_parents(
        &mut self,
        storage: &FrankenStorage,
        data_dir: &Path,
    ) -> Result<()> {
        self.verify(storage)?;
        let parents =
            write_semantic_parents_with_base(storage.raw(), data_dir, self.base.as_ref())?;
        self.verify(storage)?;
        self.parents = Some(parents);
        Ok(())
    }

    /// Seal a compact completed artifact without replacing an existing served
    /// inode. The final document sequence must be the one admitted by the
    /// canonical build or by the append's writer-lease receipt.
    pub(crate) fn seal(
        &self,
        data_dir: &Path,
        index_path: &Path,
        mut artifact: ArtifactRecord,
        expected_documents: &str,
    ) -> Result<(ArtifactRecord, SemanticCoverageCertificate)> {
        let parents = self
            .parents
            .as_ref()
            .context("semantic parent proof was not prepared")?;
        let directory = data_dir.join("vector_index/served");
        fs::create_dir_all(&directory)?;
        for relative in ["vector_index", "vector_index/served"] {
            let metadata = fs::symlink_metadata(data_dir.join(relative))?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "semantic serving directory must not be a symlink"
            );
        }
        let metadata = fs::symlink_metadata(index_path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "semantic build artifact must be a regular file"
        );
        let mut source = fs::File::open(index_path)?;
        let before = source.metadata()?;
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        std::io::copy(&mut source, &mut temporary)?;
        temporary.as_file().sync_all()?;
        ensure_same_file_metadata(&before, &source.metadata()?)?;
        let copied = FsVectorIndex::open_read_only(temporary.path())?;
        ensure!(
            copied.embedder_id() == artifact.embedder_id
                && copied.dimension() == artifact.dimension
                && Some(copied.embedder_revision())
                    == expected_vector_space_revision(&artifact.embedder_id)
                && u64::try_from(copied.record_count())? == artifact.doc_count
                && semantic_index_document_digest(&copied)? == expected_documents,
            "semantic build artifact changed after document-sequence admission"
        );
        let revision = copied.embedder_revision().to_owned();
        drop(copied);
        let digest = semantic_file_sha256(temporary.as_file_mut())?;
        let relative = format!("vector_index/served/{digest}.fsvi");
        let sealed = data_dir.join(&relative);
        match temporary.persist_noclobber(&sealed) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing = open_semantic_proof_file(data_dir, &relative)?;
                ensure!(
                    semantic_file_sha256(&mut existing)? == digest,
                    "existing semantic artifact digest mismatch"
                );
            }
            Err(error) => return Err(error.error).context("install immutable semantic artifact"),
        }
        // A reusing publisher must make the entry durable itself; it cannot
        // rely on a concurrent installer reaching its own directory sync.
        fs::File::open(&directory)?.sync_all()?;
        artifact.index_path = relative.clone();
        artifact.size_bytes = fs::metadata(&sealed)?.len();
        artifact.db_fingerprint = self.db_fingerprint.clone();
        artifact.conversation_count = self.conversation_count;
        let certificate = SemanticCoverageCertificate {
            artifact_path: relative,
            artifact_sha256: digest,
            parent_path: parents.path.clone(),
            parent_sha256: parents.sha256.clone(),
            parent_count: parents.count,
            archive_id: self.state.archive_id.clone(),
            mutation_seq: self.state.mutation_seq,
            max_message_id: self.max_message_id,
            db_fingerprint: self.db_fingerprint.clone(),
            vector_space_revision: revision,
            published_at_ms: artifact.completed_at_ms,
            doc_count: artifact.doc_count,
        };
        ensure!(
            certificate.matches_artifact(&artifact),
            "sealed semantic coverage does not match its artifact"
        );
        // Parent preparation precedes inference. Authenticate its installed
        // owner again before it can replace the serving ledger; corruption or
        // replacement during the build must leave the old certificate intact.
        SemanticParentReader::open(data_dir, &certificate)
            .context("authenticate semantic parent proof before publication")?;
        Ok((artifact, certificate))
    }

    pub(crate) fn certify_watermark(
        &self,
        storage: &FrankenStorage,
        embedder_id: &str,
    ) -> Result<bool> {
        storage.set_last_embedded_message_id_if_unchanged(
            embedder_id,
            self.max_message_id,
            &self.state,
        )
    }
}

/// Ordered digest of the actual compact document sequence, with unambiguous
/// framing. It keeps constant auxiliary memory and preserves native tie order.
pub(crate) fn semantic_index_document_digest(index: &FsVectorIndex) -> Result<String> {
    ensure!(
        index.wal_record_count() == 0,
        "semantic coverage requires a compact vector artifact"
    );
    let mut digest = Sha256::new();
    digest.update(b"cass-semantic-document-sequence-v1\0");
    digest.update(u64::try_from(index.record_count())?.to_le_bytes());
    for record in 0..index.record_count() {
        ensure!(
            !index.is_deleted(record),
            "semantic coverage cannot retain vector tombstones"
        );
        let document = index.doc_id_at(record)?;
        let identity = crate::search::vector_index::parse_semantic_doc_id(document)
            .context("semantic coverage requires a canonical document identity")?;
        ensure!(
            identity.content_hash.is_some(),
            "semantic coverage requires passage content hashes"
        );
        digest.update(u64::try_from(document.len())?.to_le_bytes());
        digest.update(document.as_bytes());
    }
    Ok(hex::encode(digest.finalize()))
}

/// Validate and receipt the same retained reader. Reopening a pathname between
/// set validation and digesting would allow a different generation to inherit
/// canonical authority, even if both files contain the same number of rows.
pub(crate) fn semantic_index_document_digest_for_canonical_ids(
    index: &FsVectorIndex,
    expected: &HashSet<String>,
) -> Result<String> {
    ensure!(
        index.record_count() == expected.len(),
        "semantic publication is not the complete canonical selection"
    );
    let mut seen = HashSet::with_capacity(expected.len());
    for record in 0..index.record_count() {
        let id = index.doc_id_at(record)?;
        ensure!(
            expected.contains(id) && seen.insert(id),
            "semantic publication differs from canonical identities"
        );
    }
    semantic_index_document_digest(index)
}
