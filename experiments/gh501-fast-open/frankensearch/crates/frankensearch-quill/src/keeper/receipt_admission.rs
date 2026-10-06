//! Read-only, explicit local-receipt admission. The ordinary KeeperSnapshot,
//! writer, recovery and maintenance entry points are intentionally untouched.

use super::*;
use crate::read_open_receipts::{Binding, Identity, Policy, Receipt, ReceiptBook, supported};
use rayon::prelude::*;

struct ReceiptCheckedSegment {
    segment: RecoveredSegment,
    receipt: Option<Receipt>,
    reused: bool,
}

struct CheckedWitness {
    witness: AuthenticatedFileWitness,
    receipt: Option<Receipt>,
    guard: Option<(File, Identity)>,
    reused: bool,
}

impl KeeperSnapshot {
    /// Open immutable, published segments with optional local identity receipts.
    ///
    /// Only the file-prefix hash can be reused. Container/MANIFEST validation
    /// and ordinary lazy section checks still run. The directory must already
    /// be private (0700) and owned by the current user. Ineligible filesystems,
    /// missing/corrupt proof state and unavailable cache storage verify in full.
    ///
    /// This explicitly trades a fresh full-file integrity scan for trusted
    /// local filesystem change metadata. It does not detect metadata-preserving
    /// storage faults, and it is not appropriate for untrusted index owners.
    /// Use `open` for strict admission. Neither this method nor `open` permits
    /// concurrent mutation of a published memory-mapped segment.
    ///
    /// # Errors
    /// Returns the ordinary admission errors, or a corruption error when the
    /// mapped descriptor changes identity during admission.
    pub fn open_with_local_receipts(
        directory: impl AsRef<Path>,
        schema: SchemaDescriptor,
        cache_directory: impl AsRef<Path>,
    ) -> Result<Self, KeeperError> {
        let open = || Self::open_local_receipts_once(
            directory.as_ref(), schema, cache_directory.as_ref(), Policy::default(),
        );
        match open() {
            Err(error) if recovery_retryable(&error) => open(),
            result => result,
        }
    }

    pub(super) fn open_local_receipts_once(
        directory: &Path,
        schema: SchemaDescriptor,
        cache_directory: &Path,
        policy: Policy,
    ) -> Result<Self, KeeperError> {
        // An advisory cache must never mutate the published index directory.
        // Missing/unresolvable cache storage is a strict-open fallback as well.
        match (directory.canonicalize(), cache_directory.canonicalize()) {
            (Ok(index), Ok(cache)) if !cache.starts_with(index) => {}
            _ => return Self::open(directory, schema),
        }
        let schema_id = schema.schema_id()
            .map_err(|source| KeeperError::InvalidSchema { source })?;
        let loaded = load_manifest_pair(directory)?;
        validate_loaded_schema(directory, schema_id, &loaded)?;
        validate_recovery_claims(directory, &loaded)?;
        let mut book = ReceiptBook::load(cache_directory, policy);
        let open = |manifest: &ManifestSegment| {
            open_receipt_segment(directory, schema, schema_id, manifest, &book)
        };
        let records = &loaded.manifest.segments;
        // Preserve MANIFEST ordering for deterministic error selection. The
        // private bounded pool never depends on the caller's global rayon pool.
        let opened: Vec<Result<ReceiptCheckedSegment, KeeperError>> = receipt_pool()
            .map_or_else(|| records.iter().map(&open).collect(),
                |pool| pool.install(|| records.par_iter().map(&open).collect()));
        let mut segments = Vec::with_capacity(opened.len());
        let mut receipt_hits = 0_usize;
        for result in opened {
            let checked = result?;
            receipt_hits += usize::from(checked.reused);
            if let Some(receipt) = checked.receipt { book.keep(receipt); }
            segments.push(checked.segment);
        }
        let full_verifications = segments.len() - receipt_hits;
        // No new proof becomes durable until complete snapshot admission,
        // including from_parts' cross-segment invariants, has succeeded.
        let snapshot = Self::from_parts(Some(directory.to_path_buf()), schema, loaded, segments)?;
        book.persist();
        tracing::debug!(receipt_hits, full_verifications,
            "Quill read-only local-receipt admission completed");
        Ok(snapshot)
    }
}

fn receipt_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(1, usize::from).min(8))
        .thread_name(|index| format!("quill-receipt-open-{index}"))
        .build().ok()).as_ref()
}

fn changed_file(path: &Path) -> KeeperError {
    KeeperError::SegmentOpen {
        path: path.to_path_buf(),
        source: QuillError::IndexCorrupted {
            path: path.to_path_buf(),
            detail: "mapped segment identity changed during read-only admission".to_owned(),
        },
    }
}

fn open_receipt_segment(
    directory: &Path, schema: SchemaDescriptor, schema_id: u64,
    manifest: &ManifestSegment, book: &ReceiptBook,
) -> Result<ReceiptCheckedSegment, KeeperError> {
    let path = directory.join(canonical_segment_name(manifest.segment_id));
    let (reader, checked) = SegmentReader::open_published_checked(
        &path, schema, crate::segment::SegmentLimits::default(),
        |reader, file| Ok(check_receipt_witness(&path, schema_id, manifest, reader, file, book)),
    ).map_err(|source| KeeperError::SegmentOpen { path: path.clone(), source })?;
    let checked = checked?;
    let segment = RecoveredSegment::bind(
        path.clone(), manifest.clone(), reader, schema, checked.witness,
    )?;
    // Recheck after bind too. Checking only before mapping misses mutation
    // during parsing/binding. This descriptor refers to the mapping's inode.
    if let Some((file, before)) = checked.guard
        && Identity::of_file(&file) != Some(before) {
        return Err(changed_file(&path));
    }
    Ok(ReceiptCheckedSegment { segment, receipt: checked.receipt, reused: checked.reused })
}

fn check_receipt_witness(
    path: &Path, schema_id: u64, manifest: &ManifestSegment,
    reader: &SegmentReader<ReadOnlyMappedFile>, file: &mut File, book: &ReceiptBook,
) -> Result<CheckedWitness, KeeperError> {
    let before = (book.enabled() && supported(file)).then(|| Identity::of_file(file)).flatten();
    let Some(before) = before else {
        return authenticate_segment_witness(path, manifest, reader, file)
            .map(|witness| CheckedWitness { witness, receipt: None, guard: None, reused: false });
    };
    let binding = Binding {
        schema_id, segment_id: manifest.segment_id,
        file_len: manifest.file_len, file_xxh3: manifest.file_xxh3,
    };
    let existing = book.admitted(binding, before);
    let reused = existing.is_some();
    let witness = if reused {
        let file_xxh3 = reader.file_xxh3();
        validate_segment_witnesses(path, manifest, reader, || Ok(file_xxh3))?;
        // This witness covers ONLY the prefix hash. Do not seed or bypass
        // SegmentReader's lazy per-section checksum cache.
        AuthenticatedFileWitness {
            file_xxh3,
            #[cfg(test)]
            full_prefix_hash_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    } else {
        authenticate_segment_witness(path, manifest, reader, file)?
    };
    let after = Identity::of_file(file);
    if after != Some(before) { return Err(changed_file(path)); }
    let guard = file.try_clone().map_err(|source| KeeperError::SegmentOpen {
        path: path.to_path_buf(), source: QuillError::Io(source),
    })?;
    let receipt = existing.or_else(|| book.verified(binding, before, after));
    Ok(CheckedWitness { witness, receipt, guard: Some((guard, before)), reused })
}
