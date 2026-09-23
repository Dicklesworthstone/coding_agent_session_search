//! Opt-in, retained global vector retrieval using the production semantic path.
//!
//! Models, vectors and filter maps are loaded once per semantic epoch. Strict
//! archive reads and local model loaders never start writers or downloads. File
//! witnesses detect ordinary publication/replacement and same-size writes; they
//! are change detectors, not an immutable corpus-generation certificate.

use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, ensure};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::fastembed_embedder::FastEmbedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::model_manager::{
    load_hash_semantic_context_strict, load_semantic_context_for_embedder_strict,
};
use coding_agent_search::search::query::{
    FieldMask, SearchClient, SearchClientOptions, SearchFilters, SearchHit, rrf_fuse_hits,
};
use coding_agent_search::search::semantic_manifest::{
    SemanticCurrentPointerV1, SemanticManifest, SemanticShardManifest,
};
use coding_agent_search::search::vector_index::vector_index_path;
use coding_agent_search::sources::provenance::SourceFilter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Session;
use super::protocol::{self, Filters};

const MAX_GUARDED_FILES: usize = 4096;
const MAX_SHARD_MANIFEST_BYTES: u64 = 1024 * 1024;
const NATIVE_ANN_UNAVAILABLE: &str = "persistent_native_ann_requires_reclaimable_owners";

fn ann_metadata(requested: bool) -> Value {
    json!({
        "requested": requested, "used": false, "attempted": false, "stats": null,
        "unavailable_reason": requested.then_some(NATIVE_ANN_UNAVAILABLE),
        "detail": requested.then_some(
            "native ANN is disabled in the persistent service until its owners can be reclaimed"
        ),
    })
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum EmbedderChoice {
    Minilm,
    MultilingualMinilm,
    Hash,
}

impl EmbedderChoice {
    fn name(self) -> &'static str {
        match self {
            Self::Minilm => "minilm",
            Self::MultilingualMinilm => "multilingual-minilm",
            Self::Hash => "hash",
        }
    }

    fn id(self) -> Result<String> {
        match self {
            Self::Hash => Ok(HashEmbedder::default().id().to_owned()),
            _ => FastEmbedder::config_for(self.name())
                .map(|config| config.embedder_id)
                .context("unknown configured semantic embedder"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Mode {
    Semantic,
    #[default]
    Hybrid,
}

#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error(
        "semantic assets or archive changed; call reload or unload before retrying semantic retrieval"
    )]
    Changed,
    #[error("semantic retrieval is unavailable: {0}")]
    Unavailable(String),
}

pub(super) fn error_kind(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Changed) => "semantic_reload_required",
        Some(Refusal::Unavailable(_)) => "semantic_unavailable",
        None => "semantic_search_failed",
    }
}

/// Keep a handle as well as metadata so same-size path replacement is visible.
/// On Unix ctime also detects in-place writes whose mtime was restored.
#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    handle: same_file::Handle,
    bytes: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileStamp {
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "semantic service requires regular, non-symlink asset files: {}",
            path.display()
        );
        let file = File::open(path)?;
        let opened = file.metadata()?;
        ensure!(
            same_metadata(&metadata, &opened),
            "semantic asset changed while opening its witness"
        );
        Ok(Some(Self {
            handle: same_file::Handle::from_file(file)?,
            bytes: opened.len(),
            modified: opened.modified()?,
            #[cfg(unix)]
            changed: {
                use std::os::unix::fs::MetadataExt;
                (opened.ctime(), opened.ctime_nsec())
            },
        }))
    }
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    if left.len() != right.len() || left.modified().ok() != right.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (left.dev(), left.ino(), left.ctime(), left.ctime_nsec())
            != (right.dev(), right.ino(), right.ctime(), right.ctime_nsec())
        {
            return false;
        }
    }
    true
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[derive(Default)]
struct AssetGuard(BTreeMap<PathBuf, Option<FileStamp>>);

impl AssetGuard {
    fn add(&mut self, path: PathBuf) -> Result<()> {
        ensure!(
            self.0.len() < MAX_GUARDED_FILES,
            "semantic service asset witness limit exceeded"
        );
        let stamp = FileStamp::read(&path)?;
        self.0.insert(path, stamp);
        Ok(())
    }

    fn vector(&mut self, path: PathBuf) -> Result<()> {
        self.add(suffixed(&path, ".wal"))?;
        self.add(path)
    }

    fn capture(data_dir: &Path, db: &Path, embedder_id: &str) -> Result<Self> {
        let mut guard = Self::default();
        for path in [
            db.to_path_buf(),
            suffixed(db, "-wal"),
            suffixed(db, "-journal"),
            SemanticManifest::path(data_dir),
            SemanticShardManifest::path(data_dir),
            SemanticCurrentPointerV1::path(data_dir),
        ] {
            guard.add(path)?;
        }
        guard.vector(vector_index_path(data_dir, embedder_id))?;
        let shard_path = SemanticShardManifest::path(data_dir);
        if let Some(Some(stamp)) = guard.0.get(&shard_path) {
            ensure!(
                stamp.bytes <= MAX_SHARD_MANIFEST_BYTES,
                "semantic shard manifest exceeds 1 MiB"
            );
            if let Some(manifest) = SemanticShardManifest::load(data_dir)? {
                for shard in manifest
                    .shards
                    .iter()
                    .filter(|shard| shard.embedder_id == embedder_id)
                {
                    let resolve = |recorded: &str| -> Result<PathBuf> {
                        let path = Path::new(recorded);
                        ensure!(
                            !recorded.is_empty()
                                && path
                                    .components()
                                    .all(|component| matches!(component, Component::Normal(_))),
                            "semantic shard asset path must stay inside the configured data directory"
                        );
                        Ok(data_dir.join(path))
                    };
                    guard.vector(resolve(&shard.index_path)?)?;
                }
            }
        }
        guard.check()?;
        Ok(guard)
    }

    fn check(&self) -> Result<()> {
        for (path, expected) in &self.0 {
            if FileStamp::read(path).ok().as_ref() != Some(expected) {
                return Err(Refusal::Changed.into());
            }
        }
        Ok(())
    }
}

struct Configuration {
    data_dir: PathBuf,
    embedder: EmbedderChoice,
}

#[derive(Default)]
pub(super) struct Semantic {
    config: Option<Configuration>,
    client: Option<SearchClient>,
    guard: AssetGuard,
    requires_reload: bool,
    load_attempts: u64,
    successful_loads: u64,
    calls_completed: u64,
    queries_attempted: u64,
    last_failure: Option<String>,
}

impl Semantic {
    pub(super) fn new(data_dir: PathBuf, embedder: EmbedderChoice) -> Self {
        Self {
            config: Some(Configuration { data_dir, embedder }),
            ..Self::default()
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.config.is_some()
    }

    pub(super) fn model_loaded(&self) -> bool {
        self.client.is_some()
            && self
                .config
                .as_ref()
                .is_some_and(|config| !matches!(config.embedder, EmbedderChoice::Hash))
    }

    pub(super) fn archive_access_attempted(&self) -> bool {
        self.load_attempts != 0
    }

    pub(super) fn unload(&mut self) {
        drop(self.client.take());
        self.guard = AssetGuard::default();
        self.requires_reload = false;
        self.last_failure = None;
    }

    pub(super) fn status(&self) -> Value {
        json!({
            "enabled": self.enabled(), "loaded": self.client.is_some(),
            "embedder": self.config.as_ref().map(|config| config.embedder),
            "neural": self.config.as_ref().map(|config| !matches!(config.embedder, EmbedderChoice::Hash)),
            "context_epoch": self.client.as_ref().map(|_| self.successful_loads),
            "load_attempts": self.load_attempts, "successful_loads": self.successful_loads,
            "calls_completed": self.calls_completed, "requires_reload": self.requires_reload,
            "queries_attempted": self.queries_attempted,
            "last_failure": self.last_failure, "change_detection": "file_identity_size_mtime_and_unix_ctime",
            "snapshot_policy": "retained_assets_with_archive_change_checks", "immutable_generation_certified": false,
            "canonical_database_access_enabled": self.enabled(), "maintenance_performed": false,
            "ann_supported": false, "ann_unavailable_reason": NATIVE_ANN_UNAVAILABLE,
        })
    }

    fn ensure_loaded(&mut self, db: &Path) -> Result<()> {
        if self.requires_reload {
            return Err(Refusal::Changed.into());
        }
        if self.client.is_some() {
            return self.check();
        }
        let config = self
            .config
            .as_ref()
            .context("semantic service is disabled")?;
        self.load_attempts = self.load_attempts.saturating_add(1);
        let guard = AssetGuard::capture(&config.data_dir, db, &config.embedder.id()?)?;
        let setup = match config.embedder {
            EmbedderChoice::Hash => load_hash_semantic_context_strict(&config.data_dir, db),
            _ => load_semantic_context_for_embedder_strict(
                &config.data_dir,
                db,
                config.embedder.name(),
            ),
        };
        let context = setup
            .context
            .ok_or_else(|| Refusal::Unavailable(setup.availability.summary()))?;
        // Every retained artifact must have been witnessed before model_manager
        // selected/opened it. Unrecognized future layouts fail closed.
        for artifact in context
            .artifacts
            .iter()
            .chain(context.quality_artifact.iter())
        {
            ensure!(
                guard
                    .0
                    .get(artifact.fsvi_path())
                    .is_some_and(Option::is_some),
                "semantic artifact lacks a pre-admission file witness"
            );
        }
        let client = SearchClient::open_semantic(
            db,
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .context("canonical semantic archive is missing")?;
        client.set_semantic_artifacts_context(
            context.embedder,
            context.artifacts,
            context.quality_artifact,
            context.filter_maps,
            context.roles,
        )?;
        guard.check()?;
        self.client = Some(client);
        self.guard = guard;
        self.successful_loads = self.successful_loads.saturating_add(1);
        self.last_failure = None;
        Ok(())
    }

    fn check(&mut self) -> Result<()> {
        if self.requires_reload || self.guard.check().is_err() {
            drop(self.client.take());
            self.guard = AssetGuard::default();
            self.requires_reload = true;
            return Err(Refusal::Changed.into());
        }
        Ok(())
    }
}

fn search_filters(filters: Filters) -> SearchFilters {
    SearchFilters {
        agents: filters.agents.into_iter().collect(),
        workspaces: filters.workspaces.into_iter().collect(),
        source_filter: filters
            .source_id
            .map_or(SourceFilter::All, SourceFilter::SourceId),
        created_from: filters.created_from,
        created_to: filters.created_to,
        session_paths: Default::default(),
    }
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn semantic_search(
        &mut self,
        query: &str,
        mode: Mode,
        approximate: bool,
        filters: Filters,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        let started = Instant::now();
        let fallback_filters = filters.clone();
        let reused = self.semantic.client.is_some();
        let lease = if self.reader_lease.is_none() {
            self.acquire_reader_lease()?
        } else {
            None
        };
        let archive = self
            .archive
            .as_deref()
            .context("semantic search requires an explicit --db")?;
        let setup = self.semantic.ensure_loaded(archive);
        let setup_ms = started.elapsed().as_millis();
        if setup.is_ok() && lease.is_some() {
            self.reader_lease = lease;
        } else {
            drop(lease);
        }
        let started = Instant::now();
        // Independent retrieval introduces semantic candidates absent from the
        // lexical shortlist. Both legs use a bounded page candidate window.
        let window = (offset + limit + 1)
            .saturating_mul(3)
            .min(protocol::MAX_WINDOW);
        let semantic = setup.and_then(|()| -> Result<(Vec<SearchHit>, Value)> {
            let client = self
                .semantic
                .client
                .as_ref()
                .context("semantic reader is not loaded")?;
            self.semantic.queries_attempted = self.semantic.queries_attempted.saturating_add(1);
            let (hits, stats) = client.search_semantic(
                query,
                search_filters(filters.clone()),
                if mode == Mode::Hybrid {
                    window
                } else {
                    limit + 1
                },
                if mode == Mode::Hybrid { 0 } else { offset },
                FieldMask::new(false, true, true, false),
                // The pinned native ANN backend leaks a data mapping during
                // graph admission. A persistent worker cannot honor unload
                // after that allocation. Exact vector owners are reclaimable;
                // do not invoke either ANN search or lazy ANN diagnostics.
                false,
            )?;
            ensure!(
                stats.is_none(),
                "exact semantic service unexpectedly invoked native ANN"
            );
            let metadata = ann_metadata(approximate);
            self.semantic.check()?;
            self.semantic.calls_completed = self.semantic.calls_completed.saturating_add(1);
            Ok((hits, metadata))
        });
        let (mut hits, realized, ann, lexical_degrade) = match semantic {
            Ok((semantic_hits, ann)) if mode == Mode::Hybrid => {
                let lexical = self.ensure_loaded().and_then(|()| {
                    self.client
                        .as_ref()
                        .context("lexical reader is not loaded")?
                        .search(
                            query,
                            search_filters(filters),
                            window,
                            0,
                            FieldMask::new(false, true, true, false),
                        )
                });
                match lexical {
                    Ok(lexical_hits) => (
                        rrf_fuse_hits(&lexical_hits, &semantic_hits, query, limit + 1, offset),
                        "hybrid",
                        ann,
                        None,
                    ),
                    Err(error) => (
                        semantic_hits
                            .into_iter()
                            .skip(offset)
                            .take(limit + 1)
                            .collect(),
                        "semantic",
                        ann,
                        Some(format!("{error:#}")),
                    ),
                }
            }
            Ok((hits, ann)) => (hits, "semantic", ann, None),
            Err(error) => {
                let reason = format!("{error:#}");
                self.semantic.last_failure = Some(reason.clone());
                if mode == Mode::Semantic {
                    return Err(error);
                }
                let mut lexical = self.search(query, filters, limit, offset)?;
                lexical["requested_mode"] = json!(mode);
                lexical["realized_mode"] = json!("lexical");
                lexical["semantic_fallback_reason"] = json!(reason);
                lexical["semantic_reused"] = json!(reused);
                lexical["semantic_setup_ms"] = json!(setup_ms);
                lexical["ann"] = ann_metadata(approximate);
                return Ok(lexical);
            }
        };
        // Check again after the lexical leg before returning any fused result.
        // A changed archive never turns stale semantic coordinates into success.
        if let Err(error) = self.semantic.check() {
            let reason = format!("{error:#}");
            self.semantic.last_failure = Some(reason.clone());
            if mode == Mode::Semantic {
                return Err(error);
            }
            let mut lexical = self.search(query, fallback_filters, limit, offset)?;
            lexical["requested_mode"] = json!(mode);
            lexical["realized_mode"] = json!("lexical");
            lexical["semantic_fallback_reason"] = json!(reason);
            lexical["semantic_reused"] = json!(reused);
            lexical["semantic_setup_ms"] = json!(setup_ms);
            lexical["ann"] = ann_metadata(approximate);
            return Ok(lexical);
        }
        ensure!(
            hits.len() <= limit + 1,
            "semantic backend exceeded the requested page"
        );
        let has_next = hits.len() > limit;
        let next_offset = offset.checked_add(limit).filter(|next| {
            has_next
                && next
                    .checked_add(limit)
                    .and_then(|n| n.checked_add(1))
                    .is_some_and(|n| n <= protocol::MAX_WINDOW)
        });
        hits.truncate(limit);
        let hits = super::summarize_hits(hits)?;
        self.queries_completed = self.queries_completed.saturating_add(1);
        Ok(json!({
            "hits": hits, "count": hits.len(), "limit": limit, "offset": offset,
            "has_more": if has_next { Some(true) } else { None }, "next_offset": next_offset,
            "page_window_exhausted": has_next && next_offset.is_none(), "preview_only": true,
            "requested_mode": mode, "realized_mode": realized,
            "semantic_fallback_reason": null, "lexical_degrade_reason": lexical_degrade,
            "semantic_reused": reused, "semantic_setup_ms": setup_ms, "search_ms": started.elapsed().as_millis(),
            "candidate_window_per_leg": if mode == Mode::Hybrid { window } else { limit + offset + 1 },
            "ann": ann, "snapshot": self.status(),
        }))
    }
}
