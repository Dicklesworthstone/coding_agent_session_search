// Admission policy for the existing v1 codec. All writes remain under the
// store's mutation lock; no source, manifest, or blob format is changed.
use super::*;

#[derive(Debug, Clone, Copy)]
pub struct RawMirrorRetentionPolicy {
    pub max_bytes: u64,
    pub min_free_bytes: u64,
}

impl RawMirrorRetentionPolicy {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            max_bytes: byte_setting("CASS_RAW_MIRROR_MAX_BYTES", 8 * 1024 * 1024 * 1024)?,
            min_free_bytes: byte_setting("CASS_RAW_MIRROR_MIN_FREE_BYTES", 2 * 1024 * 1024 * 1024)?,
        })
    }
}

fn byte_setting(name: &str, default: u64) -> Result<u64> {
    match dotenvy::var(name) {
        Ok(value) => value.trim().parse::<u64>()
            .with_context(|| format!("{name} must be an unsigned number of bytes")),
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("read {name}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMirrorCapturePaused {
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub available_bytes: u64,
    pub min_free_bytes: u64,
    pub requested_bytes: u64,
}

impl std::fmt::Display for RawMirrorCapturePaused {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out,
            "raw_mirror_capture_paused: used={} max={} available={} reserve={} requested={} bytes; \
             indexing can continue without a new raw copy. Free space or review `cass mirror prune` \
             (dry-run first); configure CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES. \
             Existing recovery evidence is retained",
            self.used_bytes, self.max_bytes, self.available_bytes,
            self.min_free_bytes, self.requested_bytes)
    }
}
impl std::error::Error for RawMirrorCapturePaused {}

// Unlike the best-effort status counter, admission must not silently ignore an
// unreadable subtree. Include manifests, descriptors, caches, audits and tmp.
fn measured_usage(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("cannot budget a symlinked raw-mirror subtree");
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            total = total.checked_add(metadata.len())
                .ok_or_else(|| anyhow!("raw-mirror byte inventory overflow"))?;
        } else {
            anyhow::bail!("cannot budget a non-regular raw-mirror entry");
        }
    }
    Ok(total)
}

struct Budget<'a> {
    root: &'a Path,
    policy: RawMirrorRetentionPolicy,
    used: u64,
    inventoried: bool,
}

impl Budget<'_> {
    fn reserve_with_available(&mut self, bytes: u64, available: u64) -> Result<()> {
        // Include one allocation block per new file in admission. Filesystem
        // free space is checked independently; the mirror cap is file bytes.
        let requested = bytes.saturating_add(4096);
        let next = bytes.checked_add(4096).and_then(|size| self.used.checked_add(size));
        if next.is_none_or(|next| next > self.policy.max_bytes)
            || available < self.policy.min_free_bytes
            || requested > available.saturating_sub(self.policy.min_free_bytes)
        {
            return Err(RawMirrorCapturePaused {
                used_bytes: self.used,
                max_bytes: self.policy.max_bytes,
                available_bytes: available,
                min_free_bytes: self.policy.min_free_bytes,
                requested_bytes: requested,
            }.into());
        }
        self.used = next.expect("checked budget addition");
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) -> Result<()> {
        // A cache hit that makes no writes must not rescan the entire mirror.
        if !self.inventoried {
            self.used = measured_usage(self.root)?;
            self.inventoried = true;
        }
        self.reserve_with_available(bytes, fs2::available_space(self.root)?)
    }
}

// Own only files successfully created by THIS capture. In particular, a failed
// write/read/verification never leaves a growing staging directory behind.
struct Staging {
    directory: PathBuf,
    files: Vec<PathBuf>,
}
impl Staging {
    fn new(root: &Path) -> Result<Self> {
        ensure_private_dir_descendant(root, &root.join("tmp"))?;
        let directory = unique_capture_temp_dir(root);
        fs::create_dir(&directory)?;
        let staging = Self { directory, files: Vec::new() };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staging.directory, fs::Permissions::from_mode(0o700))?;
        }
        Ok(staging)
    }

    fn write(&mut self, budget: &mut Budget<'_>, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        budget.reserve(bytes.len() as u64)?;
        let path = unique_temp_path(&self.directory, name);
        let mut file = private_create_new_file(&path)?;
        self.files.push(path.clone());
        file.write_all(bytes)?;
        sync_open_file_if_required(&file, || "sync bounded raw-mirror staging".to_string())?;
        Ok(path)
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        for path in &self.files {
            remove_temp_best_effort(path);
        }
        remove_empty_temp_dir_best_effort(&self.directory);
    }
}

fn stage_blob(
    root: &Path,
    staging: &mut Staging,
    budget: &mut Budget<'_>,
    bytes: &[u8],
    seen: &mut HashSet<String>,
    files: &mut Vec<CopyToTempResult>,
) -> Result<RawMirrorChunkRef> {
    let digest = blake3::hash(bytes).to_hex().to_string();
    let reference = RawMirrorChunkRef {
        blob_relative_path: raw_mirror_blob_relative_path(&digest)
            .ok_or_else(|| anyhow!("invalid computed raw-mirror digest"))?,
        blob_blake3: digest.clone(),
        blob_size_bytes: bytes.len() as u64,
    };
    if seen.insert(digest.clone()) && !content_addressed_blob_is_verified(root, &reference)? {
        let path = staging.write(budget, "blob", bytes)?;
        files.push(CopyToTempResult {
            temp_path: path, blob_blake3: digest, bytes_copied: bytes.len() as u64,
        });
    }
    Ok(reference)
}

fn prepare_bounded(
    root: &Path,
    input: &RawMirrorCaptureInput<'_>,
    metadata: &fs::Metadata,
    threshold: u64,
    chunk_size: usize,
    staging: &mut Staging,
    budget: &mut Budget<'_>,
) -> Result<PreparedRawMirrorContent> {
    let chunked = metadata.len() >= threshold;
    let count = metadata.len().div_ceil(chunk_size as u64);
    if chunked && count > RAW_MIRROR_MANIFEST_MAX_BYTES / 256 {
        anyhow::bail!("raw-mirror source needs too many chunks for a bounded manifest");
    }
    let mut source = open_stable_source_file(input.source_path, metadata)?;
    // Never chase a concurrently appending writer to EOF. Truncation, append,
    // in-place rewrites and replacement still fail the existing stability test.
    let mut snapshot = (&mut source).take(metadata.len());
    let buffer_size = if chunked { chunk_size } else {
        usize::try_from(metadata.len()).context("raw-mirror source does not fit memory")?
    };
    let mut buffer = vec![0_u8; buffer_size];
    let mut remaining = metadata.len();
    let mut hasher = blake3::Hasher::new();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    let mut chunks = Vec::new();
    loop {
        let length = remaining.min(buffer_size as u64) as usize;
        snapshot.read_exact(&mut buffer[..length])?;
        hasher.update(&buffer[..length]);
        chunks.push(stage_blob(root, staging, budget, &buffer[..length], &mut seen, &mut files)?);
        remaining -= length as u64;
        if remaining == 0 { break; }
    }
    if source_file_changed_during_capture(metadata, &source.metadata()?)
        || source_path_changed_identity_during_capture(input.source_path, metadata)
    {
        anyhow::bail!("raw-mirror source changed during bounded capture; retry a stable snapshot");
    }
    let content_hash = hasher.finalize().to_hex().to_string();
    let (reference, content_storage) = if chunked {
        let storage = RawMirrorContentStorage {
            kind: RAW_MIRROR_FIXED_CHUNKS_KIND.to_string(),
            content_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
            content_blake3: content_hash.clone(),
            content_size_bytes: metadata.len(),
            chunk_size_bytes: chunk_size as u64,
            chunks,
        };
        let bytes = serde_json::to_vec(&storage)?;
        if bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES {
            anyhow::bail!("raw-mirror chunk descriptor exceeds its size limit");
        }
        (stage_blob(root, staging, budget, &bytes, &mut seen, &mut files)?, Some(storage))
    } else {
        (chunks.remove(0), None)
    };
    Ok(PreparedRawMirrorContent {
        record: RawMirrorBlobRecord {
            blob_blake3: reference.blob_blake3,
            blob_size_bytes: reference.blob_size_bytes,
            source_content_blake3: content_hash,
            source_size_bytes: metadata.len(),
            content_storage,
        },
        files,
    })
}

// The legacy cache getter can refresh its persisted file after verification.
// Admission uses a read-only equivalent so that even that write is budgeted.
fn cached_record(root: &Path, key: &RawMirrorBlobCacheKey) -> Option<RawMirrorBlobRecord> {
    if !raw_mirror_blob_cache_key_is_strong(key) { return None; }
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache.lock().ok()?.get(&raw_mirror_blob_source_key(key))
        .filter(|entry| entry.cache_key == *key).cloned();
    let cached = cached.or_else(|| read_persisted_raw_mirror_blob_cache(root, key))?;
    if !raw_mirror_stored_blob_fingerprints(root, &cached.record)
        .is_some_and(|current| current == cached.stored_blob_fingerprints)
    {
        cached_raw_mirror_blob_record_is_verified(root, &cached.record).ok()?;
    }
    if !raw_mirror_blob_cache_source_is_current(key) { return None; }
    let record = cached.record.clone();
    remember_cached_raw_mirror_blob_record(cache, cached);
    Some(record)
}

fn save_cache(
    root: &Path, key: RawMirrorBlobCacheKey, record: RawMirrorBlobRecord,
    staging: &mut Staging, budget: &mut Budget<'_>,
) -> Result<()> {
    if !raw_mirror_blob_cache_key_is_strong(&key) { return Ok(()); }
    let Some(fingerprints) = raw_mirror_stored_blob_fingerprints(root, &record) else { return Ok(()); };
    let cached = CachedRawMirrorBlobRecord { cache_key: key.clone(), record, stored_blob_fingerprints: fingerprints };
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    remember_cached_raw_mirror_blob_record(cache, cached.clone());
    let entry_bytes = serde_json::to_vec(&cached)?;
    let bytes = serde_json::to_vec(&PersistedRawMirrorBlobCache {
        version: 1, entry: cached, entry_blake3: blake3::hash(&entry_bytes).to_hex().to_string(),
    })?;
    if bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES { return Ok(()); }
    let path = raw_mirror_blob_cache_path(root, &key).ok_or_else(|| anyhow!("invalid cache path"))?;
    if raw_mirror_path_has_symlink_below_root(root, &path) { anyhow::bail!("unsafe cache path"); }
    ensure_private_dir_descendant(root, path.parent().ok_or_else(|| anyhow!("cache has no parent"))?)?;
    let temporary = staging.write(budget, "cache", &bytes)?;
    fs::rename(temporary, &path)?;
    sync_parent(&path)?;
    Ok(())
}

pub(crate) fn capture(
    input: RawMirrorCaptureInput<'_>, threshold: u64, chunk_size: usize,
    policy: RawMirrorRetentionPolicy,
) -> Result<RawMirrorCaptureRecord> {
    if matches!(input.provider, "shelley" | "grok_bot" | "pi_durable") {
        anyhow::bail!("disabled_sensitive_container: source files cannot be raw-mirrored");
    }
    if threshold == 0 || chunk_size == 0 { anyhow::bail!("raw-mirror chunk sizes must be positive"); }
    let metadata = fs::symlink_metadata(input.source_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("refusing to raw-mirror a non-regular source");
    }
    let root = ensure_raw_mirror_root(input.data_dir)?;
    let _mutation = acquire_raw_mirror_mutation_lock(&root)?;
    let metadata = fs::symlink_metadata(input.source_path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("raw-mirror source changed identity while waiting for capture");
    }
    let mut budget = Budget { root: &root, policy, used: 0, inventoried: false };
    let key = raw_mirror_blob_cache_key(&input, &metadata, threshold, chunk_size);
    let cached = cached_record(&root, &key);
    let was_cached = cached.is_some();
    let mut staging = Staging::new(&root)?;
    let prepared = match cached {
        Some(record) => PreparedRawMirrorContent { record, files: Vec::new() },
        None => prepare_bounded(&root, &input, &metadata, threshold, chunk_size, &mut staging, &mut budget)?,
    };
    let record = prepared.record;
    let path_hash = raw_mirror_original_path_blake3(&input.source_path.display().to_string());
    let id = raw_mirror_manifest_id(input.provider, input.source_id, input.origin_kind,
        input.origin_host, &path_hash, &record.blob_blake3);
    let relative = raw_mirror_manifest_relative_path(&id);
    let path = root.join(&relative);
    let now = now_ms();
    let mut manifest = RawMirrorManifestFile {
        schema_version: RAW_MIRROR_SCHEMA_VERSION,
        manifest_kind: RAW_MIRROR_MANIFEST_KIND.to_string(),
        manifest_id: id.clone(),
        blob_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
        blob_relative_path: raw_mirror_blob_relative_path(&record.blob_blake3)
            .ok_or_else(|| anyhow!("invalid capture digest"))?,
        blob_blake3: record.blob_blake3.clone(),
        blob_size_bytes: record.blob_size_bytes,
        provider: input.provider.to_string(), source_id: input.source_id.to_string(),
        origin_kind: input.origin_kind.to_string(), origin_host: input.origin_host.map(ToOwned::to_owned),
        original_path: input.source_path.display().to_string(),
        redacted_original_path: redacted_original_path(input.provider, input.source_path),
        original_path_blake3: path_hash, captured_at_ms: now,
        source_mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
        source_size_bytes: record.source_size_bytes, content_storage: record.content_storage.clone(),
        compression: RawMirrorCompressionEnvelope { state: "none".to_string(), algorithm: None,
            uncompressed_size_bytes: Some(record.source_size_bytes) },
        encryption: RawMirrorEncryptionEnvelope { state: "none".to_string(), algorithm: None,
            key_id: None, envelope_version: None },
        db_links: unique_db_links(input.db_links),
        verification: RawMirrorVerificationRecord { status: "captured".to_string(),
            verifier: "cass_indexer".to_string(), content_blake3: Some(record.source_content_blake3.clone()),
            verified_at_ms: Some(now) },
        manifest_blake3: None,
    };
    manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    if bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES { anyhow::bail!("raw-mirror manifest exceeds its size limit"); }
    let existing = match fs::symlink_metadata(&path) {
        Ok(_) => Some(read_validated_raw_mirror_manifest(&root, &id)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let needs_merge = existing.as_ref().is_some_and(|old|
        input.db_links.iter().any(|link| !old.db_links.contains(link)));
    // Reserve publication BEFORE making any staged blob visible. A budget
    // refusal therefore leaves neither a partial manifest nor new orphan blobs.
    if existing.is_none() {
        budget.reserve(bytes.len() as u64)?;
    } else if needs_merge {
        budget.reserve(fs::symlink_metadata(&path)?.len().saturating_add(bytes.len() as u64))?;
    }
    let mut already_present = existing.is_some();
    for file in prepared.files {
        let target = root.join(raw_mirror_blob_relative_path(&file.blob_blake3)
            .ok_or_else(|| anyhow!("invalid staged digest"))?);
        already_present &= publish_content_addressed_temp(&root, &file.temp_path, &target, &file.blob_blake3)?;
    }
    if let Some(old) = existing {
        if needs_merge {
            merge_raw_mirror_manifest_db_links(&root, &path, input.db_links, Some(&record.blob_blake3))?;
        }
        manifest = old;
    } else {
        publish_manifest_bytes_create_new(&root, &path, &bytes, &record.blob_blake3)?;
    }
    if !was_cached {
        // Cache admission is optional; successfully preserving the source must
        // not be reported as a failed capture merely because its cache cannot fit.
        if let Err(error) = save_cache(&root, key, record.clone(), &mut staging, &mut budget) {
            tracing::debug!(%error, "raw-mirror capture cache not persisted");
        }
    }
    Ok(RawMirrorCaptureRecord {
        manifest_id: id, manifest_relative_path: relative,
        blob_relative_path: manifest.blob_relative_path, blob_blake3: record.blob_blake3,
        blob_size_bytes: manifest.blob_size_bytes,
        source_content_blake3: record.source_content_blake3, source_size_bytes: record.source_size_bytes,
        storage_kind: record.content_storage.as_ref().map_or("whole_blob_v1", |s| s.kind.as_str()).to_string(),
        chunk_count: record.content_storage.as_ref().map_or(1, |s| s.chunks.len()),
        captured_at_ms: manifest.captured_at_ms, source_mtime_ms: manifest.source_mtime_ms, already_present,
    })
}

#[cfg(test)]
mod tests {
    include!("policy_tests.rs");
}

pub(crate) fn capture_default(input: RawMirrorCaptureInput<'_>, policy: RawMirrorRetentionPolicy) -> Result<RawMirrorCaptureRecord> {
    capture(input, RAW_MIRROR_CHUNK_THRESHOLD_BYTES, RAW_MIRROR_CHUNK_SIZE_BYTES, policy)
}

mod retention {
    include!("retention.rs");
}
pub use retention::{RawMirrorAutomaticRetention, RawMirrorMaintenanceReport, maintain_before_index};
