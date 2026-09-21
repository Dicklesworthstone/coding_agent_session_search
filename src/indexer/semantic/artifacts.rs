//! Reclaim only backfill-owned scratch, never inferred obsolete serving assets.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use frankensearch::index::wal_path_for;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::search::semantic_manifest::selection::SemanticSelectionMetadata;
use crate::search::semantic_manifest::{
    BuildCheckpoint, MANIFEST_FORMAT_VERSION, SemanticCurrentPointerV1, SemanticGenerationManifestV1,
    SemanticManifest, SemanticShardManifest,
};
use crate::search::vector_index::VECTOR_INDEX_DIR;

mod inspection;

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod storage_tests;

pub use inspection::{
    BackfillArtifactCandidate, BackfillArtifactReclaimPlan, apply_backfill_artifact_plan,
    plan_backfill_artifacts,
};

const ARTIFACT_LOCK: &str = "semantic-backfill-artifacts.lock";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Logical bytes reclaimed, not allocated filesystem blocks. Failed removals
/// remain recoverable on the next pass and do not undo a durable checkpoint.
#[derive(Debug, Default, Clone, Serialize)]
pub struct BackfillArtifactReclaimReport {
    pub removed_files: u64,
    pub removed_directories: u64,
    pub reclaimed_bytes: u64,
    pub failed_paths: Vec<PathBuf>,
    pub checkpoint_missing: bool,
}

/// Explicit recovery for maintenance callers. Lock ordering matches a normal
/// CLI backfill: index-run first, then the backfill artifact lease. Neither lock
/// file is truncated or unlinked; stale metadata/PIDs do not prove ownership.
pub fn reclaim_backfill_artifacts(data_dir: &Path) -> Result<BackfillArtifactReclaimReport> {
    fs::create_dir_all(data_dir)?;
    let _index_lock = lock_file(&data_dir.join("index-run.lock"))
        .context("cannot reclaim semantic artifacts while an index writer is active")?;
    let artifacts = BackfillArtifacts::lock(data_dir)?;
    artifacts.sweep(None, None)
}

pub(super) struct BackfillArtifacts {
    data_dir: PathBuf,
    // Keep the same inode locked through discovery, engine execution and GC.
    _lock: File,
}

impl BackfillArtifacts {
    fn lock(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let data_dir = data_dir.canonicalize()?;
        let lock = lock_file(&data_dir.join(ARTIFACT_LOCK))
            .context("another semantic backfill owns its staging artifacts")?;
        let root = data_dir.join(VECTOR_INDEX_DIR);
        fs::create_dir_all(&root)?;
        ensure!(
            fs::symlink_metadata(&root)?.is_dir()
                && !is_link_or_reparse(&fs::symlink_metadata(&root)?),
            "refusing to reclaim through a symlinked vector_index directory"
        );
        Ok(Self {
            data_dir,
            _lock: lock,
        })
    }

    pub(super) fn begin(data_dir: &Path, input: &SemanticManifest) -> Result<Self> {
        let artifacts = Self::lock(data_dir)?;
        // The caller may have loaded its manifest before acquiring this lease.
        // Retain that input's references as well as the current durable ones.
        // Corrupt/future metadata is an error, never an empty protection set.
        artifacts.sweep(Some(input), None)?;
        Ok(artifacts)
    }

    pub(super) fn after_success(&self, manifest: &SemanticManifest, output: &Path) {
        // No cleanup runs on error/unwind. In particular a rename followed by
        // a failed directory fsync must retain both old and new candidates.
        // Recovery will first make the observed on-disk authority durable.
        if let Err(error) = self.sweep(Some(manifest), Some(output)) {
            tracing::warn!(%error, "semantic checkpoint committed; artifact cleanup deferred");
        }
    }

    fn sweep(
        &self,
        input: Option<&SemanticManifest>,
        output: Option<&Path>,
    ) -> Result<BackfillArtifactReclaimReport> {
        let discovery = self.discover(input, output, true)?;
        if discovery.checkpoint_missing {
            return Ok(BackfillArtifactReclaimReport {
                checkpoint_missing: true,
                ..Default::default()
            });
        }
        self.remove_candidates(discovery.candidates)
    }

    // One ownership classifier for automatic GC, operator preview and apply.
    // Preview observes metadata without fsync; apply pins the same authority
    // before it can authorize removal of any superseded checkpoint.
    fn discover(
        &self,
        input: Option<&SemanticManifest>,
        output: Option<&Path>,
        durable: bool,
    ) -> Result<Discovery> {
        let root = self.data_dir.join(VECTOR_INDEX_DIR);
        let mut metadata = RecoveryMetadata::new(durable);
        let mut protected = ProtectedPaths::default();
        let mut resumable_checkpoint = None;
        if let Some(manifest) =
            metadata.read::<SemanticManifest>(&SemanticManifest::path(&self.data_dir))?
        {
            ensure!(
                manifest.manifest_version <= MANIFEST_FORMAT_VERSION,
                "unsupported semantic manifest version; refusing artifact reclamation"
            );
            protected.manifest(&self.data_dir, &manifest)?;
            if let Some(checkpoint) = &manifest.checkpoint {
                // Also pin the resumable artifact/WAL before retiring any
                // previous copy. A dangling checkpoint is a recovery problem,
                // not permission to delete possible fallback staging files.
                let path = checkpoint_path(&self.data_dir, checkpoint);
                if !inspect_checkpoint(&path, durable)? {
                    return Ok(Discovery {
                        candidates: BTreeSet::new(),
                        protected,
                        metadata_fingerprint: metadata.fingerprint(),
                        checkpoint_missing: true,
                    });
                }
                resumable_checkpoint = Some((path, checkpoint.clone()));
            }
        }
        if let Some(manifest) = input {
            protected.manifest(&self.data_dir, manifest)?;
        }
        if let Some(output) = output {
            protected.index(output)?;
        }
        if let Some(shards) =
            metadata.read::<SemanticShardManifest>(&SemanticShardManifest::path(&self.data_dir))?
        {
            ensure!(
                shards.manifest_version <= MANIFEST_FORMAT_VERSION,
                "unsupported semantic shard manifest version; refusing artifact reclamation"
            );
            for shard in shards.shards {
                protected.index(&self.data_dir.join(shard.index_path))?;
                if let Some(path) = shard.ann_index_path {
                    protected.index(&self.data_dir.join(path))?;
                }
            }
        }
        protect_selected_generation(&self.data_dir, &mut protected, &mut metadata)?;

        // Plan the complete sweep before the first removal. Never traverse
        // generations/, shards/, quarantine/backup directories, or foreign data.
        let mut candidates = BTreeSet::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if is_link_or_reparse(&fs::symlink_metadata(&path)?) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if kind.is_file() {
                if let Some(base) = staging_base(&path, name) {
                    // A WAL next to a symlink/directory is not ours to remove.
                    match fs::symlink_metadata(&base) {
                        Ok(metadata) if !metadata.is_file() => continue,
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error).context("inspect staging WAL owner"),
                    }
                    let wal = wal_path_for(&base);
                    if !protected.contains(&base) && !protected.contains(&wal) {
                        candidates.insert(path);
                    }
                }
            } else if kind.is_dir()
                && reuse_name(name)
                && !protected.contains(&path)
            {
                candidates.insert(path);
            }
            // Symlinks are never candidates, including dangling ones.
        }

        if !candidates.is_empty()
            && let Some((path, checkpoint)) = resumable_checkpoint
        {
            // Existence and fsync alone cannot vouch for the replacement: a
            // zero-length/truncated file can survive an interrupted writer.
            // Check the current checkpoint before deleting any fallback copy.
            validate_reclaim_checkpoint(&path, &checkpoint)?;
        }

        Ok(Discovery {
            candidates,
            protected,
            metadata_fingerprint: metadata.fingerprint(),
            checkpoint_missing: false,
        })
    }

    fn remove_candidates(&self, candidates: BTreeSet<PathBuf>) -> Result<BackfillArtifactReclaimReport> {
        let root = self.data_dir.join(VECTOR_INDEX_DIR);
        let mut report = BackfillArtifactReclaimReport::default();
        for path in candidates {
            let removed = (|| -> Result<(bool, u64)> {
                let metadata = fs::symlink_metadata(&path)?;
                let directory = metadata.is_dir();
                // A staging file swapped for a directory must never turn a
                // planned unlink into a recursive deletion (or vice versa).
                let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
                ensure!(
                    path.parent() == Some(root.as_path())
                        && !is_link_or_reparse(&metadata)
                        && ((directory && reuse_name(name))
                            || (metadata.is_file() && staging_base(&path, name).is_some())),
                    "artifact type or scope changed during recovery"
                );
                let bytes = logical_bytes(&path)?;
                if directory {
                    // std::fs::remove_dir_all does not follow child symlinks.
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
                Ok((directory, bytes))
            })();
            match removed {
                Ok((directory, bytes)) => {
                    report.removed_directories += u64::from(directory);
                    report.removed_files += u64::from(!directory);
                    report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(bytes);
                }
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "semantic scratch artifact retained; cleanup will retry"
                    );
                    report.failed_paths.push(path);
                }
            }
        }
        if report.removed_files + report.removed_directories > 0 {
            sync_directory(&root)?;
            tracing::info!(
                removed_files = report.removed_files,
                removed_directories = report.removed_directories,
                reclaimed_bytes = report.reclaimed_bytes,
                "reclaimed unreferenced semantic backfill artifacts"
            );
        }
        Ok(report)
    }
}

struct Discovery {
    candidates: BTreeSet<PathBuf>,
    protected: ProtectedPaths,
    metadata_fingerprint: String,
    checkpoint_missing: bool,
}

fn reuse_name(name: &str) -> bool {
    name.strip_prefix(".backfill-reuse-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|ch| ch.is_ascii_alphanumeric())
    })
}

fn lock_file(path: &Path) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !is_link_or_reparse(&metadata),
            "lock is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

fn checkpoint_path(data_dir: &Path, checkpoint: &BuildCheckpoint) -> PathBuf {
    // This is the historical on-disk naming contract in engine.rs. Lifecycle
    // tests compare it to actual backfill outcomes rather than fixture guesses.
    let embedder: String = checkpoint
        .embedder_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let hash = crc32fast::hash(checkpoint.db_fingerprint.as_bytes());
    data_dir.join(VECTOR_INDEX_DIR).join(format!(
        ".staging-{}-{embedder}-{hash:08x}.fsvi",
        checkpoint.tier.as_str()
    ))
}

fn staging_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".fsvi") else {
        return false;
    };
    let Some(rest) = stem
        .strip_prefix(".staging-fast-")
        .or_else(|| stem.strip_prefix(".staging-quality-"))
    else {
        return false;
    };
    let Some((embedder, hash)) = rest.rsplit_once('-') else {
        return false;
    };
    !embedder.is_empty()
        && embedder
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
        && hash.len() == 8
        && hash
            .bytes()
            .all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch))
}

fn staging_base(path: &Path, name: &str) -> Option<PathBuf> {
    if staging_name(name) {
        return Some(path.to_path_buf());
    }
    // Accommodate the FSVI sidecar convention through wal_path_for itself,
    // including a WAL orphaned after its main staging file was already removed.
    let stem = name.strip_suffix(".wal")?;
    for name in [stem.to_owned(), format!("{stem}.fsvi")] {
        let base = path.with_file_name(&name);
        if staging_name(&name) && wal_path_for(&base) == path {
            return Some(base);
        }
    }
    None
}

#[derive(Default)]
struct ProtectedPaths {
    paths: BTreeSet<PathBuf>,
}

impl ProtectedPaths {
    fn manifest(&mut self, data_dir: &Path, manifest: &SemanticManifest) -> Result<()> {
        // Ownership does not depend on ready, tier, model, schema or fingerprint
        // validity. Even a stale/not-ready artifact can still be in use.
        for artifact in [&manifest.fast_tier, &manifest.quality_tier]
            .into_iter()
            .flatten()
        {
            self.index(&data_dir.join(&artifact.index_path))?;
        }
        if let Some(hnsw) = &manifest.hnsw {
            self.index(&data_dir.join(&hnsw.index_path))?;
        }
        if let Some(checkpoint) = &manifest.checkpoint {
            self.index(&checkpoint_path(data_dir, checkpoint))?;
        }
        Ok(())
    }

    fn index(&mut self, path: &Path) -> Result<()> {
        self.path(path)?;
        self.path(&wal_path_for(path))
    }

    fn path(&mut self, path: &Path) -> Result<()> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        // Resolve symlinked references too: a public index may alias a staging
        // name. Keep both the recorded spelling and its real filesystem target.
        self.paths.insert(normalize(&absolute));
        let mut ancestor = absolute.as_path();
        loop {
            match fs::canonicalize(ancestor) {
                Ok(resolved) => {
                    self.paths
                        .insert(normalize(&resolved.join(absolute.strip_prefix(ancestor)?)));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if fs::symlink_metadata(ancestor)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink())
                    {
                        bail!(
                            "unresolved manifest symlink {}; refusing reclamation",
                            ancestor.display()
                        );
                    }
                    ancestor = ancestor
                        .parent()
                        .context("cannot resolve artifact reference")?;
                }
                Err(error) => return Err(error).context("resolve protected semantic artifact"),
            }
        }
        Ok(())
    }

    fn contains(&self, candidate: &Path) -> bool {
        self.paths
            .iter()
            .any(|path| path.starts_with(candidate) || candidate.starts_with(path))
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

struct RecoveryMetadata {
    durable: bool,
    observed: blake3::Hasher,
}

impl RecoveryMetadata {
    fn new(durable: bool) -> Self {
        let mut observed = blake3::Hasher::new();
        observed.update(b"cass-backfill-reclaim-authority-v1\0");
        Self { durable, observed }
    }

    fn observe(&mut self, path: &Path, bytes: Option<&[u8]>) {
        // Automatic recovery must also work in non-UTF-8 archive directories.
        // These bytes are only a local approval witness, never a decoded path.
        let name = path.as_os_str().as_encoded_bytes();
        self.observed.update(&(name.len() as u64).to_le_bytes());
        self.observed.update(name);
        self.observed.update(&[u8::from(bytes.is_some())]);
        if let Some(bytes) = bytes {
            self.observed.update(&(bytes.len() as u64).to_le_bytes());
            self.observed.update(bytes);
        }
    }

    fn fingerprint(&self) -> String {
        self.observed.finalize().to_hex().to_string()
    }

    fn read<T: DeserializeOwned>(&mut self, path: &Path) -> Result<Option<T>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.observe(path, None);
                return Ok(None);
            },
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        ensure!(
            metadata.is_file() && !is_link_or_reparse(&metadata),
            "manifest is not a regular file: {}",
            path.display()
        );
        ensure!(
            metadata.len() <= MAX_MANIFEST_BYTES,
            "manifest too large for safe recovery: {}",
            path.display()
        );
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST_BYTES,
            "manifest grew during recovery"
        );
        let value = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}; refusing artifact reclamation", path.display()))?;
        // After a killed process, visibility of rename alone is not durability.
        // Pin the selected metadata and directory before deleting superseded data.
        self.observe(path, Some(&bytes));
        if self.durable {
            sync_file(path, &file)?;
            sync_directory(path.parent().context("manifest has no directory")?)?;
        }
        Ok(Some(value))
    }
}

fn protect_selected_generation(
    data_dir: &Path,
    protected: &mut ProtectedPaths,
    metadata: &mut RecoveryMetadata,
) -> Result<()> {
    let pointer_path = SemanticCurrentPointerV1::path(data_dir);
    let Some(pointer) = metadata.read::<SemanticCurrentPointerV1>(&pointer_path)? else {
        return Ok(());
    };
    // Share serving's exact bounded pointer/manifest authentication. This does
    // not open the potentially multi-GB vector artifacts or invent a second
    // digest/path contract for garbage collection.
    let selected = SemanticSelectionMetadata::read(data_dir, None)?;
    ensure!(
        selected.pointer == pointer,
        "semantic selection changed during recovery; refusing reclamation"
    );
    let manifest = metadata.read::<SemanticGenerationManifestV1>(
        &selected.generation_dir.join("manifest.json"),
    )?
    .context("selected generation manifest missing; refusing artifact reclamation")?;
    ensure!(
        selected.manifest == manifest,
        "semantic manifest changed during recovery; refusing reclamation"
    );
    for artifact in manifest.artifacts {
        let relative = Path::new(&artifact.relative_path);
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "unsafe selected semantic artifact path"
        );
        protected.index(&selected.generation_dir.join(relative))?;
    }
    Ok(())
}

fn inspect_checkpoint(path: &Path, durable: bool) -> Result<bool> {
    // Check types before opening: a FIFO masquerading as a checkpoint/WAL
    // must fail closed, not hang recovery while waiting for a writer.
    let Some(file) = open_checkpoint_file(path)? else {
        return Ok(false);
    };
    let wal = wal_path_for(path);
    let wal_file = open_checkpoint_file(&wal)?;
    if durable {
        sync_file(path, &file)?;
        if let Some(file) = wal_file {
            sync_file(&wal, &file)?;
        }
        sync_directory(path.parent().context("checkpoint has no directory")?)?;
    }
    Ok(true)
}

/// Reuse the engine's read-only FSVI admission rather than a second header
/// parser. This rejects an unreadable/truncated checkpoint or the wrong
/// producer before a sweep; it is not a full vector-content/coverage audit.
/// Do not compact or repair the checkpoint/WAL as a side effect of inspection.
fn validate_reclaim_checkpoint(path: &Path, checkpoint: &BuildCheckpoint) -> Result<()> {
    let index = frankensearch::index::VectorIndex::open_read_only(path).with_context(|| {
        format!(
            "resumable semantic checkpoint {} cannot be opened; retaining fallback artifacts",
            path.display()
        )
    })?;
    ensure!(
        index.embedder_id() == checkpoint.embedder_id,
        "resumable semantic checkpoint {} has producer {}, expected {}; retaining fallback artifacts",
        path.display(),
        index.embedder_id(),
        checkpoint.embedder_id
    );
    // Read-only recovery can ignore an incomplete or stale WAL batch. Even
    // this upper bound (physical main slots + replayable WAL records) must
    // cover the durable checkpoint's acknowledged count before retiring a
    // fallback. Extra records are allowed: a killed writer may have durably
    // appended its next batch without advancing the checkpoint yet.
    let available = u64::try_from(index.record_count()).unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(index.wal_record_count()).unwrap_or(u64::MAX));
    ensure!(
        available >= checkpoint.docs_embedded,
        "resumable semantic checkpoint {} has at most {} records, below its recorded {}; retaining fallback artifacts",
        path.display(),
        available,
        checkpoint.docs_embedded
    );
    Ok(())
}

fn open_checkpoint_file(path: &Path) -> Result<Option<File>> {
    match fs::metadata(path) {
        Ok(metadata) => ensure!(metadata.is_file(), "checkpoint/WAL is not a regular file"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect checkpoint before reclamation"),
    }
    let file = File::open(path).context("open checkpoint before reclamation")?;
    ensure!(file.metadata()?.is_file(), "checkpoint/WAL type changed during recovery");
    Ok(Some(file))
}

#[cfg(not(windows))]
fn sync_file(_path: &Path, file: &File) -> Result<()> {
    file.sync_all()
        .context("make referenced semantic data durable before reclamation")
}

#[cfg(windows)]
fn sync_file(path: &Path, _file: &File) -> Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .context("sync semantic artifact directory")
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<()> {
    // Match the manifest publisher's Windows directory-durability contract.
    Ok(())
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn logical_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link_or_reparse(&metadata) {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut bytes = 0u64;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            bytes = bytes.saturating_add(logical_bytes(&entry?.path())?);
        }
    }
    Ok(bytes)
}
