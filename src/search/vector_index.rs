//! Vector index facade for cass.
//!
//! cass uses the frankensearch FSVI vector index format and search primitives
//! (via the `frankensearch` crate). The older CVVI format has been retired.
//!
//! This module keeps cass-specific helpers (paths, role codes) in one place.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use crate::franken_sync::Connection as FrankenConnection;
use crate::franken_sync::compat::{ConnectionExt, RowExt};
use half::f16;

pub use frankensearch::index::{Quantization, SearchParams, VectorIndex, VectorIndexWriter};

use crate::search::query::SearchFilters;
use crate::sources::provenance::{LOCAL_SOURCE_ID, SourceFilter, SourceKind};
use crate::storage::sqlite::FrankenStorage;

/// Directory under the cass data dir where vector artifacts are stored.
pub const VECTOR_INDEX_DIR: &str = "vector_index";

// Message role codes stored in doc_id metadata and used for filtering.
pub const ROLE_USER: u8 = 0;
pub const ROLE_ASSISTANT: u8 = 1;
pub const ROLE_SYSTEM: u8 = 2;
pub const ROLE_TOOL: u8 = 3;

/// Map a role string (from SQLite / connectors) to a compact u8 code.
#[must_use]
pub fn role_code_from_str(role: &str) -> Option<u8> {
    match role {
        "user" => Some(ROLE_USER),
        // cass historically used both "agent" and "assistant" for model responses.
        "assistant" | "agent" => Some(ROLE_ASSISTANT),
        "system" => Some(ROLE_SYSTEM),
        "tool" => Some(ROLE_TOOL),
        _ => None,
    }
}

/// Parse a list of role strings into a set of role codes.
///
/// # Errors
///
/// Returns an error if any role string is unknown.
pub fn parse_role_codes<I, S>(roles: I) -> Result<HashSet<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = HashSet::new();
    for role in roles {
        let role_str = role.as_ref();
        let code =
            role_code_from_str(role_str).ok_or_else(|| anyhow!("unknown role: {role_str}"))?;
        out.insert(code);
    }
    Ok(out)
}

/// Path to the primary FSVI vector index for a given embedder.
#[must_use]
pub fn vector_index_path(data_dir: &Path, embedder_id: &str) -> PathBuf {
    data_dir
        .join(VECTOR_INDEX_DIR)
        .join(format!("index-{embedder_id}.fsvi"))
}

/// Stable, bounded reasons why an exact semantic artifact set cannot provide
/// progressive/two-tier serving.
///
/// These codes are shared by query-time fallback and the human/JSON readiness
/// surfaces so operators never have to infer topology failures from paths or
/// free-form error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticProgressiveUnavailableReason {
    /// Exact serving retains an opened FSVI owner, but the requested
    /// progressive/two-tier constructor cannot consume that owner without
    /// reopening a pathname and risking post-validation replacement.
    OwnerBackedReaderRequired,
    /// Exact search owns multiple shards and no ordered multi-shard two-tier
    /// implementation is available.
    MultipleExactShards,
    /// A quality artifact exists without a selected fast artifact to provide
    /// the initial phase.
    FastArtifactMissing,
    /// The selected fast artifact has no explicitly paired quality artifact.
    QualityArtifactMissing,
    /// Fast and quality artifacts are visible in metadata, but the serving
    /// setup has not retained them as one constructor-owned pair.
    ExactTierPairingRequired,
    /// The selected fast and quality roles resolve to the same filesystem
    /// object.
    ExactArtifactRoleAlias,
    /// One of the explicitly paired FSVI artifacts could not be opened by the
    /// two-tier reader.
    ExactArtifactOpenFailed,
}

impl SemanticProgressiveUnavailableReason {
    /// Stable diagnostic code used by logs and JSON/human status surfaces.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::OwnerBackedReaderRequired => "owner_backed_reader_required",
            Self::MultipleExactShards => "multiple_exact_shards",
            Self::FastArtifactMissing => "fast_artifact_missing",
            Self::QualityArtifactMissing => "quality_artifact_missing",
            Self::ExactTierPairingRequired => "exact_tier_pairing_required",
            Self::ExactArtifactRoleAlias => "exact_artifact_role_alias",
            Self::ExactArtifactOpenFailed => "exact_artifact_open_failed",
        }
    }
}

/// Stable, bounded reasons why a semantic artifact cannot serve its selected
/// HNSW sidecar.
///
/// Approximate search is an optimization over the exact FSVI source of truth.
/// Callers use this reason to report a truthful exact-search fallback without
/// exposing paths, parser errors, or other unbounded artifact details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticAnnUnavailableReason {
    /// More than one exact shard is active, but no sharded ANN topology was
    /// explicitly selected.
    MultipleExactShards,
    /// The selected exact artifact has no explicitly paired ANN sidecar.
    SidecarMissing,
    /// The ANN metadata or native sidecars could not be opened or validated.
    SidecarOpenFailed,
    /// The selected persisted ANN metadata was readable, but its graph was
    /// legacy, stale, incomplete, corrupt, or otherwise could not be admitted
    /// as the exact native graph.
    SidecarNotNative,
}

impl SemanticAnnUnavailableReason {
    /// Stable diagnostic code used by logs and status/search metadata.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MultipleExactShards => "multiple_exact_shards",
            Self::SidecarMissing => "ann_sidecar_missing",
            Self::SidecarOpenFailed => "ann_sidecar_open_failed",
            Self::SidecarNotNative => "ann_sidecar_not_native",
        }
    }
}

/// One opened semantic index inseparably paired with the exact path that
/// selected it and its optional CASS-owned ANN sidecar.
///
/// The constructor opens the FSVI itself and all fields are private, so a
/// reader from one file cannot be relabelled with another file's path. Relative
/// inputs are frozen against one current-directory snapshot before the open.
#[derive(Debug, Clone)]
pub struct SemanticIndexArtifact {
    index: Arc<VectorIndex>,
    fsvi_path: PathBuf,
    ann_path: Option<PathBuf>,
    ann_unavailable_reason: Option<SemanticAnnUnavailableReason>,
    owner_backed_progressive_reader: bool,
}

impl SemanticIndexArtifact {
    /// Open an FSVI artifact and retain its exact serving paths.
    ///
    /// # Errors
    ///
    /// Returns an error when the current directory cannot be captured for a
    /// relative input or when the selected FSVI cannot be opened. An invalid
    /// optional ANN sidecar never prevents the exact FSVI from opening; its
    /// bounded rejection reason is retained for exact-search fallback.
    pub fn open(fsvi_path: impl Into<PathBuf>, ann_path: Option<PathBuf>) -> Result<Self> {
        let current_dir =
            std::env::current_dir().context("capture semantic artifact current directory")?;
        let freeze = |path: PathBuf| {
            if path.is_absolute() {
                path
            } else {
                current_dir.join(path)
            }
        };
        let fsvi_path = freeze(fsvi_path.into());
        let ann_path = ann_path.map(freeze);
        reject_final_component_symlink("FSVI", &fsvi_path)?;
        let index = VectorIndex::open_read_only(&fsvi_path)
            .with_context(|| format!("open semantic FSVI {}", fsvi_path.display()))?;
        let ann_unavailable_reason =
            ann_path
                .as_deref()
                .and_then(|path| match std::fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Some(SemanticAnnUnavailableReason::SidecarMissing)
                    }
                    Err(_) => Some(SemanticAnnUnavailableReason::SidecarOpenFailed),
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        Some(SemanticAnnUnavailableReason::SidecarOpenFailed)
                    }
                    Ok(metadata) if !metadata.file_type().is_file() => {
                        Some(SemanticAnnUnavailableReason::SidecarOpenFailed)
                    }
                    Ok(_) => match same_file::is_same_file(&fsvi_path, path) {
                        Ok(true) | Err(_) => Some(SemanticAnnUnavailableReason::SidecarOpenFailed),
                        Ok(false) => None,
                    },
                });
        Ok(Self {
            index: Arc::new(index),
            fsvi_path,
            ann_path,
            ann_unavailable_reason,
            // `VectorIndex::open_read_only` retains the exact FSVI owner used
            // by CASS exact search, but FrankenSearch's current two-tier
            // constructors still reopen a pathname. Keep those lanes
            // fail-closed until an owner-accepting constructor can set this
            // capability true.
            owner_backed_progressive_reader: false,
        })
    }

    /// Opened FSVI reader.
    #[must_use]
    pub fn index(&self) -> &VectorIndex {
        self.index.as_ref()
    }

    /// Clone the opened reader owner for a search operation.
    #[must_use]
    pub(crate) fn index_owner(&self) -> Arc<VectorIndex> {
        Arc::clone(&self.index)
    }

    /// Exact FSVI path used by [`Self::open`].
    #[must_use]
    pub fn fsvi_path(&self) -> &Path {
        &self.fsvi_path
    }

    /// Optional CASS-owned HNSW sidecar paired with this FSVI.
    #[must_use]
    pub fn ann_path(&self) -> Option<&Path> {
        self.ann_path.as_deref()
    }

    /// Bounded reason the explicitly paired ANN sidecar was rejected before
    /// query-time native admission.
    #[must_use]
    pub fn ann_unavailable_reason(&self) -> Option<SemanticAnnUnavailableReason> {
        self.ann_unavailable_reason
    }

    /// Whether progressive/two-tier constructors can consume this retained
    /// owner without reopening [`Self::fsvi_path`].
    ///
    /// Path-opened artifacts deliberately return false. A future
    /// owner-accepting FrankenSearch API must be wired through a distinct
    /// constructor before this capability can become true.
    #[must_use]
    pub fn has_owner_backed_progressive_reader(&self) -> bool {
        self.owner_backed_progressive_reader
    }
}

fn reject_final_component_symlink(role: &str, path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "semantic {role} artifact must not be a final-component symlink: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect semantic {role} path {}", path.display()))
        }
    }
}

/// Semantic doc_id fields encoded into FSVI records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticDocId {
    pub message_id: u64,
    pub chunk_idx: u8,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub created_at_ms: i64,
    pub content_hash: Option<[u8; 32]>,
}

impl SemanticDocId {
    /// Encode this semantic vector record doc_id into the string form stored in FSVI.
    ///
    /// Hot-path encoder: runs once per embedded message during indexing and
    /// for every search hit that goes through semantic lookup. Build the
    /// output in a single pre-sized `String` with `itoa::Buffer` for the
    /// integer fields instead of `format!`, which walks the formatter-trait
    /// machinery per arg and grows its internal buffer on demand.
    #[must_use]
    pub fn to_doc_id_string(&self) -> String {
        // Capacity estimate: "m|" (2) + seven integer fields up to 20 chars
        // + six '|' separators + optional 64-hex hash + one '|' if present.
        // Slight over-allocation is fine and avoids any realloc.
        let capacity = 2 + (7 * 20) + 6 + if self.content_hash.is_some() { 65 } else { 0 };
        let mut out = String::with_capacity(capacity);
        let mut buf = itoa::Buffer::new();
        out.push_str("m|");
        out.push_str(buf.format(self.message_id));
        out.push('|');
        out.push_str(buf.format(self.chunk_idx));
        out.push('|');
        out.push_str(buf.format(self.agent_id));
        out.push('|');
        out.push_str(buf.format(self.workspace_id));
        out.push('|');
        out.push_str(buf.format(self.source_id));
        out.push('|');
        out.push_str(buf.format(self.role));
        out.push('|');
        out.push_str(buf.format(self.created_at_ms));
        if let Some(hash) = self.content_hash {
            out.push('|');
            // Stack-buffered hex encode: avoids the 64-byte heap alloc that
            // `hex::encode(hash)` performs internally. Hex output is pure
            // ASCII so str::from_utf8 can't fail on the filled slice.
            let mut hex_buf = [0u8; 64];
            hex::encode_to_slice(hash, &mut hex_buf)
                .expect("32 bytes encode to exactly 64 hex chars");
            out.push_str(std::str::from_utf8(&hex_buf).expect("hex output is always valid ASCII"));
        }
        out
    }
}

/// Parse a cass semantic doc_id string.
///
/// Accepts doc_ids with trailing segments (future expansion) and an optional
/// 64-hex content hash suffix.
#[must_use]
pub fn parse_semantic_doc_id(doc_id: &str) -> Option<SemanticDocId> {
    // Fast reject: every cass semantic doc_id starts with "m|". `strip_prefix`
    // avoids the full iterator setup + first `.next()` comparison when the
    // discriminator doesn't match. `splitn(8, '|')` caps the field scan at
    // exactly the 7 required fields + a single tail holding the optional
    // content hash (which itself never contains '|').
    let rest = doc_id.strip_prefix("m|")?;
    let mut parts = rest.splitn(8, '|');
    let parsed = SemanticDocId {
        message_id: parts.next()?.parse().ok()?,
        chunk_idx: parts.next()?.parse().ok()?,
        agent_id: parts.next()?.parse().ok()?,
        workspace_id: parts.next()?.parse().ok()?,
        source_id: parts.next()?.parse().ok()?,
        role: parts.next()?.parse().ok()?,
        created_at_ms: parts.next()?.parse().ok()?,
        content_hash: parts.next().and_then(|hash_hex| {
            if hash_hex.len() != 64 {
                return None;
            }
            let mut hash = [0u8; 32];
            hex::decode_to_slice(hash_hex, &mut hash).ok()?;
            Some(hash)
        }),
    };

    Some(parsed)
}

/// Lean filter-only view of a parsed semantic doc_id.
///
/// Drops the content_hash (which requires hex::decode_to_slice on 64 bytes)
/// plus the unused message_id and chunk_idx. Used by
/// `SemanticFilter::matches`, which runs once per HNSW-visited node during
/// ANN traversal — often thousands of times per query — and never reads the
/// content_hash or message identifiers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SemanticDocIdFilterView {
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub created_at_ms: i64,
}

/// Parse only the filter-relevant fields of a cass semantic doc_id string.
///
/// ~5x cheaper than `parse_semantic_doc_id` when the content_hash is present,
/// because it skips the 64-byte hex decode that dominates the full-parse cost.
#[must_use]
pub(crate) fn parse_semantic_doc_id_filter_view(doc_id: &str) -> Option<SemanticDocIdFilterView> {
    let rest = doc_id.strip_prefix("m|")?;
    let mut parts = rest.splitn(8, '|');
    // message_id + chunk_idx: we only need to advance the iterator past them.
    parts.next()?;
    parts.next()?;
    let agent_id: u32 = parts.next()?.parse().ok()?;
    let workspace_id: u32 = parts.next()?.parse().ok()?;
    let source_id: u32 = parts.next()?.parse().ok()?;
    let role: u8 = parts.next()?.parse().ok()?;
    let created_at_ms: i64 = parts.next()?.parse().ok()?;
    Some(SemanticDocIdFilterView {
        agent_id,
        workspace_id,
        source_id,
        role,
        created_at_ms,
    })
}

fn map_filter_set(keys: &HashSet<String>, map: &HashMap<String, u32>) -> Option<HashSet<u32>> {
    if keys.is_empty() {
        return None;
    }
    let mut set = HashSet::new();
    for key in keys {
        if let Some(id) = map.get(key) {
            set.insert(*id);
        }
    }
    Some(set)
}

fn source_id_hash(source_id: &str) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(source_id.as_bytes());
    hasher.finalize()
}

/// Semantic filter constraints expressed in numeric IDs for fast evaluation.
#[derive(Debug, Clone, Default)]
pub struct SemanticFilter {
    pub agents: Option<HashSet<u32>>,
    pub workspaces: Option<HashSet<u32>>,
    pub sources: Option<HashSet<u32>>,
    pub roles: Option<HashSet<u8>>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
}

impl SemanticFilter {
    pub fn from_search_filters(filters: &SearchFilters, maps: &SemanticFilterMaps) -> Result<Self> {
        let agents = map_filter_set(&filters.agents, &maps.agent_slug_to_id);
        let workspaces = map_filter_set(&filters.workspaces, &maps.workspace_path_to_id);
        let sources = maps.sources_from_filter(&filters.source_filter)?;

        Ok(Self {
            agents,
            workspaces,
            sources,
            roles: None,
            created_from: filters.created_from,
            created_to: filters.created_to,
        })
    }

    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.agents.is_none()
            && self.workspaces.is_none()
            && self.sources.is_none()
            && self.roles.is_none()
            && self.created_from.is_none()
            && self.created_to.is_none()
    }

    #[must_use]
    pub fn with_roles(mut self, roles: Option<HashSet<u8>>) -> Self {
        self.roles = roles;
        self
    }
}

/// Lookup maps for converting human filters (agent slug, workspace path, source id)
/// into compact numeric IDs embedded into semantic doc_id strings.
#[derive(Debug, Clone)]
pub struct SemanticFilterMaps {
    agent_slug_to_id: HashMap<String, u32>,
    workspace_path_to_id: HashMap<String, u32>,
    source_id_to_id: HashMap<String, u32>,
    remote_source_ids: HashSet<u32>,
}

impl SemanticFilterMaps {
    pub fn from_storage(storage: &FrankenStorage) -> Result<Self> {
        Self::from_connection(storage.raw())
    }

    pub fn from_connection(conn: &FrankenConnection) -> Result<Self> {
        let mut agent_slug_to_id = HashMap::new();
        let agent_rows = conn.query_map_collect(
            "SELECT id, slug FROM agents",
            &[],
            |row: &crate::franken_sync::Row| {
                let id: i64 = row.get_typed(0)?;
                let slug: String = row.get_typed(1)?;
                Ok((id, slug))
            },
        )?;
        for (id, slug) in agent_rows {
            let id_u32 = u32::try_from(id).map_err(|_| anyhow!("agent id out of range"))?;
            agent_slug_to_id.insert(slug, id_u32);
        }

        let mut workspace_path_to_id = HashMap::new();
        let workspace_rows = conn.query_map_collect(
            "SELECT id, path FROM workspaces",
            &[],
            |row: &crate::franken_sync::Row| {
                let id: i64 = row.get_typed(0)?;
                let path: String = row.get_typed(1)?;
                Ok((id, path))
            },
        )?;
        for (id, path) in workspace_rows {
            let id_u32 = u32::try_from(id).map_err(|_| anyhow!("workspace id out of range"))?;
            workspace_path_to_id.insert(path, id_u32);
        }

        let mut source_id_to_id = HashMap::new();
        let mut remote_source_ids = HashSet::new();
        let source_rows = conn.query_map_collect(
            "SELECT id, kind FROM sources",
            &[],
            |row: &crate::franken_sync::Row| {
                let id: String = row.get_typed(0)?;
                let kind: String = row.get_typed(1)?;
                Ok((id, kind))
            },
        )?;
        for (id, kind) in source_rows {
            let id_u32 = source_id_hash(&id);
            if SourceKind::parse(&kind).is_none_or(|k| k.is_remote()) {
                remote_source_ids.insert(id_u32);
            }
            source_id_to_id.insert(id, id_u32);
        }

        Ok(Self {
            agent_slug_to_id,
            workspace_path_to_id,
            source_id_to_id,
            remote_source_ids,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(
        agent_slug_to_id: HashMap<String, u32>,
        workspace_path_to_id: HashMap<String, u32>,
        source_id_to_id: HashMap<String, u32>,
        remote_source_ids: HashSet<u32>,
    ) -> Self {
        Self {
            agent_slug_to_id,
            workspace_path_to_id,
            source_id_to_id,
            remote_source_ids,
        }
    }

    fn sources_from_filter(&self, filter: &SourceFilter) -> Result<Option<HashSet<u32>>> {
        let result = match filter {
            SourceFilter::All => None,
            SourceFilter::Local => Some(HashSet::from([self.source_id(LOCAL_SOURCE_ID)])),
            SourceFilter::Remote => Some(self.remote_source_ids.clone()),
            SourceFilter::SourceId(id) => Some(HashSet::from([self.source_id(id)])),
        };
        Ok(result)
    }

    fn source_id(&self, source_id: &str) -> u32 {
        self.source_id_to_id
            .get(source_id)
            .copied()
            .unwrap_or_else(|| source_id_hash(source_id))
    }
}

/// Collapsed semantic search hit (best chunk per message).
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub message_id: u64,
    pub chunk_idx: u8,
    pub score: f32,
}

impl frankensearch::core::filter::SearchFilter for SemanticFilter {
    fn matches(&self, doc_id: &str, _metadata: Option<&serde_json::Value>) -> bool {
        // Use the filter-view parse: skips the expensive 64-byte hex decode
        // of content_hash that the full parse runs on every call.
        let Some(parsed) = parse_semantic_doc_id_filter_view(doc_id) else {
            return false;
        };

        if let Some(agents) = &self.agents
            && !agents.contains(&parsed.agent_id)
        {
            return false;
        }
        if let Some(workspaces) = &self.workspaces
            && !workspaces.contains(&parsed.workspace_id)
        {
            return false;
        }
        if let Some(sources) = &self.sources
            && !sources.contains(&parsed.source_id)
        {
            return false;
        }
        if let Some(roles) = &self.roles
            && !roles.contains(&parsed.role)
        {
            return false;
        }
        if let Some(from) = self.created_from
            && parsed.created_at_ms < from
        {
            return false;
        }
        if let Some(to) = self.created_to
            && parsed.created_at_ms > to
        {
            return false;
        }

        true
    }

    fn matches_doc_id_hash(
        &self,
        _doc_id_hash: u64,
        _metadata: Option<&serde_json::Value>,
    ) -> Option<bool> {
        None
    }

    fn name(&self) -> &str {
        "cass_semantic_filter"
    }
}

/// Scalar dot product benchmark helper.
#[must_use]
pub fn dot_product_scalar_bench(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// SIMD dot product benchmark helper (uses frankensearch's portable SIMD).
#[must_use]
pub fn dot_product_simd_bench(a: &[f32], b: &[f32]) -> f32 {
    frankensearch::index::dot_product_f32_f32(a, b).expect("dot product inputs must match length")
}

/// Scalar dot product benchmark helper for f16 stored vectors vs f32 query.
#[must_use]
pub fn dot_product_f16_scalar_bench(stored: &[f16], query: &[f32]) -> f32 {
    stored.iter().zip(query).map(|(x, y)| x.to_f32() * y).sum()
}

/// SIMD dot product benchmark helper for f16 stored vectors vs f32 query.
#[must_use]
pub fn dot_product_f16_simd_bench(stored: &[f16], query: &[f32]) -> f32 {
    frankensearch::index::dot_product_f16_f32(stored, query)
        .expect("dot product inputs must match length")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_code_from_str_accepts_known_roles() {
        let cases = [
            ("user", Some(ROLE_USER)),
            ("assistant", Some(ROLE_ASSISTANT)),
            ("agent", Some(ROLE_ASSISTANT)),
            ("system", Some(ROLE_SYSTEM)),
            ("tool", Some(ROLE_TOOL)),
            ("unknown", None),
        ];

        for (role, expected_code) in cases {
            assert_eq!(role_code_from_str(role), expected_code, "{role}");
        }
    }

    #[test]
    fn parse_role_codes_rejects_unknown_roles() {
        let err = parse_role_codes(["user", "bogus"]).unwrap_err();
        assert!(err.to_string().contains("unknown role"));
    }

    #[test]
    fn vector_index_path_points_to_fsvi() {
        let dir = Path::new("/tmp/cass");
        let p = vector_index_path(dir, "fnv1a-384");
        assert!(p.ends_with("vector_index/index-fnv1a-384.fsvi"));
    }

    #[test]
    fn exact_artifact_contract_retains_selected_paths_without_mutation() {
        let dir = tempfile::tempdir().expect("artifact fixture");
        let fsvi_path = dir.path().join("nonconventional-fast-name.fsvi");
        let ann_path = dir.path().join("consumer-owned-ann.chsw");
        let writer = VectorIndex::create_with_revision(
            &fsvi_path,
            "fnv1a-2",
            "artifact-rev",
            2,
            Quantization::F16,
        )
        .expect("create exact FSVI");
        writer.finish().expect("finish exact FSVI");
        std::fs::write(&ann_path, b"consumer-owned ANN fixture").expect("write ANN fixture");
        let metadata_before = std::fs::metadata(&fsvi_path).expect("metadata before open");
        let bytes_before = std::fs::read(&fsvi_path).expect("bytes before open");
        let mut entries_before = std::fs::read_dir(dir.path())
            .expect("entries before open")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        entries_before.sort();

        let artifact =
            SemanticIndexArtifact::open(&fsvi_path, Some(ann_path.clone())).expect("open artifact");

        assert_eq!(artifact.fsvi_path(), fsvi_path);
        assert_eq!(artifact.ann_path(), Some(ann_path.as_path()));
        assert_eq!(artifact.index().embedder_id(), "fnv1a-2");
        assert_eq!(artifact.index().dimension(), 2);
        assert!(
            !artifact.has_owner_backed_progressive_reader(),
            "a path-opened artifact must not authorize a later pathname reopen"
        );
        let metadata_after = std::fs::metadata(&fsvi_path).expect("metadata after open");
        assert_eq!(metadata_after.len(), metadata_before.len());
        assert_eq!(
            std::fs::read(&fsvi_path).expect("bytes after open"),
            bytes_before,
            "opening a serving artifact must not rewrite the FSVI bytes"
        );
        assert_eq!(
            metadata_after.modified().expect("mtime after"),
            metadata_before.modified().expect("mtime before"),
            "opening a serving artifact must not rewrite the FSVI"
        );
        let mut entries_after = std::fs::read_dir(dir.path())
            .expect("entries after open")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        entries_after.sort();
        assert_eq!(
            entries_after, entries_before,
            "opening an artifact must not create conventional aliases or ANN sidecars"
        );
    }

    #[test]
    fn exact_artifact_contract_rejects_missing_and_corrupt_paths() {
        let dir = tempfile::tempdir().expect("artifact fixture");
        let missing = dir.path().join("missing-selected.fsvi");
        let missing_error =
            SemanticIndexArtifact::open(&missing, None).expect_err("missing path must fail");
        assert!(missing_error.to_string().contains("missing-selected.fsvi"));

        let corrupt = dir.path().join("corrupt-selected.fsvi");
        std::fs::write(&corrupt, b"not an fsvi").expect("write corrupt FSVI");
        let corrupt_error =
            SemanticIndexArtifact::open(&corrupt, None).expect_err("corrupt path must fail");
        assert!(corrupt_error.to_string().contains("corrupt-selected.fsvi"));

        let valid = dir.path().join("valid-selected.fsvi");
        VectorIndex::create_with_revision(&valid, "fnv1a-2", "artifact-rev", 2, Quantization::F16)
            .expect("create valid FSVI")
            .finish()
            .expect("finish valid FSVI");
        let missing_ann = dir.path().join("missing-selected.chsw");
        let artifact = SemanticIndexArtifact::open(&valid, Some(missing_ann.clone()))
            .expect("a missing optional ANN must preserve exact FSVI serving");
        assert_eq!(artifact.ann_path(), Some(missing_ann.as_path()));
        assert_eq!(
            artifact.ann_unavailable_reason(),
            Some(SemanticAnnUnavailableReason::SidecarMissing)
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_artifact_contract_rejects_fsvi_symlink_and_degrades_invalid_ann_aliases() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("artifact alias fixture");
        let fsvi_path = dir.path().join("selected.fsvi");
        VectorIndex::create_with_revision(
            &fsvi_path,
            "fnv1a-2",
            "artifact-rev",
            2,
            Quantization::F16,
        )
        .expect("create selected FSVI")
        .finish()
        .expect("finish selected FSVI");

        let fsvi_symlink = dir.path().join("selected-symlink.fsvi");
        symlink(&fsvi_path, &fsvi_symlink).expect("create FSVI symlink");
        let fsvi_symlink_error = SemanticIndexArtifact::open(&fsvi_symlink, None)
            .expect_err("final-component FSVI symlink must fail closed");
        assert!(fsvi_symlink_error.to_string().contains("symlink"));

        let ann_target = dir.path().join("selected-ann-target.chsw");
        std::fs::write(&ann_target, b"ANN fixture").expect("write ANN target");
        let ann_symlink = dir.path().join("selected-ann-symlink.chsw");
        symlink(&ann_target, &ann_symlink).expect("create ANN symlink");
        let ann_symlink_artifact =
            SemanticIndexArtifact::open(&fsvi_path, Some(ann_symlink.clone()))
                .expect("an ANN symlink must not disable exact FSVI serving");
        assert_eq!(ann_symlink_artifact.ann_path(), Some(ann_symlink.as_path()));
        assert_eq!(
            ann_symlink_artifact.ann_unavailable_reason(),
            Some(SemanticAnnUnavailableReason::SidecarOpenFailed)
        );

        let ann_hard_link = dir.path().join("selected-ann-hard-link.chsw");
        std::fs::hard_link(&fsvi_path, &ann_hard_link).expect("create cross-role hard link");
        let hard_link_artifact =
            SemanticIndexArtifact::open(&fsvi_path, Some(ann_hard_link.clone()))
                .expect("an aliased ANN role must not disable exact FSVI serving");
        assert_eq!(hard_link_artifact.ann_path(), Some(ann_hard_link.as_path()));
        assert_eq!(
            hard_link_artifact.ann_unavailable_reason(),
            Some(SemanticAnnUnavailableReason::SidecarOpenFailed)
        );
    }

    #[test]
    fn semantic_doc_id_roundtrip_with_hash() {
        let hash = [0u8; 32];
        let doc_id = SemanticDocId {
            message_id: 42,
            chunk_idx: 2,
            agent_id: 3,
            workspace_id: 7,
            source_id: 11,
            role: 1,
            created_at_ms: 1_700_000_000_000,
            content_hash: Some(hash),
        }
        .to_doc_id_string();
        let parsed = parse_semantic_doc_id(&doc_id).expect("parse");
        assert_eq!(parsed.message_id, 42);
        assert_eq!(parsed.chunk_idx, 2);
        assert_eq!(parsed.agent_id, 3);
        assert_eq!(parsed.workspace_id, 7);
        assert_eq!(parsed.source_id, 11);
        assert_eq!(parsed.role, 1);
        assert_eq!(parsed.created_at_ms, 1_700_000_000_000);
        assert_eq!(parsed.content_hash, Some(hash));
    }

    #[test]
    fn semantic_doc_id_roundtrip_without_hash() {
        let doc_id = SemanticDocId {
            message_id: 42,
            chunk_idx: 2,
            agent_id: 3,
            workspace_id: 7,
            source_id: 11,
            role: 1,
            created_at_ms: 1_700_000_000_000,
            content_hash: None,
        }
        .to_doc_id_string();
        let parsed = parse_semantic_doc_id(&doc_id).expect("parse");
        assert_eq!(parsed.message_id, 42);
        assert_eq!(parsed.chunk_idx, 2);
        assert_eq!(parsed.agent_id, 3);
        assert_eq!(parsed.workspace_id, 7);
        assert_eq!(parsed.source_id, 11);
        assert_eq!(parsed.role, 1);
        assert_eq!(parsed.created_at_ms, 1_700_000_000_000);
        assert_eq!(parsed.content_hash, None);
    }
}
