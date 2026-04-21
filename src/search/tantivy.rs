use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::connectors::NormalizedConversation;
use crate::connectors::NormalizedMessage;
use crate::search::canonicalize::is_hard_message_noise;
use crate::sources::provenance::LOCAL_SOURCE_ID;
use anyhow::{Context, Error, Result};
use frankensearch::lexical::{
    CASS_SCHEMA_HASH, CASS_SCHEMA_VERSION, CassDocument as FsCassDocument,
    CassDocumentRef as FsCassDocumentRef, CassFields as FsCassFields,
    CassMergeStatus as FsCassMergeStatus, CassTantivyIndex as FsCassTantivyIndex, Index,
    IndexReader, ReloadPolicy as FsReloadPolicy, Schema, cass_build_schema as fs_build_schema,
    cass_ensure_tokenizer as fs_ensure_tokenizer, cass_fields_from_schema as fs_fields_from_schema,
    cass_index_dir as fs_index_dir, cass_open_search_reader as fs_cass_open_search_reader,
    cass_schema_hash_matches, tantivy_crate,
};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

pub(crate) fn normalized_index_source_id(
    source_id: Option<&str>,
    origin_kind: Option<&str>,
    origin_host: Option<&str>,
) -> String {
    let trimmed_source_id = source_id.unwrap_or_default().trim();
    if !trimmed_source_id.is_empty() {
        if trimmed_source_id.eq_ignore_ascii_case(LOCAL_SOURCE_ID) {
            return LOCAL_SOURCE_ID.to_string();
        }
        return trimmed_source_id.to_string();
    }

    let trimmed_origin_host = origin_host.map(str::trim).filter(|value| !value.is_empty());
    let trimmed_origin_kind = origin_kind.unwrap_or_default().trim();
    if trimmed_origin_kind.eq_ignore_ascii_case("ssh")
        || trimmed_origin_kind.eq_ignore_ascii_case("remote")
    {
        return trimmed_origin_host.unwrap_or("remote").to_string();
    }
    if let Some(origin_host) = trimmed_origin_host {
        return origin_host.to_string();
    }

    LOCAL_SOURCE_ID.to_string()
}

pub(crate) fn normalized_index_origin_kind(source_id: &str, origin_kind: Option<&str>) -> String {
    if let Some(kind) = origin_kind.map(str::trim).filter(|value| !value.is_empty()) {
        if kind.eq_ignore_ascii_case("local") {
            return LOCAL_SOURCE_ID.to_string();
        }
        if kind.eq_ignore_ascii_case("ssh") || kind.eq_ignore_ascii_case("remote") {
            return "remote".to_string();
        }
        return kind.to_ascii_lowercase();
    }

    if source_id == LOCAL_SOURCE_ID {
        LOCAL_SOURCE_ID.to_string()
    } else {
        "remote".to_string()
    }
}

pub(crate) fn normalized_index_origin_host(origin_host: Option<&str>) -> Option<String> {
    origin_host
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub const SCHEMA_HASH: &str = CASS_SCHEMA_HASH;
const DEFAULT_TANTIVY_MAX_WRITER_THREADS: usize = 26;

pub(crate) fn tantivy_writer_parallelism_hint_for_available(available_parallelism: usize) -> usize {
    let max_threads = dotenvy::var("CASS_TANTIVY_MAX_WRITER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TANTIVY_MAX_WRITER_THREADS);

    available_parallelism.max(1).clamp(1, max_threads)
}

/// Governor-aware variant of `tantivy_writer_parallelism_hint_for_available`.
/// Returns the same value on an idle host but scales down when the machine
/// responsiveness governor has shrunk the global capacity. Call this from
/// production code paths; the ungoverned `_for_available` variant is kept so
/// formula-only unit tests stay deterministic.
pub(crate) fn tantivy_writer_parallelism_hint_for_available_governed(
    available_parallelism: usize,
) -> usize {
    let raw = tantivy_writer_parallelism_hint_for_available(available_parallelism);
    crate::indexer::responsiveness::effective_worker_count(raw).max(1)
}

pub(crate) fn tantivy_writer_parallelism_hint() -> usize {
    tantivy_writer_parallelism_hint_for_available_governed(
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1),
    )
}

fn tantivy_add_batch_max_messages() -> usize {
    dotenvy::var("CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| 4_096.max(tantivy_writer_parallelism_hint().saturating_mul(512)))
}

fn tantivy_add_batch_max_chars() -> usize {
    dotenvy::var("CASS_TANTIVY_ADD_BATCH_MAX_CHARS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            (16 * 1024 * 1024)
                .max(tantivy_writer_parallelism_hint().saturating_mul(2 * 1024 * 1024))
        })
}

fn tantivy_prebuilt_add_batch_max_messages() -> usize {
    dotenvy::var("CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| 16_384.max(tantivy_writer_parallelism_hint().saturating_mul(512)))
}

fn map_fs_err(err: frankensearch::SearchError) -> Error {
    Error::new(err)
}

#[derive(Clone)]
struct CassDocContext {
    agent: String,
    workspace: Option<String>,
    workspace_original: Option<String>,
    source_path: String,
    title: Option<String>,
    started_at_fallback: Option<i64>,
    source_id: String,
    origin_kind: String,
    origin_host: Option<String>,
    conversation_id: Option<i64>,
}

fn cass_doc_context(conv: &NormalizedConversation, conversation_id: Option<i64>) -> CassDocContext {
    let cass_origin = conv.metadata.get("cass").and_then(|c| c.get("origin"));
    let raw_source_id = cass_origin
        .and_then(|o| o.get("source_id"))
        .and_then(|v| v.as_str());
    let raw_origin_kind = cass_origin
        .and_then(|o| o.get("kind"))
        .and_then(|v| v.as_str());
    let origin_host = normalized_index_origin_host(
        cass_origin
            .and_then(|o| o.get("host"))
            .and_then(|v| v.as_str()),
    );
    let source_id =
        normalized_index_source_id(raw_source_id, raw_origin_kind, origin_host.as_deref());
    let origin_kind = normalized_index_origin_kind(&source_id, raw_origin_kind);

    CassDocContext {
        agent: conv.agent_slug.clone(),
        workspace: conv
            .workspace
            .as_ref()
            .map(|ws| ws.to_string_lossy().to_string()),
        workspace_original: conv
            .metadata
            .get("cass")
            .and_then(|c| c.get("workspace_original"))
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        source_path: conv.source_path.to_string_lossy().to_string(),
        title: conv.title.clone(),
        started_at_fallback: conv.started_at,
        source_id,
        origin_kind,
        origin_host,
        conversation_id,
    }
}

fn cass_document_for_message(
    context: &CassDocContext,
    msg: &NormalizedMessage,
) -> Option<FsCassDocument> {
    if is_hard_message_noise(Some(msg.role.as_str()), &msg.content) {
        return None;
    }

    Some(FsCassDocument {
        agent: context.agent.clone(),
        workspace: context.workspace.clone(),
        workspace_original: context.workspace_original.clone(),
        source_path: context.source_path.clone(),
        msg_idx: msg.idx.max(0) as u64,
        created_at: msg.created_at.or(context.started_at_fallback),
        title: context.title.clone(),
        content: msg.content.clone(),
        conversation_id: context.conversation_id,
        source_id: context.source_id.clone(),
        origin_kind: context.origin_kind.clone(),
        origin_host: context.origin_host.clone(),
    })
}

fn push_cass_document_into_pending(
    docs: &mut Vec<FsCassDocument>,
    pending_chars: &mut usize,
    doc: FsCassDocument,
) {
    *pending_chars = pending_chars.saturating_add(doc.content.len());
    docs.push(doc);
}

/// Returns true if the given stored hash matches the current schema hash.
pub fn schema_hash_matches(stored: &str) -> bool {
    cass_schema_hash_matches(stored)
}

pub type Fields = FsCassFields;
pub type MergeStatus = FsCassMergeStatus;

const FEDERATED_SEARCH_MANIFEST_FILE: &str = "federated-search-manifest.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchableIndexSummary {
    pub docs: usize,
    pub segments: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FederatedSearchManifest {
    version: u32,
    kind: String,
    schema_hash: String,
    shards: Vec<FederatedSearchShardManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FederatedSearchShardManifest {
    relative_path: String,
    docs: usize,
    segments: usize,
    meta_fingerprint: String,
}

fn federated_search_manifest_path(index_path: &Path) -> PathBuf {
    index_path.join(FEDERATED_SEARCH_MANIFEST_FILE)
}

fn write_root_schema_hash_file(index_path: &Path) -> Result<()> {
    fs::write(
        index_path.join("schema_hash.json"),
        format!("{{\"schema_hash\":\"{CASS_SCHEMA_HASH}\"}}"),
    )
    .with_context(|| {
        format!(
            "writing cass schema hash metadata for searchable index {}",
            index_path.display()
        )
    })?;
    Ok(())
}

fn manifest_relative_shard_path(shard_idx: usize) -> String {
    format!("shards/shard-{shard_idx:05}")
}

fn meta_fingerprint_for_existing_index_dir(index_path: &Path) -> Result<String> {
    let meta_path = index_path.join("meta.json");
    let bytes = fs::read(&meta_path)
        .with_context(|| format!("reading Tantivy meta file {}", meta_path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn load_federated_search_manifest_internal(
    index_path: &Path,
) -> Result<Option<FederatedSearchManifest>> {
    let manifest_path = federated_search_manifest_path(index_path);
    match fs::read(&manifest_path) {
        Ok(bytes) => serde_json::from_slice::<FederatedSearchManifest>(&bytes)
            .with_context(|| {
                format!(
                    "parsing federated search manifest {}",
                    manifest_path.display()
                )
            })
            .map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| {
            format!(
                "reading federated search manifest {}",
                manifest_path.display()
            )
        }),
    }
}

pub fn searchable_index_exists(index_path: &Path) -> bool {
    index_path.join("meta.json").exists() || federated_search_manifest_path(index_path).exists()
}

pub fn searchable_index_modified_time(index_path: &Path) -> Option<SystemTime> {
    let meta_path = index_path.join("meta.json");
    if meta_path.exists() {
        return fs::metadata(meta_path).and_then(|m| m.modified()).ok();
    }
    fs::metadata(federated_search_manifest_path(index_path))
        .and_then(|m| m.modified())
        .ok()
}

pub fn searchable_index_fingerprint(index_path: &Path) -> Result<Option<String>> {
    let meta_path = index_path.join("meta.json");
    match fs::read(&meta_path) {
        Ok(bytes) => Ok(Some(blake3::hash(&bytes).to_hex().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let manifest_path = federated_search_manifest_path(index_path);
            match fs::read(&manifest_path) {
                Ok(bytes) => Ok(Some(blake3::hash(&bytes).to_hex().to_string())),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err).with_context(|| {
                    format!(
                        "reading federated search manifest {}",
                        manifest_path.display()
                    )
                }),
            }
        }
        Err(err) => {
            Err(err).with_context(|| format!("reading Tantivy meta file {}", meta_path.display()))
        }
    }
}

pub fn searchable_index_summary(index_path: &Path) -> Result<Option<SearchableIndexSummary>> {
    if let Some(manifest) = load_federated_search_manifest_internal(index_path)? {
        let docs = manifest.shards.iter().map(|shard| shard.docs).sum();
        let segments = manifest.shards.iter().map(|shard| shard.segments).sum();
        return Ok(Some(SearchableIndexSummary { docs, segments }));
    }

    let meta_path = index_path.join("meta.json");
    if !meta_path.exists() {
        return Ok(None);
    }

    let mut index = Index::open_in_dir(index_path).with_context(|| {
        format!(
            "opening searchable Tantivy index directory for summary: {}",
            index_path.display()
        )
    })?;
    ensure_tokenizer(&mut index);
    let (reader, _fields) =
        fs_cass_open_search_reader(index_path, FsReloadPolicy::Manual).map_err(map_fs_err)?;
    Ok(Some(SearchableIndexSummary {
        docs: reader.searcher().num_docs() as usize,
        segments: index
            .searchable_segment_metas()
            .context("reading searchable segment metadata for Tantivy summary")?
            .len(),
    }))
}

pub fn open_federated_search_readers(
    index_path: &Path,
    reload_policy: FsReloadPolicy,
) -> Result<Option<Vec<(IndexReader, Fields)>>> {
    let Some(manifest) = load_federated_search_manifest_internal(index_path)? else {
        return Ok(None);
    };

    if manifest.schema_hash != CASS_SCHEMA_HASH {
        return Err(anyhow::anyhow!(
            "federated search manifest schema mismatch: expected {}, got {}",
            CASS_SCHEMA_HASH,
            manifest.schema_hash
        ));
    }

    let readers = manifest
        .shards
        .into_iter()
        .map(|shard| {
            let shard_path = index_path.join(&shard.relative_path);
            fs_cass_open_search_reader(&shard_path, reload_policy)
                .map_err(map_fs_err)
                .with_context(|| {
                    format!(
                        "opening federated lexical shard reader {}",
                        shard_path.display()
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(readers))
}

fn materialize_federated_search_bundle_for_write(index_path: &Path) -> Result<()> {
    let Some(manifest) = load_federated_search_manifest_internal(index_path)? else {
        return Ok(());
    };

    let stage_parent = index_path.parent().unwrap_or(index_path);
    let materialize_root = tempfile::Builder::new()
        .prefix("cass-federated-materialize-")
        .tempdir_in(stage_parent)
        .with_context(|| {
            format!(
                "creating staging directory to materialize federated lexical bundle {}",
                index_path.display()
            )
        })?;
    let materialized_index_path = materialize_root.path().join("index");
    let shard_paths = manifest
        .shards
        .iter()
        .map(|shard| index_path.join(&shard.relative_path))
        .collect::<Vec<_>>();

    TantivyIndex::assemble_compatible_index_directories(&materialized_index_path, &shard_paths)
        .with_context(|| {
            format!(
                "materializing federated lexical bundle into mutable Tantivy index {}",
                index_path.display()
            )
        })?;

    if index_path.exists() {
        fs::remove_dir_all(index_path).with_context(|| {
            format!(
                "removing federated lexical bundle before mutable materialization {}",
                index_path.display()
            )
        })?;
    }
    fs::rename(&materialized_index_path, index_path).with_context(|| {
        format!(
            "publishing materialized mutable Tantivy index {} -> {}",
            materialized_index_path.display(),
            index_path.display()
        )
    })?;
    materialize_root
        .close()
        .context("closing federated lexical materialization staging directory")?;
    Ok(())
}

pub fn publish_federated_searchable_index_directories<P: AsRef<Path>>(
    output_path: &Path,
    input_paths: &[P],
) -> Result<SearchableIndexSummary> {
    if input_paths.is_empty() {
        return Err(anyhow::anyhow!(
            "cannot publish federated lexical bundle without at least one input shard"
        ));
    }
    ensure_empty_merge_output_directory(output_path)?;

    let shard_root = output_path.join("shards");
    fs::create_dir_all(&shard_root).with_context(|| {
        format!(
            "creating federated lexical shard root {}",
            shard_root.display()
        )
    })?;

    let mut manifest = FederatedSearchManifest {
        version: 1,
        kind: "cass-federated-lexical-index".to_string(),
        schema_hash: CASS_SCHEMA_HASH.to_string(),
        shards: Vec::with_capacity(input_paths.len()),
    };
    let mut total_docs = 0usize;
    let mut total_segments = 0usize;

    for (shard_idx, input_path) in input_paths.iter().enumerate() {
        let input_path = input_path.as_ref();
        let summary = searchable_index_summary(input_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "federated lexical publish input is not a searchable index: {}",
                input_path.display()
            )
        })?;
        let meta_fingerprint = meta_fingerprint_for_existing_index_dir(input_path)?;
        let relative_path = manifest_relative_shard_path(shard_idx);
        let destination_path = output_path.join(&relative_path);
        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "creating parent directory for federated lexical shard {}",
                    destination_path.display()
                )
            })?;
        }
        fs::rename(input_path, &destination_path).with_context(|| {
            format!(
                "moving staged lexical shard {} into federated publish bundle {}",
                input_path.display(),
                destination_path.display()
            )
        })?;

        total_docs = total_docs.saturating_add(summary.docs);
        total_segments = total_segments.saturating_add(summary.segments);
        manifest.shards.push(FederatedSearchShardManifest {
            relative_path,
            docs: summary.docs,
            segments: summary.segments,
            meta_fingerprint,
        });
    }

    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serializing federated search manifest")?;
    fs::write(federated_search_manifest_path(output_path), &manifest_bytes).with_context(|| {
        format!(
            "writing federated search manifest {}",
            federated_search_manifest_path(output_path).display()
        )
    })?;
    write_root_schema_hash_file(output_path)?;

    Ok(SearchableIndexSummary {
        docs: total_docs,
        segments: total_segments,
    })
}

pub struct TantivyIndex {
    inner: FsCassTantivyIndex,
    pub fields: Fields,
}

impl TantivyIndex {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        materialize_federated_search_bundle_for_write(path)?;
        let inner = FsCassTantivyIndex::open_or_create(path).map_err(map_fs_err)?;
        let fields = inner.fields();
        Ok(Self { inner, fields })
    }

    pub fn open_or_create_with_writer_parallelism(
        path: &Path,
        writer_parallelism: usize,
    ) -> Result<Self> {
        materialize_federated_search_bundle_for_write(path)?;
        let inner = FsCassTantivyIndex::open_or_create_with_writer_parallelism(
            path,
            writer_parallelism.max(1),
        )
        .map_err(map_fs_err)?;
        let fields = inner.fields();
        Ok(Self { inner, fields })
    }

    pub fn add_conversation(&mut self, conv: &NormalizedConversation) -> Result<()> {
        self.add_messages(conv, &conv.messages)
    }

    pub fn add_conversation_with_id(
        &mut self,
        conv: &NormalizedConversation,
        conversation_id: Option<i64>,
    ) -> Result<()> {
        self.add_messages_with_conversation_id(conv, &conv.messages, conversation_id)
    }

    pub fn delete_all(&mut self) -> Result<()> {
        self.inner.delete_all().map_err(map_fs_err)
    }

    pub fn commit(&mut self) -> Result<()> {
        self.inner.commit().map_err(map_fs_err)
    }

    pub fn configure_bulk_load_merge_policy(&mut self) {
        self.inner.configure_bulk_load_merge_policy();
    }

    pub fn reader(&self) -> Result<IndexReader> {
        self.inner.reader().map_err(map_fs_err)
    }

    pub fn segment_count(&self) -> usize {
        self.inner.segment_count()
    }

    pub fn merge_status(&self) -> MergeStatus {
        self.inner.merge_status()
    }

    /// Attempt to merge segments if idle conditions are met.
    /// Returns Ok(true) if merge was triggered, Ok(false) if skipped.
    pub fn optimize_if_idle(&mut self) -> Result<bool> {
        self.inner.optimize_if_idle().map_err(map_fs_err)
    }

    /// Force immediate segment merge and wait for completion.
    /// Use sparingly - blocks until merge finishes.
    pub fn force_merge(&mut self) -> Result<()> {
        self.inner.force_merge().map_err(map_fs_err)
    }

    pub fn merge_compatible_index_directories<P: AsRef<Path>>(
        output_path: &Path,
        input_paths: &[P],
    ) -> Result<Self> {
        if input_paths.is_empty() {
            return Err(anyhow::anyhow!(
                "cannot merge Tantivy index directories without at least one input"
            ));
        }
        ensure_empty_merge_output_directory(output_path)?;

        let indices = input_paths
            .iter()
            .map(|input_path| {
                let input_path = input_path.as_ref();
                let mut index = Index::open_in_dir(input_path).with_context(|| {
                    format!(
                        "opening compatible Tantivy index directory for merge: {}",
                        input_path.display()
                    )
                })?;
                ensure_tokenizer(&mut index);
                Ok(index)
            })
            .collect::<Result<Vec<_>>>()?;
        let output_directory = tantivy_crate::directory::MmapDirectory::open(output_path)
            .with_context(|| {
                format!(
                    "opening Tantivy output directory for merged index: {}",
                    output_path.display()
                )
            })?;
        let mut merged = tantivy_crate::indexer::merge_indices(&indices, output_directory)
            .with_context(|| {
                format!(
                    "merging {} compatible Tantivy index directories into {}",
                    indices.len(),
                    output_path.display()
                )
            })?;
        ensure_tokenizer(&mut merged);
        fs::write(
            output_path.join("schema_hash.json"),
            format!("{{\"schema_hash\":\"{CASS_SCHEMA_HASH}\"}}"),
        )
        .with_context(|| {
            format!(
                "writing cass schema hash metadata for merged Tantivy index {}",
                output_path.display()
            )
        })?;
        drop(merged);
        Self::open_or_create(output_path)
    }

    pub fn assemble_compatible_index_directories<P: AsRef<Path>>(
        output_path: &Path,
        input_paths: &[P],
    ) -> Result<Self> {
        if input_paths.is_empty() {
            return Err(anyhow::anyhow!(
                "cannot assemble Tantivy index directories without at least one input"
            ));
        }
        ensure_empty_merge_output_directory(output_path)?;

        let mut combined_index_meta: Option<tantivy_crate::IndexMeta> = None;
        let mut combined_segments = Vec::new();
        let mut max_opstamp = 0u64;
        let mut managed_paths = BTreeSet::new();

        for input_path in input_paths {
            let input_path = input_path.as_ref();
            let mut index = Index::open_in_dir(input_path).with_context(|| {
                format!(
                    "opening compatible Tantivy index directory for assembly: {}",
                    input_path.display()
                )
            })?;
            ensure_tokenizer(&mut index);
            let metas = index.load_metas().with_context(|| {
                format!(
                    "loading Tantivy metadata for assembled index input {}",
                    input_path.display()
                )
            })?;

            match &mut combined_index_meta {
                Some(combined_meta) => {
                    if metas.schema != combined_meta.schema {
                        return Err(anyhow::anyhow!(
                            "attempted to assemble Tantivy index directories with different schemas"
                        ));
                    }
                    if metas.index_settings != combined_meta.index_settings {
                        return Err(anyhow::anyhow!(
                            "attempted to assemble Tantivy index directories with different index settings"
                        ));
                    }
                }
                None => {
                    combined_index_meta = Some(tantivy_crate::IndexMeta {
                        index_settings: metas.index_settings.clone(),
                        segments: Vec::new(),
                        schema: metas.schema.clone(),
                        opstamp: 0,
                        payload: None,
                    });
                }
            }

            max_opstamp = max_opstamp.max(metas.opstamp);
            for segment in metas.segments {
                for relative_path in segment.list_files() {
                    let source_path = input_path.join(&relative_path);
                    if !source_path.exists() {
                        continue;
                    }
                    link_or_copy_searchable_index_file(&source_path, output_path, &relative_path)?;
                    if !managed_paths.insert(relative_path.clone()) {
                        return Err(anyhow::anyhow!(
                            "assembled Tantivy index would contain duplicate segment file path {}",
                            relative_path.display()
                        ));
                    }
                }
                combined_segments.push(segment);
            }
        }

        let mut combined_index_meta = combined_index_meta.ok_or_else(|| {
            anyhow::anyhow!("cannot assemble Tantivy index directories without index metadata")
        })?;
        combined_index_meta.segments = combined_segments;
        combined_index_meta.opstamp = max_opstamp;
        combined_index_meta.payload = Some(format!(
            "Cass assembled {} compatible Tantivy segments from {} input directories",
            combined_index_meta.segments.len(),
            input_paths.len()
        ));

        write_searchable_generation_metadata(
            output_path,
            &combined_index_meta,
            &mut managed_paths,
        )?;
        Self::open_or_create(output_path)
    }

    pub fn add_messages(
        &mut self,
        conv: &NormalizedConversation,
        messages: &[NormalizedMessage],
    ) -> Result<()> {
        self.add_messages_with_conversation_id(conv, messages, None)
    }

    pub fn add_messages_with_conversation_id(
        &mut self,
        conv: &NormalizedConversation,
        messages: &[NormalizedMessage],
        conversation_id: Option<i64>,
    ) -> Result<()> {
        self.add_messages_with_conversation_id_and_batch_hook(
            conv,
            messages,
            conversation_id,
            |_| Ok(()),
        )
    }

    pub fn add_messages_with_conversation_id_and_batch_hook<F>(
        &mut self,
        conv: &NormalizedConversation,
        messages: &[NormalizedMessage],
        conversation_id: Option<i64>,
        mut on_batch_flushed: F,
    ) -> Result<()>
    where
        F: FnMut(usize) -> Result<()>,
    {
        let context = cass_doc_context(conv, conversation_id);
        let max_messages = tantivy_add_batch_max_messages();
        let max_chars = tantivy_add_batch_max_chars();
        let mut docs: Vec<FsCassDocument> = Vec::new();
        let mut pending_chars = 0usize;

        for msg in messages {
            let Some(doc) = cass_document_for_message(&context, msg) else {
                continue;
            };
            push_cass_document_into_pending(&mut docs, &mut pending_chars, doc);
            if docs.len() >= max_messages || pending_chars >= max_chars {
                let flushed_docs = docs.len();
                self.inner.add_cass_documents(&docs).map_err(map_fs_err)?;
                on_batch_flushed(flushed_docs)?;
                docs.clear();
                pending_chars = 0;
            }
        }

        if docs.is_empty() {
            Ok(())
        } else {
            let flushed_docs = docs.len();
            self.inner.add_cass_documents(&docs).map_err(map_fs_err)?;
            on_batch_flushed(flushed_docs)
        }
    }

    pub fn add_prebuilt_documents_slice(&mut self, documents: &[FsCassDocument]) -> Result<usize> {
        let max_messages = tantivy_prebuilt_add_batch_max_messages();
        let max_chars = tantivy_add_batch_max_chars();
        let mut indexed_docs = 0usize;
        let mut batch_start = 0usize;
        let mut pending_chars = 0usize;

        for (idx, doc) in documents.iter().enumerate() {
            pending_chars = pending_chars.saturating_add(doc.content.len());
            let batch_len = idx + 1 - batch_start;
            if batch_len >= max_messages || pending_chars >= max_chars {
                let batch_end = idx + 1;
                indexed_docs = indexed_docs.saturating_add(batch_end - batch_start);
                self.inner
                    .add_cass_documents(&documents[batch_start..batch_end])
                    .map_err(map_fs_err)?;
                batch_start = batch_end;
                pending_chars = 0;
            }
        }

        if batch_start < documents.len() {
            indexed_docs = indexed_docs.saturating_add(documents.len() - batch_start);
            self.inner
                .add_cass_documents(&documents[batch_start..])
                .map_err(map_fs_err)?;
        }

        Ok(indexed_docs)
    }

    pub fn add_prebuilt_document_refs_slice<'a>(
        &mut self,
        documents: &[FsCassDocumentRef<'a>],
    ) -> Result<usize> {
        let max_messages = tantivy_prebuilt_add_batch_max_messages();
        let max_chars = tantivy_add_batch_max_chars();
        let mut indexed_docs = 0usize;
        let mut batch_start = 0usize;
        let mut pending_chars = 0usize;

        for (idx, doc) in documents.iter().enumerate() {
            pending_chars = pending_chars.saturating_add(doc.content.len());
            let batch_len = idx + 1 - batch_start;
            if batch_len >= max_messages || pending_chars >= max_chars {
                let batch_end = idx + 1;
                indexed_docs = indexed_docs.saturating_add(batch_end - batch_start);
                self.inner
                    .add_cass_document_refs(&documents[batch_start..batch_end])
                    .map_err(map_fs_err)?;
                batch_start = batch_end;
                pending_chars = 0;
            }
        }

        if batch_start < documents.len() {
            indexed_docs = indexed_docs.saturating_add(documents.len() - batch_start);
            self.inner
                .add_cass_document_refs(&documents[batch_start..])
                .map_err(map_fs_err)?;
        }

        Ok(indexed_docs)
    }

    pub fn add_prebuilt_documents<I>(&mut self, documents: I) -> Result<usize>
    where
        I: IntoIterator<Item = FsCassDocument>,
    {
        let docs = documents.into_iter().collect::<Vec<_>>();
        self.add_prebuilt_documents_slice(&docs)
    }

    pub fn add_conversations_with_ids<'a, I>(&mut self, conversations: I) -> Result<usize>
    where
        I: IntoIterator<Item = (&'a NormalizedConversation, Option<i64>)>,
    {
        let max_messages = tantivy_add_batch_max_messages();
        let max_chars = tantivy_add_batch_max_chars();
        let mut docs: Vec<FsCassDocument> = Vec::new();
        let mut pending_chars = 0usize;
        let mut indexed_docs = 0usize;

        for (conv, conversation_id) in conversations {
            let context = cass_doc_context(conv, conversation_id);
            for msg in &conv.messages {
                let Some(doc) = cass_document_for_message(&context, msg) else {
                    continue;
                };
                push_cass_document_into_pending(&mut docs, &mut pending_chars, doc);
                if docs.len() >= max_messages || pending_chars >= max_chars {
                    indexed_docs = indexed_docs.saturating_add(docs.len());
                    self.inner.add_cass_documents(&docs).map_err(map_fs_err)?;
                    docs.clear();
                    pending_chars = 0;
                }
            }
        }

        if !docs.is_empty() {
            indexed_docs = indexed_docs.saturating_add(docs.len());
            self.inner.add_cass_documents(&docs).map_err(map_fs_err)?;
        }

        Ok(indexed_docs)
    }
}

pub fn build_schema() -> Schema {
    fs_build_schema()
}

pub fn fields_from_schema(schema: &Schema) -> Result<Fields> {
    fs_fields_from_schema(schema).map_err(map_fs_err)
}

pub fn expected_index_dir(base: &Path) -> std::path::PathBuf {
    base.join("index").join(CASS_SCHEMA_VERSION)
}

pub fn index_dir(base: &Path) -> Result<std::path::PathBuf> {
    fs_index_dir(base).map_err(map_fs_err)
}

pub fn ensure_tokenizer(index: &mut Index) {
    fs_ensure_tokenizer(index);
}

fn ensure_empty_merge_output_directory(output_path: &Path) -> Result<()> {
    match fs::metadata(output_path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(anyhow::anyhow!(
                    "merged Tantivy output path is not a directory: {}",
                    output_path.display()
                ));
            }
            let mut entries = fs::read_dir(output_path).with_context(|| {
                format!(
                    "reading merged Tantivy output directory before merge: {}",
                    output_path.display()
                )
            })?;
            if entries.next().transpose()?.is_some() {
                return Err(anyhow::anyhow!(
                    "merged Tantivy output directory must be empty before merge: {}",
                    output_path.display()
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(output_path).with_context(|| {
                format!(
                    "creating merged Tantivy output directory before merge: {}",
                    output_path.display()
                )
            })?;
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "checking merged Tantivy output directory before merge: {}",
                    output_path.display()
                )
            });
        }
    }
    Ok(())
}

fn link_or_copy_searchable_index_file(
    source_path: &Path,
    output_path: &Path,
    relative_path: &Path,
) -> Result<()> {
    let destination_path = output_path.join(relative_path);
    if destination_path.exists() {
        return Err(anyhow::anyhow!(
            "assembled Tantivy output path already exists: {}",
            destination_path.display()
        ));
    }

    match fs::hard_link(source_path, &destination_path) {
        Ok(()) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::CrossesDevices
                    | std::io::ErrorKind::Unsupported
            ) =>
        {
            fs::copy(source_path, &destination_path).with_context(|| {
                format!(
                    "copying Tantivy segment file into assembled generation {} -> {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "hard-linking Tantivy segment file into assembled generation {} -> {}",
                source_path.display(),
                destination_path.display()
            )
        }),
    }
}

fn write_searchable_generation_metadata(
    output_path: &Path,
    index_meta: &tantivy_crate::IndexMeta,
    managed_paths: &mut BTreeSet<std::path::PathBuf>,
) -> Result<()> {
    let meta_path = output_path.join("meta.json");
    fs::write(
        &meta_path,
        serde_json::to_vec_pretty(index_meta).context("serializing assembled Tantivy meta.json")?,
    )
    .with_context(|| {
        format!(
            "writing assembled Tantivy meta.json for {}",
            output_path.display()
        )
    })?;
    managed_paths.insert(std::path::PathBuf::from("meta.json"));
    fs::write(
        output_path.join(".managed.json"),
        serde_json::to_vec(managed_paths).context("serializing assembled Tantivy managed paths")?,
    )
    .with_context(|| {
        format!(
            "writing assembled Tantivy managed file manifest for {}",
            output_path.display()
        )
    })?;
    fs::write(
        output_path.join("schema_hash.json"),
        format!("{{\"schema_hash\":\"{CASS_SCHEMA_HASH}\"}}"),
    )
    .with_context(|| {
        format!(
            "writing cass schema hash metadata for assembled Tantivy index {}",
            output_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{NormalizedConversation, NormalizedMessage};
    use serde_json::Value;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn open_or_create_roundtrip() {
        let dir = TempDir::new().expect("temp dir");
        let idx = TantivyIndex::open_or_create(dir.path()).expect("create index");
        let reader = idx.reader().expect("reader");
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 0);
    }

    #[test]
    fn schema_hash_matches_current_hash() {
        assert!(schema_hash_matches(SCHEMA_HASH));
        assert!(!schema_hash_matches("invalid"));
    }

    #[test]
    fn generate_edge_ngrams_prefixes() {
        let out = frankensearch::lexical::cass_generate_edge_ngrams("hello world");
        assert!(out.contains("he"));
        assert!(out.contains("world"));
    }

    #[test]
    fn build_preview_truncates_with_ellipsis() {
        let preview = frankensearch::lexical::cass_build_preview("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(preview, "abcdefghij…");
    }

    #[test]
    fn merge_status_api_is_exposed() {
        let dir = TempDir::new().expect("temp dir");
        let index = TantivyIndex::open_or_create(dir.path()).expect("create");
        let status = index.merge_status();
        assert_eq!(status.merge_threshold, 4);
    }

    #[test]
    fn merge_status_should_merge_logic() {
        let status = MergeStatus {
            segment_count: 5,
            last_merge_ts: 0,
            ms_since_last_merge: -1,
            merge_threshold: 4,
            cooldown_ms: 300_000,
        };
        assert!(status.should_merge());
    }

    #[test]
    fn index_dir_creates_versioned_path() {
        let dir = TempDir::new().expect("temp dir");
        let result = index_dir(dir.path()).expect("index dir");
        assert!(result.ends_with("index/v7"));
    }

    #[test]
    fn tokenizer_registration_is_callable() {
        let dir = TempDir::new().expect("temp dir");
        let mut idx = Index::create_in_ram(build_schema());
        ensure_tokenizer(&mut idx);
        let _ = TantivyIndex::open_or_create(dir.path()).expect("open or create");
    }

    #[test]
    fn add_messages_batches_large_payloads_without_dropping_docs() {
        let dir = TempDir::new().expect("temp dir");
        let mut idx = TantivyIndex::open_or_create(dir.path()).expect("create index");
        let content = "x".repeat(4096);
        let messages: Vec<_> = (0..1_200)
            .map(|i| NormalizedMessage {
                idx: i,
                role: "assistant".to_string(),
                author: None,
                created_at: Some(1_700_000_000_000 + i),
                content: format!("{i}-{content}"),
                extra: Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
            .collect();
        let conv = NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some("large-batch".to_string()),
            title: Some("Large Batch".to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from("/tmp/rollout.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            metadata: Value::Null,
            messages,
        };

        idx.add_messages(&conv, &conv.messages)
            .expect("add messages");
        idx.commit().expect("commit");

        let reader = idx.reader().expect("reader");
        reader.reload().expect("reload");
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), conv.messages.len() as u64);
    }

    #[test]
    fn add_conversations_with_ids_streams_large_payloads_without_dropping_docs() {
        let dir = TempDir::new().expect("temp dir");
        let mut idx = TantivyIndex::open_or_create(dir.path()).expect("create index");
        let content = "y".repeat(2048);
        let conversations: Vec<_> = (0..24)
            .map(|conv_idx| {
                let messages = (0..256)
                    .map(|msg_idx| NormalizedMessage {
                        idx: msg_idx,
                        role: "assistant".to_string(),
                        author: None,
                        created_at: Some(1_700_000_000_000 + (conv_idx * 1_000 + msg_idx)),
                        content: format!("conv-{conv_idx}-msg-{msg_idx}-{content}"),
                        extra: Value::Null,
                        snippets: Vec::new(),
                        invocations: Vec::new(),
                    })
                    .collect();
                NormalizedConversation {
                    agent_slug: "codex".to_string(),
                    external_id: Some(format!("conv-{conv_idx}")),
                    title: Some(format!("Conversation {conv_idx}")),
                    workspace: Some(PathBuf::from("/tmp/workspace")),
                    source_path: PathBuf::from(format!("/tmp/rollout-{conv_idx}.jsonl")),
                    started_at: Some(1_700_000_000_000 + conv_idx),
                    ended_at: Some(1_700_000_000_999 + conv_idx),
                    metadata: Value::Null,
                    messages,
                }
            })
            .collect();
        let expected_docs: usize = conversations.iter().map(|conv| conv.messages.len()).sum();

        let indexed_docs = idx
            .add_conversations_with_ids(conversations.iter().map(|conv| (conv, Some(42))))
            .expect("add conversations");
        assert_eq!(indexed_docs, expected_docs);
        idx.commit().expect("commit");

        let reader = idx.reader().expect("reader");
        reader.reload().expect("reload");
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), expected_docs as u64);
    }

    #[test]
    fn normalized_index_source_id_infers_remote_from_origin_host_without_kind() {
        let source_id = normalized_index_source_id(Some("   "), None, Some("dev@laptop"));
        assert_eq!(source_id, "dev@laptop");
        assert_eq!(normalized_index_origin_kind(&source_id, None), "remote");
    }

    #[test]
    fn add_prebuilt_documents_streams_large_payloads_without_dropping_docs() {
        let dir = TempDir::new().expect("temp dir");
        let mut idx = TantivyIndex::open_or_create(dir.path()).expect("create index");
        let content = "z".repeat(2048);
        let docs: Vec<_> = (0..6_144)
            .map(|msg_idx| FsCassDocument {
                agent: "codex".to_string(),
                workspace: Some("/tmp/workspace".to_string()),
                workspace_original: None,
                source_path: "/tmp/prebuilt-rollout.jsonl".to_string(),
                msg_idx: msg_idx as u64,
                created_at: Some(1_700_000_000_000 + msg_idx as i64),
                title: Some("Prebuilt Batch".to_string()),
                content: format!("prebuilt-msg-{msg_idx}-{content}"),
                conversation_id: Some(7),
                source_id: crate::sources::provenance::LOCAL_SOURCE_ID.to_string(),
                origin_kind: crate::sources::provenance::LOCAL_SOURCE_ID.to_string(),
                origin_host: None,
            })
            .collect();
        let expected_docs = docs.len();

        let indexed_docs = idx.add_prebuilt_documents(docs).expect("add prebuilt docs");
        assert_eq!(indexed_docs, expected_docs);
        idx.commit().expect("commit");

        let reader = idx.reader().expect("reader");
        reader.reload().expect("reload");
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), expected_docs as u64);
    }

    #[test]
    fn merge_compatible_index_directories_roundtrips_docs_into_single_segment() {
        let root = TempDir::new().expect("temp dir");
        let shard_a = root.path().join("shard-a");
        let shard_b = root.path().join("shard-b");
        let merged = root.path().join("merged");

        let mut shard_a_index = TantivyIndex::open_or_create(&shard_a).expect("create shard a");
        let mut shard_b_index = TantivyIndex::open_or_create(&shard_b).expect("create shard b");

        let make_conv = |external_id: &str, title: &str, content: &str| NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some(external_id.to_string()),
            title: Some(title.to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            metadata: Value::Null,
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: None,
                    created_at: Some(1_700_000_000_010),
                    content: format!("{content}-a"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".to_string(),
                    author: None,
                    created_at: Some(1_700_000_000_020),
                    content: format!("{content}-b"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
            ],
        };

        let conv_a = make_conv("merge-a", "Merge A", "alpha");
        let conv_b = make_conv("merge-b", "Merge B", "beta");
        shard_a_index
            .add_conversation_with_id(&conv_a, Some(10))
            .expect("index shard a");
        shard_b_index
            .add_conversation_with_id(&conv_b, Some(20))
            .expect("index shard b");
        shard_a_index.commit().expect("commit shard a");
        shard_b_index.commit().expect("commit shard b");
        drop(shard_a_index);
        drop(shard_b_index);

        let merged_index =
            TantivyIndex::merge_compatible_index_directories(&merged, &[&shard_a, &shard_b])
                .expect("merge shard indices");
        assert_eq!(
            merged_index.segment_count(),
            1,
            "merged shard indices should collapse into a single searchable segment"
        );
        let reader = merged_index.reader().expect("reader");
        reader.reload().expect("reload");
        assert_eq!(reader.searcher().num_docs(), 4);
    }

    #[test]
    fn merge_compatible_index_directories_rejects_non_empty_output_directory() {
        let root = TempDir::new().expect("temp dir");
        let shard = root.path().join("shard");
        let merged = root.path().join("merged");
        fs::create_dir_all(&merged).expect("create merged dir");
        fs::write(merged.join("sentinel.txt"), "occupied").expect("write sentinel");

        let mut shard_index = TantivyIndex::open_or_create(&shard).expect("create shard");
        let conv = NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some("merge-occupied".to_string()),
            title: Some("Occupied".to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from("/tmp/merge-occupied.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            metadata: Value::Null,
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "assistant".to_string(),
                author: None,
                created_at: Some(1_700_000_000_010),
                content: "occupied".to_string(),
                extra: Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            }],
        };
        shard_index
            .add_conversation_with_id(&conv, Some(1))
            .expect("index shard");
        shard_index.commit().expect("commit shard");
        drop(shard_index);

        let error = match TantivyIndex::merge_compatible_index_directories(&merged, &[&shard]) {
            Ok(_) => panic!("non-empty merge output dir should be rejected"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("must be empty"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn assemble_compatible_index_directories_roundtrips_docs_into_multi_segment_generation() {
        let root = TempDir::new().expect("temp dir");
        let shard_a = root.path().join("shard-a");
        let shard_b = root.path().join("shard-b");
        let assembled = root.path().join("assembled");

        let mut shard_a_index = TantivyIndex::open_or_create(&shard_a).expect("create shard a");
        let mut shard_b_index = TantivyIndex::open_or_create(&shard_b).expect("create shard b");

        let make_conv = |external_id: &str, title: &str, content: &str| NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some(external_id.to_string()),
            title: Some(title.to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
            started_at: Some(1_700_000_001_000),
            ended_at: Some(1_700_000_001_100),
            metadata: Value::Null,
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: None,
                    created_at: Some(1_700_000_001_010),
                    content: format!("{content}-a"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".to_string(),
                    author: None,
                    created_at: Some(1_700_000_001_020),
                    content: format!("{content}-b"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
            ],
        };

        let conv_a = make_conv("assemble-a", "Assemble A", "alpha");
        let conv_b = make_conv("assemble-b", "Assemble B", "beta");
        shard_a_index
            .add_conversation_with_id(&conv_a, Some(10))
            .expect("index shard a");
        shard_b_index
            .add_conversation_with_id(&conv_b, Some(20))
            .expect("index shard b");
        shard_a_index.commit().expect("commit shard a");
        shard_b_index.commit().expect("commit shard b");
        drop(shard_a_index);
        drop(shard_b_index);

        let assembled_index =
            TantivyIndex::assemble_compatible_index_directories(&assembled, &[&shard_a, &shard_b])
                .expect("assemble shard indices");
        let reader = assembled_index.reader().expect("reader");
        reader.reload().expect("reload");
        assert_eq!(reader.searcher().num_docs(), 4);
        assert_eq!(
            assembled_index.segment_count(),
            2,
            "assembled shard indices should preserve one searchable segment per input artifact"
        );
        assert!(
            assembled.join(".managed.json").exists(),
            "assembled index generation should persist a Tantivy managed-file manifest"
        );
    }

    #[test]
    fn publish_federated_searchable_index_directories_writes_manifest_without_root_meta() {
        let root = TempDir::new().expect("temp dir");
        let shard_a = root.path().join("shard-a");
        let shard_b = root.path().join("shard-b");
        let published = root.path().join("published");

        let mut shard_a_index = TantivyIndex::open_or_create(&shard_a).expect("create shard a");
        let mut shard_b_index = TantivyIndex::open_or_create(&shard_b).expect("create shard b");

        let make_conv = |external_id: &str, title: &str, content: &str| NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some(external_id.to_string()),
            title: Some(title.to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
            started_at: Some(1_700_000_002_000),
            ended_at: Some(1_700_000_002_100),
            metadata: Value::Null,
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: None,
                    created_at: Some(1_700_000_002_010),
                    content: format!("{content}-a"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".to_string(),
                    author: None,
                    created_at: Some(1_700_000_002_020),
                    content: format!("{content}-b"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
            ],
        };

        shard_a_index
            .add_conversation_with_id(&make_conv("fed-a", "Fed A", "alpha"), Some(10))
            .expect("index shard a");
        shard_b_index
            .add_conversation_with_id(&make_conv("fed-b", "Fed B", "beta"), Some(20))
            .expect("index shard b");
        shard_a_index.commit().expect("commit shard a");
        shard_b_index.commit().expect("commit shard b");
        drop(shard_a_index);
        drop(shard_b_index);

        let summary =
            publish_federated_searchable_index_directories(&published, &[&shard_a, &shard_b])
                .expect("publish federated bundle");
        assert_eq!(summary.docs, 4);
        assert_eq!(summary.segments, 2);
        assert!(
            !published.join("meta.json").exists(),
            "federated publish root should not force a standard single-index meta.json"
        );
        assert!(
            published.join(FEDERATED_SEARCH_MANIFEST_FILE).exists(),
            "federated publish root should persist its manifest"
        );
        let manifest = load_federated_search_manifest_internal(&published)
            .expect("load manifest")
            .expect("manifest present");
        assert_eq!(manifest.shards.len(), 2);
        assert_eq!(
            searchable_index_summary(&published)
                .expect("summary")
                .expect("searchable summary")
                .docs,
            4
        );
    }

    #[test]
    fn open_or_create_materializes_federated_bundle_back_into_mutable_index() {
        let root = TempDir::new().expect("temp dir");
        let shard_a = root.path().join("shard-a");
        let shard_b = root.path().join("shard-b");
        let published = root.path().join("published");

        let mut shard_a_index = TantivyIndex::open_or_create(&shard_a).expect("create shard a");
        let mut shard_b_index = TantivyIndex::open_or_create(&shard_b).expect("create shard b");

        let make_conv = |external_id: &str, title: &str, content: &str| NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some(external_id.to_string()),
            title: Some(title.to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
            started_at: Some(1_700_000_003_000),
            ended_at: Some(1_700_000_003_100),
            metadata: Value::Null,
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: None,
                    created_at: Some(1_700_000_003_010),
                    content: format!("{content}-a"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".to_string(),
                    author: None,
                    created_at: Some(1_700_000_003_020),
                    content: format!("{content}-b"),
                    extra: Value::Null,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
            ],
        };

        shard_a_index
            .add_conversation_with_id(&make_conv("mat-a", "Mat A", "alpha"), Some(10))
            .expect("index shard a");
        shard_b_index
            .add_conversation_with_id(&make_conv("mat-b", "Mat B", "beta"), Some(20))
            .expect("index shard b");
        shard_a_index.commit().expect("commit shard a");
        shard_b_index.commit().expect("commit shard b");
        drop(shard_a_index);
        drop(shard_b_index);

        publish_federated_searchable_index_directories(&published, &[&shard_a, &shard_b])
            .expect("publish federated bundle");
        assert!(
            published.join(FEDERATED_SEARCH_MANIFEST_FILE).exists(),
            "test fixture should start in federated bundle form"
        );

        let mutable_index =
            TantivyIndex::open_or_create(&published).expect("materialize mutable index");
        let reader = mutable_index.reader().expect("reader");
        reader.reload().expect("reload");
        assert_eq!(reader.searcher().num_docs(), 4);
        assert!(
            published.join("meta.json").exists(),
            "writer open should materialize a standard writable Tantivy index"
        );
        assert!(
            !published.join(FEDERATED_SEARCH_MANIFEST_FILE).exists(),
            "materialization should replace the federated bundle manifest"
        );
    }
}
