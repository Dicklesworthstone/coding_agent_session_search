// Admission policy for raw-mirror captures. All writes remain under the store's
// mutation lock, and budgets account for the bytes actually stored on disk.
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
        Ok(value) => value
            .trim()
            .parse::<u64>()
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
        write!(
            out,
            "raw_mirror_capture_paused: used={} max={} available={} reserve={} requested={} bytes; \
             indexing can continue without a new raw copy. Free space or review `cass mirror prune` \
             (dry-run first); configure CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES. \
             Existing recovery evidence is retained",
            self.used_bytes,
            self.max_bytes,
            self.available_bytes,
            self.min_free_bytes,
            self.requested_bytes
        )
    }
}
impl std::error::Error for RawMirrorCapturePaused {}

// Unlike the best-effort status counter, admission must not silently ignore an
// unreadable subtree. Include manifests, descriptors, caches, audits and tmp.
fn measured_usage(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut path = root.to_path_buf();
    let mut directories = Vec::new();
    loop {
        let metadata = raw_mirror_symlink_metadata(&path)
            .with_context(|| format!("inventory raw-mirror entry {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "cannot budget a symlinked raw-mirror subtree {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            // Keep one directory cursor per level instead of retaining every
            // sibling path. The format uses only a few levels; refuse hostile
            // nesting before it can exhaust file descriptors or memory.
            anyhow::ensure!(
                directories.len() < 64,
                "raw-mirror inventory exceeds 64 directory levels"
            );
            let entries = raw_mirror_io("read_dir", &path, || fs::read_dir(&path))?;
            directories.push((path.clone(), entries));
        } else if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| anyhow!("raw-mirror byte inventory overflow"))?;
        } else {
            anyhow::bail!(
                "cannot budget a non-regular raw-mirror entry {}",
                path.display()
            );
        }
        loop {
            let Some((directory, entries)) = directories.last_mut() else {
                return Ok(total);
            };
            match raw_mirror_io("read_dir_next", directory, || entries.next().transpose())? {
                Some(entry) => {
                    path = entry.path();
                    break;
                }
                None => {
                    directories.pop();
                }
            }
        }
    }
}

/// Mirror usage from `measured_usage`, shared by every capture in this
/// process. A walk per capture made indexing quadratic in mirror files: at
/// 13,500 sessions each capture stat-ed ~40,000 files and a full index slowed
/// from 12 to 2.6 sessions/s. A walk is reused until it is older than
/// max(60 s, 20 times its own duration), and a prune in this process drops it.
/// Admissions add their reservations, which count 4 KiB per file and so
/// overstate small files. That conservative upper bound can admit a write;
/// the walked bytes, adjusted for replacements that shrink, are a lower bound
/// that can refuse one. Only ambiguous or expired decisions walk again. An
/// arbitrary margin around the cap made every near-full watch capture walk
/// the whole mirror even when either bound already proved the decision.
static MEASURED_USAGE: OnceLock<Mutex<HashMap<PathBuf, MeasuredUsage>>> = OnceLock::new();
const MEASURED_USAGE_MAX_ROOTS: usize = 256;

#[derive(Debug, Clone, Copy)]
struct MeasuredUsage {
    /// Bytes found by the last walk, minus subsequent known shrinkage.
    measured: u64,
    /// Bytes reserved by admissions since that walk.
    admitted: u64,
    measured_at: Option<std::time::Instant>,
    walk: Duration,
    walks: u64,
    mutation_generation: u64,
}

fn measured_usage_cache() -> std::sync::MutexGuard<'static, HashMap<PathBuf, MeasuredUsage>> {
    MEASURED_USAGE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const USAGE_GENERATION_FILE: &str = ".usage-generation";

fn read_usage_generation(root: &Path) -> Result<u64> {
    let path = root.join(USAGE_GENERATION_FILE);
    let generation = match raw_mirror_symlink_metadata(&path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == 16,
                "invalid raw-mirror usage generation"
            );
            let mut file = open_stable_source_file(&path, &metadata)?;
            let mut bytes = [0_u8; 16];
            raw_mirror_io("read_usage_generation", &path, || {
                file.read_exact(&mut bytes)
            })?;
            let (magic, value) = bytes.split_at(8);
            anyhow::ensure!(magic == b"CASSRMU1", "invalid raw-mirror usage generation");
            let mut value_bytes = [0_u8; 8];
            value_bytes.copy_from_slice(value);
            u64::from_le_bytes(value_bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error).context("inspect raw-mirror usage generation"),
    };
    anyhow::ensure!(
        generation < u64::MAX,
        "raw-mirror usage generation exhausted"
    );
    Ok(generation)
}

fn make_usage_cache_room(cache: &mut HashMap<PathBuf, MeasuredUsage>, root: &Path) {
    if !cache.contains_key(root)
        && cache.len() >= MEASURED_USAGE_MAX_ROOTS
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.measured_at)
            .map(|(path, _)| path.clone())
    {
        cache.remove(&oldest);
    }
}

/// Fence cached usage before a writer changes the mirror. The caller holds
/// the existing mutation lock. Previews must never call this function.
/// All cooperating writers must use this protocol; older writers cannot
/// invalidate another process's in-memory inventory.
pub(super) fn note_mutation(root: &Path) -> Result<()> {
    let path = root.join(USAGE_GENERATION_FILE);
    let previous = read_usage_generation(root)?;
    let next = previous + 1;
    let mut bytes = [0_u8; 16];
    let (magic, value) = bytes.split_at_mut(8);
    magic.copy_from_slice(b"CASSRMU1");
    value.copy_from_slice(&next.to_le_bytes());
    // Atomic replacement never writes through a hard-linked destination.
    // A crash may invalidate a cache unnecessarily, but cannot publish data
    // under a generation that was never made visible.
    replace_raw_mirror_bytes(root, &path, &bytes)?;
    let mut cache = measured_usage_cache();
    make_usage_cache_room(&mut cache, root);
    let entry = cache.entry(root.to_path_buf()).or_insert(MeasuredUsage {
        measured: 0,
        admitted: 0,
        measured_at: None,
        walk: Duration::ZERO,
        walks: 0,
        mutation_generation: previous,
    });
    if entry.mutation_generation != previous {
        entry.measured_at = None;
        entry.admitted = 0;
    }
    entry.mutation_generation = next;
    Ok(())
}

/// Replacements outside a capture's Budget can grow or shrink an existing
/// file. Growth counts toward the conservative upper bound; subtract known
/// shrinkage from the lower bound too, so a cached refusal stays sound.
pub(super) fn record_file_size_change(root: &Path, previous: u64, current: u64) {
    if let Some(entry) = measured_usage_cache().get_mut(root) {
        if current >= previous {
            entry.admitted = entry.admitted.saturating_add(current - previous);
        } else {
            // A replacement may refer to a file published since the inventory.
            // Subtract from the total upper bound without turning excess
            // shrinkage into an underestimate through saturating arithmetic.
            let shrinkage = previous - current;
            let from_measured = shrinkage.min(entry.measured);
            entry.measured -= from_measured;
            entry.admitted = entry.admitted.saturating_sub(shrinkage - from_measured);
        }
    }
}

/// Usage to admit `bytes` against under `policy`: the shared total when it
/// decides clearly (see MEASURED_USAGE), otherwise a new walk.
fn shared_usage(root: &Path, policy: RawMirrorRetentionPolicy, bytes: u64) -> Result<u64> {
    // Admission can now precede our own mutation fence. Never reuse an
    // inventory from before a peer's capture or prune, even on a refusal.
    let generation = read_usage_generation(root)?;
    let previous = measured_usage_cache().get(root).copied();
    if let Some(entry) = previous
        && entry.mutation_generation == generation
        && let Some(measured_at) = entry.measured_at
    {
        let refresh_after = entry.walk.saturating_mul(20).max(Duration::from_secs(60));
        let request = bytes.saturating_add(4096);
        let estimated = entry.measured.saturating_add(entry.admitted);
        if measured_at.elapsed() < refresh_after {
            if estimated.saturating_add(request) <= policy.max_bytes {
                return Ok(estimated);
            }
            if entry.measured.saturating_add(request) > policy.max_bytes {
                return Ok(entry.measured);
            }
        }
    }
    let started = std::time::Instant::now();
    // The caller holds this mirror's mutation lock. Do not hold the global
    // cache mutex across filesystem I/O: a slow store must not block captures
    // or progress reporting for unrelated stores.
    let measured = measured_usage(root)?;
    let mut cache = measured_usage_cache();
    make_usage_cache_room(&mut cache, root);
    cache.insert(
        root.to_path_buf(),
        MeasuredUsage {
            measured,
            admitted: 0,
            measured_at: Some(std::time::Instant::now()),
            walk: started.elapsed(),
            walks: previous.map_or(0, |entry| entry.walks) + 1,
            mutation_generation: generation,
        },
    );
    Ok(measured)
}

fn record_admitted_usage(root: &Path, used: u64) {
    if let Some(entry) = measured_usage_cache().get_mut(root) {
        entry.admitted = used.saturating_sub(entry.measured);
    }
}

/// A prune in this process removes files, so the next admission walks again.
pub(super) fn forget_measured_usage(root: &Path) {
    measured_usage_cache().remove(root);
}

#[cfg(test)]
fn measured_usage_walks(root: &Path) -> u64 {
    measured_usage_cache()
        .get(root)
        .map_or(0, |entry| entry.walks)
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
        let next = bytes
            .checked_add(4096)
            .and_then(|size| self.used.checked_add(size));
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
            }
            .into());
        }
        self.used = next.expect("checked budget addition");
        Ok(())
    }

    fn reserve(&mut self, bytes: u64) -> Result<()> {
        // A cache hit that makes no writes must not rescan the entire mirror.
        if !self.inventoried {
            self.used = shared_usage(self.root, self.policy, bytes)?;
            self.inventoried = true;
        }
        let available = raw_mirror_io("available_space", self.root, || {
            fs2::available_space(self.root)
        })?;
        self.reserve_with_available(bytes, available)?;
        record_admitted_usage(self.root, self.used);
        Ok(())
    }
}

// Own only files successfully created by THIS capture. In particular, a failed
// write/read/verification never leaves a growing staging directory behind.
struct Staging {
    root: PathBuf,
    directory: Option<PathBuf>,
    files: Vec<PathBuf>,
    mutation_noted: bool,
}
impl Staging {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            directory: None,
            files: Vec::new(),
            mutation_noted: false,
        }
    }

    fn note_mutation(&mut self) -> Result<()> {
        if !self.mutation_noted {
            note_mutation(&self.root)?;
            self.mutation_noted = true;
        }
        Ok(())
    }

    fn write(&mut self, budget: &mut Budget<'_>, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        budget.reserve(bytes.len() as u64)?;
        self.note_mutation()?;
        if self.directory.is_none() {
            ensure_private_dir_descendant(&self.root, &self.root.join("tmp"))?;
            let directory = unique_capture_temp_dir(&self.root);
            raw_mirror_io("create_staging_dir", &directory, || {
                fs::create_dir(&directory)
            })?;
            self.directory = Some(directory);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let directory = self.directory.as_ref().expect("created staging directory");
                raw_mirror_tracked_io("set_staging_permissions", directory, || {
                    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                })?;
            }
        }
        let directory = self
            .directory
            .as_ref()
            .expect("initialized staging directory");
        let path = unique_temp_path(directory, name);
        let mut file = private_create_new_file(&path)?;
        self.files.push(path.clone());
        raw_mirror_io("write_staging", &path, || file.write_all(bytes))?;
        sync_open_file_if_required(&file, &path, || {
            "sync bounded raw-mirror staging".to_string()
        })?;
        Ok(path)
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        for path in &self.files {
            remove_temp_best_effort(path);
        }
        if let Some(directory) = &self.directory {
            remove_empty_temp_dir_best_effort(directory);
        }
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
        encoding: None,
        content_blake3: None,
        content_size_bytes: None,
    };
    if seen.insert(digest.clone()) && !content_addressed_blob_is_verified(root, &reference)? {
        let path = staging.write(budget, "blob", bytes)?;
        files.push(CopyToTempResult {
            temp_path: path,
            blob_blake3: digest,
            bytes_copied: bytes.len() as u64,
        });
    }
    Ok(reference)
}

struct ReusableSourceChunk {
    reference: RawMirrorChunkRef,
    fingerprint: RawMirrorStoredBlobFingerprint,
}

/// Previous source bytes are never trusted here. The caller still reads and
/// hashes every byte of the new source. Only an unchanged, previously verified
/// stored chunk may avoid another read or compression pass (GH #557).
fn reusable_source_chunks(
    root: &Path,
    key: &RawMirrorBlobCacheKey,
) -> Option<HashMap<String, ReusableSourceChunk>> {
    if !raw_mirror_blob_cache_key_is_strong(key) {
        return None;
    }
    let source_key = raw_mirror_blob_source_key(key);
    let cached = BLOB_CAPTURE_CACHE
        .get()
        .and_then(|cache| cache.lock().ok()?.get(&source_key).cloned())
        .or_else(|| read_persisted_raw_mirror_blob_cache_for_source(root, key))?;
    if raw_mirror_blob_source_key(&cached.cache_key) != source_key
        || !raw_mirror_blob_cache_key_is_strong(&cached.cache_key)
    {
        return None;
    }
    validate_raw_mirror_blob_record(&cached.record).ok()?;
    let storage = cached.record.content_storage.as_ref()?;
    // The advisory cache checksum alone cannot establish the mapping from
    // compressed bytes to decoded content. Bind that mapping to a real,
    // content-addressed descriptor before trusting any stored chunk witness.
    let descriptor = RawMirrorChunkRef {
        blob_relative_path: raw_mirror_blob_relative_path(&cached.record.blob_blake3)?,
        blob_blake3: cached.record.blob_blake3.clone(),
        blob_size_bytes: cached.record.blob_size_bytes,
        encoding: None,
        content_blake3: None,
        content_size_bytes: None,
    };
    let bytes =
        read_bounded_raw_mirror_blob(root, &descriptor, RAW_MIRROR_MANIFEST_MAX_BYTES).ok()?;
    let observed: RawMirrorContentStorage = serde_json::from_slice(&bytes).ok()?;
    if &observed != storage {
        return None;
    }
    let fingerprints: HashMap<_, _> = cached
        .stored_blob_fingerprints
        .into_iter()
        .map(|fingerprint| (fingerprint.blob_relative_path.clone(), fingerprint))
        .collect();
    let mut chunks = HashMap::new();
    for mut reference in observed.chunks {
        let Some(fingerprint) = fingerprints.get(&reference.blob_relative_path) else {
            continue;
        };
        let digest = raw_mirror_chunk_content_hash(&reference).to_owned();
        let size = raw_mirror_chunk_content_size(&reference);
        // A v1 plain chunk remains readable inside a new v2 descriptor, with
        // its content identity made explicit and its stored bytes unchanged.
        reference.encoding.get_or_insert_with(|| "none".to_string());
        reference.content_blake3 = Some(digest.clone());
        reference.content_size_bytes = Some(size);
        chunks.entry(digest).or_insert_with(|| ReusableSourceChunk {
            reference,
            fingerprint: fingerprint.clone(),
        });
    }
    Some(chunks)
}

fn stage_source_chunk(
    root: &Path,
    staging: &mut Staging,
    budget: &mut Budget<'_>,
    bytes: &[u8],
    reusable: &HashMap<String, ReusableSourceChunk>,
    seen: &mut HashSet<String>,
    files: &mut Vec<CopyToTempResult>,
) -> Result<RawMirrorChunkRef> {
    let content_blake3 = blake3::hash(bytes).to_hex().to_string();
    if let Some(previous) = reusable.get(&content_blake3)
        && raw_mirror_chunk_content_size(&previous.reference) == bytes.len() as u64
    {
        let verified = if raw_mirror_stored_blob_fingerprint(root, &previous.reference)
            .is_some_and(|current| current == previous.fingerprint)
        {
            true
        } else if content_addressed_blob_is_verified(root, &previous.reference)? {
            // Changed metadata requires both stored and decoded verification;
            // missing chunks fall through to capture from the current source.
            stream_verified_raw_mirror_chunk(root, &previous.reference, |_| Ok(()))?;
            true
        } else {
            false
        };
        if verified {
            seen.insert(previous.reference.blob_blake3.clone());
            return Ok(previous.reference.clone());
        }
    }
    let plain = RawMirrorChunkRef {
        blob_relative_path: raw_mirror_blob_relative_path(&content_blake3)
            .ok_or_else(|| anyhow!("invalid computed raw-mirror content digest"))?,
        blob_blake3: content_blake3.clone(),
        blob_size_bytes: bytes.len() as u64,
        encoding: None,
        content_blake3: None,
        content_size_bytes: None,
    };
    // A codec upgrade must not rewrite a previously preserved prefix just
    // because its source grew. Reuse verified legacy plain bytes before
    // considering compression, including identical chunks staged in this run.
    let (mut reference, encoding) =
        if seen.contains(&content_blake3) || content_addressed_blob_is_verified(root, &plain)? {
            seen.insert(content_blake3.clone());
            (plain, "none")
        } else {
            let compressed = crate::raw_mirror::codec::compress_chunk(bytes)?;
            if compressed.len() < bytes.len() {
                (
                    stage_blob(root, staging, budget, &compressed, seen, files)?,
                    "zstd",
                )
            } else {
                (
                    stage_blob(root, staging, budget, bytes, seen, files)?,
                    "none",
                )
            }
        };
    reference.encoding = Some(encoding.to_string());
    reference.content_blake3 = Some(content_blake3);
    reference.content_size_bytes = Some(bytes.len() as u64);
    Ok(reference)
}

/// The ordinary snapshot keeps its strict metadata checks. A proven prefix
/// instead carries the original byte/object receipt for a fixed historical
/// range; later bytes are outside this raw capture.
struct CaptureSource<'a> {
    metadata: &'a fs::Metadata,
    prefix: Option<&'a crate::connectors::codex::StablePrefixReceipt>,
}

fn prepare_bounded(
    root: &Path,
    input: &RawMirrorCaptureInput<'_>,
    source: CaptureSource<'_>,
    threshold: u64,
    chunk_size: usize,
    staging: &mut Staging,
    budget: &mut Budget<'_>,
) -> Result<PreparedRawMirrorContent> {
    anyhow::ensure!(
        chunk_size > 0 && chunk_size <= crate::raw_mirror::codec::MAX_DECODED_CHUNK_BYTES,
        "raw-mirror chunk size exceeds the bounded codec policy"
    );
    let CaptureSource { metadata, prefix } = source;
    let source_bytes = prefix.map_or(metadata.len(), |receipt| receipt.committed_bytes);
    let chunked = source_bytes >= threshold;
    let count = source_bytes.div_ceil(chunk_size as u64);
    if chunked && count > RAW_MIRROR_MANIFEST_MAX_BYTES / 512 {
        anyhow::bail!("raw-mirror source needs too many chunks for a bounded manifest");
    }
    let reusable = if chunked && prefix.is_none() {
        let key = raw_mirror_blob_cache_key(input, metadata, threshold, chunk_size);
        reusable_source_chunks(root, &key).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let mut source = open_stable_source_file(input.source_path, metadata)?;
    if let Some(receipt) = prefix {
        receipt.validate_opened_source(&source)?;
        source.rewind()?;
    }
    // Never chase a writer to a later EOF. Ordinary captures retain the
    // unchanged whole-snapshot checks below. Prefixes are fixed at the
    // admitted byte boundary and must match its independently recorded hash.
    let mut snapshot = (&mut source).take(source_bytes);
    let buffer_size = if chunked {
        chunk_size
    } else {
        usize::try_from(source_bytes).context("raw-mirror source does not fit memory")?
    };
    let mut buffer = vec![0_u8; buffer_size];
    let mut remaining = source_bytes;
    let mut hasher = blake3::Hasher::new();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    let mut chunks = Vec::new();
    loop {
        let length = remaining.min(buffer_size as u64) as usize;
        raw_mirror_io("read_source_chunk", input.source_path, || {
            snapshot.read_exact(&mut buffer[..length])
        })?;
        hasher.update(&buffer[..length]);
        chunks.push(if chunked {
            stage_source_chunk(
                root,
                staging,
                budget,
                &buffer[..length],
                &reusable,
                &mut seen,
                &mut files,
            )?
        } else {
            stage_blob(
                root,
                staging,
                budget,
                &buffer[..length],
                &mut seen,
                &mut files,
            )?
        });
        remaining -= length as u64;
        if remaining == 0 {
            break;
        }
    }
    let content_hash = hasher.finalize().to_hex().to_string();
    if let Some(receipt) = prefix {
        anyhow::ensure!(
            content_hash == receipt.blake3,
            "prefix capture bytes do not match the parsed source receipt"
        );
        // Verify the held object as well as its current name. A replacement,
        // in-place rewrite, truncation or new advisory lock cannot authorize
        // publication merely because a bounded read already finished.
        receipt.validate_opened_source(&source)?;
    } else if source_file_changed_during_capture(
        metadata,
        &raw_mirror_io("fstat_source", input.source_path, || source.metadata())?,
    ) || source_path_changed_identity_during_capture(input.source_path, metadata)
    {
        anyhow::bail!("raw-mirror source changed during bounded capture; retry a stable snapshot");
    }
    let (reference, content_storage) = if chunked {
        let storage = RawMirrorContentStorage {
            kind: RAW_MIRROR_FIXED_CHUNKS_V2_KIND.to_string(),
            content_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
            content_blake3: content_hash.clone(),
            content_size_bytes: source_bytes,
            chunk_size_bytes: chunk_size as u64,
            chunks,
        };
        let bytes = serde_json::to_vec(&storage)?;
        if bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES {
            anyhow::bail!("raw-mirror chunk descriptor exceeds its size limit");
        }
        (
            stage_blob(root, staging, budget, &bytes, &mut seen, &mut files)?,
            Some(storage),
        )
    } else {
        (chunks.remove(0), None)
    };
    Ok(PreparedRawMirrorContent {
        record: RawMirrorBlobRecord {
            blob_blake3: reference.blob_blake3,
            blob_size_bytes: reference.blob_size_bytes,
            source_content_blake3: content_hash,
            source_size_bytes: source_bytes,
            content_storage,
        },
        files,
    })
}

// The legacy cache getter can refresh its persisted file after verification.
// Admission uses a read-only equivalent so that even that write is budgeted.
fn cached_record(root: &Path, key: &RawMirrorBlobCacheKey) -> Option<RawMirrorBlobRecord> {
    if !raw_mirror_blob_cache_key_is_strong(key) {
        return None;
    }
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .ok()?
        .get(&raw_mirror_blob_source_key(key))
        .filter(|entry| entry.cache_key == *key)
        .cloned();
    let cached = cached.or_else(|| read_persisted_raw_mirror_blob_cache(root, key))?;
    if !raw_mirror_stored_blob_fingerprints(root, &cached.record)
        .is_some_and(|current| current == cached.stored_blob_fingerprints)
        && cached_raw_mirror_blob_record_is_verified(root, &cached.record).is_err()
    {
        remove_cached_raw_mirror_blob_record_if_unchanged(cache, key, &cached.record);
        return None;
    }
    if !raw_mirror_blob_cache_source_is_current(key) {
        return None;
    }
    let record = cached.record.clone();
    remember_cached_raw_mirror_blob_record(cache, cached);
    Some(record)
}

#[cfg(test)]
pub(super) fn prepare_for_test(
    root: &Path,
    source_path: &Path,
    metadata: &fs::Metadata,
    threshold: u64,
    chunk_size: usize,
) -> Result<PreparedRawMirrorContent> {
    let input = RawMirrorCaptureInput {
        data_dir: root,
        provider: "codex",
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path,
        db_links: &[],
    };
    let mut staging = Staging::new(root);
    let mut budget = Budget {
        root,
        policy: RawMirrorRetentionPolicy::from_env()?,
        used: 0,
        inventoried: false,
    };
    prepare_bounded(
        root,
        &input,
        CaptureSource {
            metadata,
            prefix: None,
        },
        threshold,
        chunk_size,
        &mut staging,
        &mut budget,
    )
}

fn save_cache(
    root: &Path,
    key: RawMirrorBlobCacheKey,
    record: RawMirrorBlobRecord,
    staging: &mut Staging,
    budget: &mut Budget<'_>,
) -> Result<()> {
    if !raw_mirror_blob_cache_key_is_strong(&key) {
        return Ok(());
    }
    let Some(fingerprints) = raw_mirror_stored_blob_fingerprints(root, &record) else {
        return Ok(());
    };
    let cached = CachedRawMirrorBlobRecord {
        cache_key: key.clone(),
        record,
        stored_blob_fingerprints: fingerprints,
    };
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    remember_cached_raw_mirror_blob_record(cache, cached.clone());
    let entry_bytes = serde_json::to_vec(&cached)?;
    let bytes = serde_json::to_vec(&PersistedRawMirrorBlobCache {
        version: 1,
        entry: cached,
        entry_blake3: blake3::hash(&entry_bytes).to_hex().to_string(),
    })?;
    if bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES {
        return Ok(());
    }
    let path =
        raw_mirror_blob_cache_path(root, &key).ok_or_else(|| anyhow!("invalid cache path"))?;
    if raw_mirror_path_has_symlink_below_root(root, &path) {
        anyhow::bail!("unsafe cache path");
    }
    ensure_private_dir_descendant(
        root,
        path.parent()
            .ok_or_else(|| anyhow!("cache has no parent"))?,
    )?;
    let previous_size = match raw_mirror_symlink_metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error).context("inspect previous raw-mirror cache size"),
    };
    let temporary = staging.write(budget, "cache", &bytes)?;
    raw_mirror_io("publish_cache", &path, || fs::rename(temporary, &path))?;
    record_file_size_change(root, previous_size, bytes.len() as u64);
    sync_parent(&path)?;
    Ok(())
}

pub(crate) fn capture(
    input: RawMirrorCaptureInput<'_>,
    threshold: u64,
    chunk_size: usize,
    policy: RawMirrorRetentionPolicy,
) -> Result<RawMirrorCaptureRecord> {
    capture_with_prefix(input, threshold, chunk_size, policy, None)
}

pub(crate) fn capture_codex_prefix(
    input: RawMirrorCaptureInput<'_>,
    receipt: &crate::connectors::codex::StablePrefixReceipt,
    threshold: u64,
    chunk_size: usize,
    policy: RawMirrorRetentionPolicy,
) -> Result<RawMirrorCaptureRecord> {
    anyhow::ensure!(
        input.provider == receipt.provider.slug()
            && input.source_path == receipt.source_path.as_path(),
        "prefix capture does not match its admitted provider and source"
    );
    receipt.validate_source()?;
    capture_with_prefix(input, threshold, chunk_size, policy, Some(receipt))
}

fn capture_with_prefix(
    input: RawMirrorCaptureInput<'_>,
    threshold: u64,
    chunk_size: usize,
    policy: RawMirrorRetentionPolicy,
    prefix: Option<&crate::connectors::codex::StablePrefixReceipt>,
) -> Result<RawMirrorCaptureRecord> {
    if crate::raw_mirror::is_sensitive_container_provider(input.provider) {
        anyhow::bail!(
            "disabled_sensitive_container: {} source files cannot be raw-mirrored",
            input.provider
        );
    }
    if threshold == 0 {
        anyhow::bail!("raw mirror chunk threshold must be greater than zero");
    }
    if chunk_size == 0 {
        anyhow::bail!("raw mirror chunk size must be greater than zero");
    }
    if chunk_size > crate::raw_mirror::codec::MAX_DECODED_CHUNK_BYTES {
        anyhow::bail!("raw mirror chunk size exceeds the bounded codec policy");
    }
    let metadata = raw_mirror_symlink_metadata(input.source_path)
        .with_context(|| format!("stat raw mirror source {}", input.source_path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "refusing to raw-mirror symlink source {}",
            input.source_path.display()
        );
    }
    if !metadata.is_file() {
        anyhow::bail!(
            "refusing to raw-mirror non-file source {}",
            input.source_path.display()
        );
    }
    if crate::raw_mirror::zcode_source_is_sensitive(input.source_path)? {
        anyhow::bail!(
            "disabled_sensitive_container: ZCode database and store files cannot be raw-mirrored"
        );
    }
    let root = ensure_raw_mirror_root(input.data_dir)?;
    let _mutation = acquire_raw_mirror_mutation_lock(&root)?;
    // Ordinary admission is bound to the pre-lock source snapshot. A prefix
    // instead supplies its original object/content proof below, allowing only
    // appends outside the admitted bytes while the mutation lock was pending.
    let current = raw_mirror_symlink_metadata(input.source_path)?;
    if current.file_type().is_symlink()
        || !current.is_file()
        || (prefix.is_none() && source_file_changed_during_capture(&metadata, &current))
    {
        anyhow::bail!("raw mirror source changed identity while waiting for capture");
    }
    // The original name owns privacy, provenance and manifest identity.
    // Recheck prefix evidence after waiting for the mutation lock. Its reader
    // binds to this current object while the ordinary path keeps the original
    // pre-lock snapshot. No unbudgeted full-size temporary view is created.
    if let Some(receipt) = prefix {
        receipt.validate_source()?;
    }
    let reader_metadata = if prefix.is_some() { &current } else { &metadata };
    // Validate coordination even on a cache hit, but do not rewrite it until
    // this capture actually needs to publish something. Repeated watch scans
    // of unchanged sources must not create staging trees or mutation events.
    read_usage_generation(&root)?;
    let mut budget = Budget {
        root: &root,
        policy,
        used: 0,
        inventoried: false,
    };
    let key = raw_mirror_blob_cache_key(&input, &metadata, threshold, chunk_size);
    // Publishing B bytes while the source has grown to N must not associate
    // the B-byte blob with N's full-file fingerprint, even on a later process.
    let cached = if prefix.is_none() {
        cached_record(&root, &key)
    } else {
        None
    };
    let was_cached = cached.is_some();
    let mut staging = Staging::new(&root);
    let prepared = match cached {
        Some(record) => PreparedRawMirrorContent {
            record,
            files: Vec::new(),
        },
        None => prepare_bounded(
            &root,
            &input,
            CaptureSource {
                metadata: reader_metadata,
                prefix,
            },
            threshold,
            chunk_size,
            &mut staging,
            &mut budget,
        )?,
    };
    let record = prepared.record;
    if let Some(receipt) = prefix {
        anyhow::ensure!(
            record.source_size_bytes == receipt.committed_bytes
                && record.source_content_blake3 == receipt.blake3,
            "prefix capture bytes do not match the parsed source receipt"
        );
        // No content-addressed blob or manifest is visible yet. Reject a
        // replacement or in-place edit before the publication reservation.
        receipt.validate_source()?;
    }
    let path_hash = raw_mirror_original_path_blake3(&input.source_path.display().to_string());
    let id = raw_mirror_manifest_id(
        input.provider,
        input.source_id,
        input.origin_kind,
        input.origin_host,
        &path_hash,
        &record.blob_blake3,
    );
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
        provider: input.provider.to_string(),
        source_id: input.source_id.to_string(),
        origin_kind: input.origin_kind.to_string(),
        origin_host: input.origin_host.map(ToOwned::to_owned),
        original_path: input.source_path.display().to_string(),
        redacted_original_path: redacted_original_path(input.provider, input.source_path),
        original_path_blake3: path_hash,
        captured_at_ms: now,
        source_mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
        source_size_bytes: record.source_size_bytes,
        content_storage: record.content_storage.clone(),
        compression: RawMirrorCompressionEnvelope {
            state: if record
                .content_storage
                .as_ref()
                .is_some_and(|storage| storage.kind == RAW_MIRROR_FIXED_CHUNKS_V2_KIND)
            {
                "per_chunk"
            } else {
                "none"
            }
            .to_string(),
            algorithm: None,
            uncompressed_size_bytes: Some(record.source_size_bytes),
        },
        encryption: RawMirrorEncryptionEnvelope {
            state: "none".to_string(),
            algorithm: None,
            key_id: None,
            envelope_version: None,
        },
        db_links: unique_db_links(input.db_links),
        verification: RawMirrorVerificationRecord {
            status: "captured".to_string(),
            verifier: "cass_indexer".to_string(),
            content_blake3: Some(record.source_content_blake3.clone()),
            verified_at_ms: Some(now),
        },
        manifest_blake3: None,
    };
    manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    if bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES {
        anyhow::bail!("raw-mirror manifest exceeds its size limit");
    }
    let existing = match raw_mirror_symlink_metadata(&path) {
        Ok(_) => Some(read_validated_raw_mirror_manifest(&root, &id)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let needs_merge = existing.as_ref().is_some_and(|old| {
        input
            .db_links
            .iter()
            .any(|link| !old.db_links.contains(link))
    });
    // Reserve publication BEFORE making any staged blob visible. A budget
    // refusal therefore leaves neither a partial manifest nor new orphan blobs.
    if existing.is_none() {
        budget.reserve(bytes.len() as u64)?;
    } else if needs_merge {
        budget.reserve(
            raw_mirror_symlink_metadata(&path)?
                .len()
                .saturating_add(bytes.len() as u64),
        )?;
    }
    if existing.is_none() || needs_merge {
        staging.note_mutation()?;
    }
    let mut already_present = existing.is_some();
    for file in prepared.files {
        let target = root.join(
            raw_mirror_blob_relative_path(&file.blob_blake3)
                .ok_or_else(|| anyhow!("invalid staged digest"))?,
        );
        already_present &=
            publish_content_addressed_temp(&root, &file.temp_path, &target, &file.blob_blake3)?;
    }
    if let Some(old) = existing {
        if needs_merge {
            merge_raw_mirror_manifest_db_links(
                &root,
                &path,
                input.db_links,
                Some(&record.blob_blake3),
            )?;
        }
        manifest = old;
    } else {
        publish_manifest_bytes_create_new(&root, &path, &bytes, &record.blob_blake3)?;
    }
    if !was_cached && prefix.is_none() {
        // Cache admission is optional; successfully preserving the source must
        // not be reported as a failed capture merely because its cache cannot fit.
        if let Err(error) = save_cache(&root, key, record.clone(), &mut staging, &mut budget) {
            tracing::debug!(%error, "raw-mirror capture cache not persisted");
        }
    }
    Ok(RawMirrorCaptureRecord {
        manifest_id: id,
        manifest_relative_path: relative,
        blob_relative_path: manifest.blob_relative_path,
        blob_blake3: record.blob_blake3,
        blob_size_bytes: manifest.blob_size_bytes,
        source_content_blake3: record.source_content_blake3,
        source_size_bytes: record.source_size_bytes,
        storage_kind: record
            .content_storage
            .as_ref()
            .map_or("whole_blob_v1", |s| s.kind.as_str())
            .to_string(),
        chunk_count: record
            .content_storage
            .as_ref()
            .map_or(1, |s| s.chunks.len()),
        captured_at_ms: manifest.captured_at_ms,
        source_mtime_ms: manifest.source_mtime_ms,
        already_present,
    })
}

#[cfg(test)]
mod tests {
    include!("policy_tests.rs");
}

#[cfg(test)]
mod compression_tests {
    include!("capture_compression_tests.rs");
}

pub(crate) fn capture_default(
    input: RawMirrorCaptureInput<'_>,
    policy: RawMirrorRetentionPolicy,
) -> Result<RawMirrorCaptureRecord> {
    capture(
        input,
        RAW_MIRROR_CHUNK_THRESHOLD_BYTES,
        RAW_MIRROR_CHUNK_SIZE_BYTES,
        policy,
    )
}

mod retention {
    include!("retention.rs");
}
pub(crate) use retention::maintain_while_index_owned;
pub use retention::{
    RawMirrorAutomaticRetention, RawMirrorMaintenanceReport, maintain_before_index,
};
