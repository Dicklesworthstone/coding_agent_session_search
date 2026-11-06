//! Independently versioned conversation-title vectors.
//!
//! Each canonical conversation contributes at most one bounded title. Native
//! FSVI shards use a dedicated document discriminator; title IDs can never be
//! mistaken for message IDs. Ranking weights live in `title_fusion`, outside
//! this representation and its persisted identity.
//!
//! A partition has two publication slots. Writers replace only an inactive
//! slot, then atomically select it in a small manifest. Open readers retain
//! their immutable native owners, so subsequent slot replacement cannot
//! retarget an already validated query. Successful updates retain at most two
//! files per partition, rather than an unbounded series of generations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::indexer::redact_secrets::redact_text;
use crate::indexer::semantic::expected_vector_space_revision;
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::semantic_coverage::{
    ensure_same_file_metadata, open_semantic_proof_file, semantic_file_sha256,
};
use crate::search::semantic_manifest::{persist_named_temp, sync_parent_directory};
use crate::search::vector_index::{Quantization, VectorIndex};
use crate::storage::sqlite::SemanticArchiveState;

pub const TITLE_REPRESENTATION_VERSION: &str = "bounded-title-v1";
pub const TITLE_MAX_SCAN_CHARS: usize = 512;
pub const TITLE_MAX_INPUT_CHARS: usize = 128;
pub const TITLE_PARTITIONS: u16 = 256;
pub const TITLE_MAX_PARTITION_RECORDS: usize = 4_096;
pub const TITLE_MAX_RECORDS: usize = TITLE_PARTITIONS as usize * TITLE_MAX_PARTITION_RECORDS;
pub const TITLE_MANIFEST_MAX_BYTES: u64 = 256 * 1024;
pub const TITLE_SHARD_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const TITLE_SCHEMA_VERSION: u32 = 1;

/// Native inference identity, excluding the independent body passage policy.
/// Same-dimensional vectors from another execution/model revision are refused.
pub fn title_model_revision(embedder_id: &str) -> Option<&'static str> {
    if !matches!(
        embedder_id,
        "minilm-384" | "multilingual-minilm-384" | "fnv1a-384"
    ) {
        return None;
    }
    let revision = expected_vector_space_revision(embedder_id)?;
    Some(
        revision
            .split_once(":passages-")
            .map_or(revision, |(model, _)| model),
    )
}

pub fn title_vector_revision(embedder_id: &str) -> Result<String> {
    let model = title_model_revision(embedder_id)
        .context("independent titles require a supported native or hash vector space")?;
    Ok(format!("{model}:{TITLE_REPRESENTATION_VERSION}"))
}

/// Bound before normalization, redaction and canonicalization. This never walks
/// the suffix of a huge exported harness instruction. The database projection
/// applies the same scalar bound before transferring title text into Rust.
pub fn normalized_title(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let bounded: String = raw
        .chars()
        .take(TITLE_MAX_SCAN_CHARS)
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let redacted = redact_text(&bounded);
    let canonical = canonicalize_for_embedding(&redacted);
    let normalized: String = canonical.chars().take(TITLE_MAX_INPUT_CHARS).collect();
    let normalized = normalized.trim();
    if normalized.is_empty()
        || matches!(
            normalized.to_ascii_lowercase().as_str(),
            "new chat" | "new conversation" | "untitled"
        )
    {
        return None;
    }
    Some(normalized.to_owned())
}

pub fn title_input_hash(raw: Option<&str>) -> Option<[u8; 32]> {
    normalized_title(raw).map(|title| content_hash(&title))
}

/// Source-qualified canonical identity. Titles and representative messages
/// have separate witnesses so a metadata-only change reuses the title vector.
#[allow(clippy::too_many_arguments)]
pub fn title_identity_hash(
    conversation_id: i64,
    agent_id: i64,
    workspace_id: Option<i64>,
    source_id: &str,
    external_id: Option<&str>,
    source_path: &str,
    started_at: Option<i64>,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"cass-title-identity-v1\0");
    hash.update(conversation_id.to_le_bytes());
    hash.update(agent_id.to_le_bytes());
    for optional in [workspace_id, started_at] {
        hash.update([u8::from(optional.is_some())]);
        hash.update(optional.unwrap_or(0).to_le_bytes());
    }
    hash.update([u8::from(external_id.is_some())]);
    for text in [source_id, external_id.unwrap_or_default(), source_path] {
        hash.update((text.len() as u64).to_le_bytes());
        hash.update(text.as_bytes());
    }
    hash.finalize().into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleDocId {
    pub conversation_id: i64,
    /// An actual canonical message, never a synthetic title carrier.
    pub representative_message_id: i64,
    /// Per-conversation destructive mutation witness, including ID reuse.
    pub invalidation_seq: u64,
    pub title_hash: [u8; 32],
    pub identity_hash: [u8; 32],
}

impl TitleDocId {
    pub fn encode(&self) -> String {
        format!(
            "t|{}|{}|{}|{}|{}",
            self.conversation_id,
            self.representative_message_id,
            self.invalidation_seq,
            hex::encode(self.title_hash),
            hex::encode(self.identity_hash)
        )
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value.len() > 256 {
            return None;
        }
        let mut parts = value.strip_prefix("t|")?.split('|');
        let conversation_id = parts.next()?.parse::<i64>().ok()?;
        let representative_message_id = parts.next()?.parse::<i64>().ok()?;
        let invalidation_seq = parts.next()?.parse::<u64>().ok()?;
        let mut title_hash = [0; 32];
        hex::decode_to_slice(parts.next()?, &mut title_hash).ok()?;
        let mut identity_hash = [0; 32];
        hex::decode_to_slice(parts.next()?, &mut identity_hash).ok()?;
        if parts.next().is_some()
            || conversation_id <= 0
            || representative_message_id <= 0
            || invalidation_seq > i64::MAX as u64
        {
            return None;
        }
        Some(Self {
            conversation_id,
            representative_message_id,
            invalidation_seq,
            title_hash,
            identity_hash,
        })
    }
}

pub fn title_partition(conversation_id: i64) -> Result<u16> {
    ensure!(
        conversation_id > 0,
        "title requires a positive canonical conversation ID"
    );
    Ok((conversation_id as u64 % u64::from(TITLE_PARTITIONS)) as u16)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleShard {
    pub slot: u8,
    pub records: usize,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleJournalCursor {
    pub mutation_seq: u64,
    pub conversation_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleIndexManifest {
    pub schema_version: u32,
    pub representation_version: String,
    pub embedder_id: String,
    pub model_revision: String,
    pub dimension: usize,
    pub archive_state: SemanticArchiveState,
    pub generation: String,
    pub indexed_titles: usize,
    /// Complete only at the captured mutation boundary, never a promise about
    /// later canonical writes. Search validates candidates in its own snapshot.
    pub complete: bool,
    pub bootstrap_after: Option<i64>,
    pub journal_cursor: TitleJournalCursor,
    pub shards: BTreeMap<u16, TitleShard>,
}

impl TitleIndexManifest {
    pub(crate) fn fresh(
        embedder_id: &str,
        dimension: usize,
        state: SemanticArchiveState,
    ) -> Result<Self> {
        let model_revision = title_model_revision(embedder_id)
            .context("title artifacts do not support this embedder")?
            .to_owned();
        Ok(Self {
            schema_version: TITLE_SCHEMA_VERSION,
            representation_version: TITLE_REPRESENTATION_VERSION.to_owned(),
            embedder_id: embedder_id.to_owned(),
            model_revision,
            dimension,
            journal_cursor: TitleJournalCursor {
                mutation_seq: state.mutation_seq,
                conversation_id: i64::MAX,
            },
            archive_state: state,
            generation: format!("{:032x}", rand::random::<u128>()),
            indexed_titles: 0,
            complete: false,
            bootstrap_after: Some(0),
            shards: BTreeMap::new(),
        })
    }

    pub fn compatible(&self, embedder_id: &str, dimension: usize) -> bool {
        self.schema_version == TITLE_SCHEMA_VERSION
            && self.representation_version == TITLE_REPRESENTATION_VERSION
            && self.embedder_id == embedder_id
            && Some(self.model_revision.as_str()) == title_model_revision(embedder_id)
            && self.dimension == dimension
    }

    pub(crate) fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema_version == TITLE_SCHEMA_VERSION,
            "unsupported title artifact schema"
        );
        ensure!(
            title_model_revision(&self.embedder_id).is_some(),
            "unsupported title artifact model"
        );
        ensure!(
            self.dimension == 384,
            "unsupported title artifact dimension"
        );
        ensure!(
            self.representation_version.len() <= 128
                && self.model_revision.len() <= 256
                && valid_hex(&self.generation, 32),
            "invalid title artifact identity"
        );
        SemanticArchiveState::parse(&serde_json::to_string(&self.archive_state)?)?;
        ensure!(
            self.journal_cursor.mutation_seq <= i64::MAX as u64
                && self.journal_cursor.conversation_id >= 0
                && self.bootstrap_after.is_none_or(|cursor| cursor >= 0)
                && (!self.complete || self.bootstrap_after.is_none()),
            "invalid title artifact maintenance cursor"
        );
        let mut records = 0usize;
        for (partition, shard) in &self.shards {
            ensure!(
                *partition < TITLE_PARTITIONS
                    && shard.slot <= 1
                    && (1..=TITLE_MAX_PARTITION_RECORDS).contains(&shard.records)
                    && (1..=TITLE_SHARD_MAX_BYTES).contains(&shard.bytes)
                    && valid_hex(&shard.sha256, 64),
                "invalid title shard metadata"
            );
            records = records
                .checked_add(shard.records)
                .context("title record count overflow")?;
        }
        ensure!(
            records == self.indexed_titles && records <= TITLE_MAX_RECORDS,
            "invalid title artifact record count"
        );
        Ok(())
    }

    pub fn live_bytes(&self) -> u64 {
        self.shards.values().map(|shard| shard.bytes).sum()
    }
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn title_index_root(data_dir: &Path, embedder_id: &str) -> Result<PathBuf> {
    ensure!(
        title_model_revision(embedder_id).is_some(),
        "unsupported title artifact model"
    );
    Ok(data_dir
        .join("vector_index")
        .join("titles")
        .join(embedder_id))
}

pub fn title_manifest_path(data_dir: &Path, embedder_id: &str) -> Result<PathBuf> {
    Ok(title_index_root(data_dir, embedder_id)?.join("current.json"))
}

pub(crate) fn title_shard_name(partition: u16, slot: u8) -> Result<String> {
    ensure!(
        partition < TITLE_PARTITIONS && slot <= 1,
        "invalid title shard slot"
    );
    Ok(format!("part-{partition:03}-{slot}.fsvi"))
}

pub fn load_title_manifest(
    data_dir: &Path,
    embedder_id: &str,
) -> Result<Option<TitleIndexManifest>> {
    let path = title_manifest_path(data_dir, embedder_id)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "title manifest must be a regular file"
    );
    let relative = path
        .strip_prefix(data_dir)?
        .to_str()
        .context("invalid title manifest path")?;
    let file = open_semantic_proof_file(data_dir, relative)?;
    ensure!(
        file.metadata()?.len() <= TITLE_MANIFEST_MAX_BYTES,
        "title manifest exceeds its byte budget"
    );
    let mut bytes = Vec::new();
    file.take(TITLE_MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= TITLE_MANIFEST_MAX_BYTES,
        "title manifest exceeds its byte budget"
    );
    let manifest: TitleIndexManifest =
        serde_json::from_slice(&bytes).context("decode title manifest")?;
    manifest.validate_shape()?;
    ensure!(
        manifest.embedder_id == embedder_id,
        "title artifact producer mismatch"
    );
    Ok(Some(manifest))
}

pub(crate) struct TitleFileLease(File);

impl Drop for TitleFileLease {
    fn drop(&mut self) {
        if let Err(error) = fs2::FileExt::unlock(&self.0) {
            tracing::warn!(%error, "failed to release title artifact lease");
        }
    }
}

fn open_lock(root: &Path, name: &str, create: bool) -> Result<File> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "title lease must be a regular file"
        ),
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .open(path)?)
}

pub(crate) fn title_writer_lease(data_dir: &Path, embedder_id: &str) -> Result<TitleFileLease> {
    let root = title_index_root(data_dir, embedder_id)?;
    // Check each application-owned component before writing any artifact.
    let relative = root.strip_prefix(data_dir)?;
    fs::create_dir_all(data_dir)?;
    let mut component_path = data_dir.to_path_buf();
    for component in relative.components() {
        component_path.push(component);
        match fs::symlink_metadata(&component_path) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "title artifact directory contains a symlink or non-directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&component_path)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    let file = open_lock(&root, "writer.lock", true)?;
    fs2::FileExt::try_lock_exclusive(&file)
        .context("title maintenance is already running; retry the bounded batch")?;
    // Created by maintenance, never by a read-only search.
    open_lock(&root, "publish.lock", true)?;
    Ok(TitleFileLease(file))
}

fn publication_lease(root: &Path, exclusive: bool) -> Result<TitleFileLease> {
    let file = open_lock(root, "publish.lock", false)?;
    if exclusive {
        fs2::FileExt::try_lock_exclusive(&file)
            .context("title reader is opening a generation; retry title publication")?;
    } else {
        fs2::FileExt::try_lock_shared(&file)
            .context("title publication is in progress; retry title search")?;
    }
    Ok(TitleFileLease(file))
}

pub(crate) fn publish_title_manifest(
    data_dir: &Path,
    manifest: &TitleIndexManifest,
    expected: Option<&TitleIndexManifest>,
) -> Result<()> {
    manifest.validate_shape()?;
    let root = title_index_root(data_dir, &manifest.embedder_id)?;
    let _lease = publication_lease(&root, true)?;
    ensure!(
        load_title_manifest(data_dir, &manifest.embedder_id)?.as_ref() == expected,
        "title manifest changed during maintenance; candidate was not published"
    );
    let bytes = serde_json::to_vec(manifest)?;
    ensure!(
        bytes.len() as u64 <= TITLE_MANIFEST_MAX_BYTES,
        "title manifest exceeds its byte budget"
    );
    let mut temporary = tempfile::Builder::new()
        .prefix(".title-current-")
        .tempfile_in(&root)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    let path = title_manifest_path(data_dir, &manifest.embedder_id)?;
    let selected = persist_named_temp(temporary, &path, true)?;
    selected.sync_all()?;
    sync_parent_directory(&path)?;
    Ok(())
}

#[derive(Debug)]
struct TitleProof {
    file: File,
    metadata: Metadata,
    sha256: String,
}

impl TitleProof {
    fn verify(&mut self) -> Result<()> {
        let current = self.file.metadata()?;
        if ensure_same_file_metadata(&self.metadata, &current).is_ok() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if current.nlink() == 0
                && self.metadata.dev() == current.dev()
                && self.metadata.ino() == current.ino()
                && self.metadata.len() == current.len()
            {
                // Atomic replacement can unlink a still-open prior slot. Its
                // ctime changes without changing bytes; certify the retained
                // descriptor once, never reopen the replacement pathname.
                ensure!(
                    semantic_file_sha256(&mut self.file)? == self.sha256,
                    "retained title vector bytes changed"
                );
                ensure_same_file_metadata(&current, &self.file.metadata()?)?;
                self.metadata = current;
                return Ok(());
            }
        }
        ensure_same_file_metadata(&self.metadata, &current)
    }
}

#[derive(Debug)]
pub struct TitleIndex {
    manifest: TitleIndexManifest,
    artifacts: Vec<Arc<VectorIndex>>,
    proofs: Mutex<Vec<TitleProof>>,
}

impl TitleIndex {
    pub fn open(data_dir: &Path, embedder_id: &str) -> Result<Option<Self>> {
        Self::open_bounded(data_dir, embedder_id, TITLE_MAX_RECORDS)
    }

    pub fn open_bounded(
        data_dir: &Path,
        embedder_id: &str,
        max_records: usize,
    ) -> Result<Option<Self>> {
        if !title_manifest_path(data_dir, embedder_id)?.exists() {
            return Ok(None);
        }
        let root = title_index_root(data_dir, embedder_id)?;
        let _lease = publication_lease(&root, false)?;
        let Some(manifest) = load_title_manifest(data_dir, embedder_id)? else {
            return Ok(None);
        };
        ensure!(
            manifest.compatible(embedder_id, 384),
            "title representation or model changed; run explicit semantic indexing"
        );
        ensure!(
            manifest.indexed_titles <= max_records,
            "title artifact exceeds canonical admission budget; increase CASS_TITLE_MAX_ADMISSION_TITLES explicitly"
        );
        let mut artifacts = Vec::with_capacity(manifest.shards.len());
        let mut proofs = Vec::with_capacity(manifest.shards.len());
        for (partition, shard) in &manifest.shards {
            let (index, proof) = open_title_shard(data_dir, &manifest, *partition, shard)?;
            artifacts.push(Arc::new(index));
            proofs.push(proof);
        }
        Ok(Some(Self {
            manifest,
            artifacts,
            proofs: Mutex::new(proofs),
        }))
    }

    pub fn manifest(&self) -> &TitleIndexManifest {
        &self.manifest
    }

    pub fn artifacts(&self) -> &[Arc<VectorIndex>] {
        &self.artifacts
    }

    pub fn verify_unchanged(&self) -> Result<()> {
        let mut proofs = self
            .proofs
            .lock()
            .map_err(|_| anyhow::anyhow!("title artifact proof lock poisoned"))?;
        for proof in proofs.iter_mut() {
            proof.verify()?;
        }
        Ok(())
    }
}

fn open_title_shard(
    data_dir: &Path,
    manifest: &TitleIndexManifest,
    partition: u16,
    shard: &TitleShard,
) -> Result<(VectorIndex, TitleProof)> {
    let root = title_index_root(data_dir, &manifest.embedder_id)?;
    let path = root.join(title_shard_name(partition, shard.slot)?);
    let relative = path
        .strip_prefix(data_dir)?
        .to_str()
        .context("invalid title shard path")?;
    let mut file = open_semantic_proof_file(data_dir, relative)?;
    let metadata = file.metadata()?;
    ensure!(metadata.len() == shard.bytes, "title shard length mismatch");
    ensure!(
        semantic_file_sha256(&mut file)? == shard.sha256,
        "title shard digest mismatch"
    );
    let index = VectorIndex::open_read_only(&path).context("open native title FSVI shard")?;
    ensure!(
        index.embedder_id() == manifest.embedder_id
            && index.embedder_revision() == title_vector_revision(&manifest.embedder_id)?
            && index.dimension() == manifest.dimension
            && index.quantization() == Quantization::F16
            && index.record_count() == shard.records
            && index.wal_record_count() == 0,
        "title shard vector-space or record-count mismatch"
    );
    let mut conversations = BTreeSet::new();
    for record in 0..index.record_count() {
        let identity = TitleDocId::parse(index.doc_id_at(record)?)
            .context("title shard contains an invalid canonical identity")?;
        ensure!(
            conversations.insert(identity.conversation_id)
                && title_partition(identity.conversation_id)? == partition,
            "title shard identities are duplicated or in the wrong partition"
        );
    }
    ensure_same_file_metadata(&metadata, &file.metadata()?)?;
    ensure_same_file_metadata(&metadata, &fs::metadata(&path)?)?;
    Ok((
        index,
        TitleProof {
            file,
            metadata,
            sha256: shard.sha256.clone(),
        },
    ))
}

/// Maintenance reads only affected partitions, never the entire title index.
pub(crate) fn read_title_partition(
    data_dir: &Path,
    manifest: &TitleIndexManifest,
    partition: u16,
) -> Result<BTreeMap<i64, (TitleDocId, Vec<f32>)>> {
    let Some(shard) = manifest.shards.get(&partition) else {
        return Ok(BTreeMap::new());
    };
    let (index, mut proof) = open_title_shard(data_dir, manifest, partition, shard)?;
    let mut records = BTreeMap::new();
    for record in 0..index.record_count() {
        let identity = TitleDocId::parse(index.doc_id_at(record)?)
            .context("invalid title record during bounded maintenance")?;
        records.insert(
            identity.conversation_id,
            (identity, index.vector_at_f32(record)?),
        );
    }
    proof.verify()?;
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_titles_normalize_redact_and_skip_only_placeholder_labels() {
        assert_eq!(normalized_title(Some("\t New chat\n")), None);
        assert_eq!(normalized_title(None), None);
        assert_eq!(normalized_title(Some("\0 \n")), None);
        assert_eq!(
            normalized_title(Some("  Cache\ncoherence\tprotocols  ")),
            Some("Cache coherence protocols".to_owned())
        );
        let huge = format!("{}SHOULD_NOT_BE_READ", "λ".repeat(TITLE_MAX_SCAN_CHARS));
        let bounded = normalized_title(Some(&huge)).expect("bounded Unicode title");
        assert_eq!(bounded.chars().count(), TITLE_MAX_INPUT_CHARS);
        assert_eq!(bounded, "λ".repeat(TITLE_MAX_INPUT_CHARS));
        let secret = "Investigate token ghp_abcdefghijklmnopqrstuvwxyz1234567890";
        assert!(
            !normalized_title(Some(secret))
                .expect("redacted title")
                .contains("abcdefghijklmnopqrstuvwxyz")
        );
    }

    #[test]
    fn title_ids_cannot_be_hydrated_as_message_ids() {
        let identity = TitleDocId {
            conversation_id: 7,
            representative_message_id: 19,
            invalidation_seq: 4,
            title_hash: [1; 32],
            identity_hash: [2; 32],
        };
        assert_eq!(TitleDocId::parse(&identity.encode()), Some(identity));
        assert!(crate::search::vector_index::parse_semantic_doc_id(&identity.encode()).is_none());
        assert!(TitleDocId::parse(&format!("{}|extra", identity.encode())).is_none());
        let invalid = TitleDocId {
            conversation_id: 0,
            ..identity
        };
        assert!(TitleDocId::parse(&invalid.encode()).is_none());
    }

    #[test]
    fn title_input_policy_is_independent_of_body_passages_and_fusion() -> Result<()> {
        assert_eq!(
            title_model_revision("fnv1a-384"),
            Some("hash-fnv1a-modular-v1")
        );
        assert!(!title_vector_revision("minilm-384")?.contains("passages"));
        assert!(title_vector_revision("minilm-384")?.contains(TITLE_REPRESENTATION_VERSION));
        Ok(())
    }
}
