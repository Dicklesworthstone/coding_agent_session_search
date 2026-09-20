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
            fs::symlink_metadata(&root)?.is_dir(),
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
        let root = self.data_dir.join(VECTOR_INDEX_DIR);
        let mut protected = ProtectedPaths::default();
        if let Some(manifest) =
            read_durable_json::<SemanticManifest>(&SemanticManifest::path(&self.data_dir))?
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
                if !sync_checkpoint(&path)? {
                    return Ok(BackfillArtifactReclaimReport {
                        checkpoint_missing: true,
                        ..Default::default()
                    });
                }
            }
        }
        if let Some(manifest) = input {
            protected.manifest(&self.data_dir, manifest)?;
        }
        if let Some(output) = output {
            protected.index(output)?;
        }
        if let Some(shards) =
            read_durable_json::<SemanticShardManifest>(&SemanticShardManifest::path(&self.data_dir))?
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
        protect_selected_generation(&self.data_dir, &mut protected)?;

        // Plan the complete sweep before the first removal. Never traverse
        // generations/, shards/, quarantine/backup directories, or foreign data.
        let mut candidates = BTreeSet::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
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
                && name.strip_prefix(".backfill-reuse-").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|ch| ch.is_ascii_alphanumeric())
                })
                && !protected.contains(&path)
            {
                candidates.insert(path);
            }
            // Symlinks are never candidates, including dangling ones.
        }

        let mut report = BackfillArtifactReclaimReport::default();
        for path in candidates {
            let removed = (|| -> Result<(bool, u64)> {
                let metadata = fs::symlink_metadata(&path)?;
                let directory = metadata.is_dir();
                ensure!(
                    directory || metadata.is_file(),
                    "artifact type changed during recovery"
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

fn lock_file(path: &Path) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
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

fn read_durable_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    ensure!(
        metadata.is_file(),
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
    sync_file(path, &file)?;
    sync_directory(path.parent().context("manifest has no directory")?)?;
    Ok(Some(value))
}

fn protect_selected_generation(data_dir: &Path, protected: &mut ProtectedPaths) -> Result<()> {
    let pointer_path = SemanticCurrentPointerV1::path(data_dir);
    let Some(pointer) = read_durable_json::<SemanticCurrentPointerV1>(&pointer_path)? else {
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
    let manifest = read_durable_json::<SemanticGenerationManifestV1>(
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

fn sync_checkpoint(path: &Path) -> Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("open resumable checkpoint before reclamation"),
    };
    ensure!(
        file.metadata()?.is_file(),
        "checkpoint is not a regular file"
    );
    sync_file(path, &file)?;
    let wal = wal_path_for(path);
    match File::open(&wal) {
        Ok(file) => sync_file(&wal, &file)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("pin resumable checkpoint WAL"),
    }
    sync_directory(path.parent().context("checkpoint has no directory")?)?;
    Ok(true)
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

fn logical_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
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
