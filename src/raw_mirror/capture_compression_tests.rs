use super::*;

const CHUNK: usize = 256 * 1024;

fn policy(max_bytes: u64) -> RawMirrorRetentionPolicy {
    RawMirrorRetentionPolicy {
        max_bytes,
        min_free_bytes: 0,
    }
}

fn input<'a>(data: &'a Path, source: &'a Path) -> RawMirrorCaptureInput<'a> {
    RawMirrorCaptureInput {
        data_dir: data,
        provider: "codex",
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path: source,
        db_links: &[],
    }
}

fn capture_chunks(data: &Path, source: &Path, max_bytes: u64) -> Result<RawMirrorCaptureRecord> {
    capture(input(data, source), CHUNK as u64, CHUNK, policy(max_bytes))
}

#[cfg(unix)]
#[derive(Clone)]
struct AppendCaptureTrace(Arc<Mutex<Vec<u8>>>);

#[cfg(unix)]
impl Write for AppendCaptureTrace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("capture trace lock poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn trace_capture_chunks(
    data: &Path,
    source: &Path,
) -> Result<(RawMirrorCaptureRecord, Vec<Value>)> {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = AppendCaptureTrace(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let record = tracing::subscriber::with_default(subscriber, || {
        capture_chunks(data, source, 8 * 1024 * 1024)
    })?;
    let log = String::from_utf8(
        bytes
            .lock()
            .map_err(|_| anyhow!("capture trace lock poisoned"))?
            .clone(),
    )?;
    let events = log
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((record, events))
}

#[cfg(unix)]
fn capture_trace_has_operation(events: &[Value], operation: &str, path: &Path) -> bool {
    events.iter().any(|event| {
        event["fields"]["operation"] == operation
            && event["fields"]["path"] == path.display().to_string()
    })
}

#[cfg(unix)]
fn clear_source_capture_memo(data: &Path, source: &Path) -> Result<RawMirrorBlobCacheKey> {
    let key = raw_mirror_blob_cache_key(
        &input(data, source),
        &fs::metadata(source)?,
        CHUNK as u64,
        CHUNK,
    );
    if let Some(cache) = BLOB_CAPTURE_CACHE.get() {
        cache
            .lock()
            .map_err(|_| anyhow!("capture memo lock poisoned"))?
            .remove(&raw_mirror_blob_source_key(&key));
    }
    Ok(key)
}

#[cfg(unix)]
#[test]
fn gh557_append_reuses_persisted_plain_and_compressed_chunk_evidence() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("append.jsonl");
    let mut original = vec![b'c'; CHUNK];
    let mut noise = vec![0_u8; CHUNK + 17];
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gh557 incompressible source chunk");
    hasher.finalize_xof().fill(&mut noise);
    original.extend(noise);
    fs::write(&source, &original)?;
    let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
    let root = raw_mirror_root(&data);
    let manifest = read_validated_raw_mirror_manifest(&root, &first.manifest_id)?;
    let before = manifest.content_storage.expect("chunk descriptor");
    assert_eq!(before.chunks[0].encoding.as_deref(), Some("zstd"));
    assert_eq!(before.chunks[1].encoding.as_deref(), Some("none"));
    clear_source_capture_memo(&data, &source)?;

    let tail = b"\nnew record after the previous partial tail\n";
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(tail)?;
    let mut expected = original.clone();
    expected.extend_from_slice(tail);
    let (second, events) = trace_capture_chunks(&data, &source)?;
    let manifest = read_validated_raw_mirror_manifest(&root, &second.manifest_id)?;
    let after = manifest.content_storage.expect("chunk descriptor");
    for (previous, current) in before.chunks.iter().zip(&after.chunks).take(2) {
        assert_eq!(previous, current);
        let blob = root.join(&previous.blob_relative_path);
        for operation in [
            "open_source",
            "open_blob",
            "hash_blob_read",
            "read_decoded_chunk",
        ] {
            assert!(
                !capture_trace_has_operation(&events, operation, &blob),
                "unchanged verified chunk was reread ({operation}): {}",
                blob.display()
            );
        }
    }
    assert!(capture_trace_has_operation(
        &events,
        "read_capture_cache",
        &{
            let key = raw_mirror_blob_cache_key(
                &input(&data, &source),
                &fs::metadata(&source)?,
                CHUNK as u64,
                CHUNK,
            );
            raw_mirror_blob_cache_path(&root, &key).expect("cache path")
        }
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event["fields"]["operation"] == "read_source_chunk"
                    && event["fields"]["path"] == source.display().to_string()
            })
            .count(),
        expected.len().div_ceil(CHUNK),
        "blob reuse must retain every source chunk read"
    );
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    assert_eq!(read_source_bytes(&data, &second.manifest_id)?, expected);
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh557_chunk_evidence_never_substitutes_for_rereading_changed_source_bytes() -> Result<()> {
    for mutation in ["prefix-append", "same-size", "truncate", "replace"] {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let source = temp.path().join("changing.jsonl");
        let original = vec![b'o'; 2 * CHUNK + 31];
        fs::write(&source, &original)?;
        let modified = fs::metadata(&source)?.modified()?;
        let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
        clear_source_capture_memo(&data, &source)?;
        let mut expected = original.clone();
        expected[1] = b'n';
        match mutation {
            "prefix-append" => expected.extend_from_slice(b"new tail"),
            "truncate" => expected.truncate(CHUNK + 17),
            "replace" => fs::rename(&source, source.with_extension("retained"))?,
            "same-size" => {}
            _ => unreachable!(),
        }
        fs::write(&source, &expected)?;
        File::open(&source)?.set_modified(modified)?;
        let next = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
        assert_ne!(
            next.source_content_blake3, first.source_content_blake3,
            "{mutation}"
        );
        assert_eq!(
            read_source_bytes(&data, &next.manifest_id)?,
            expected,
            "{mutation}"
        );
        assert_eq!(
            read_source_bytes(&data, &first.manifest_id)?,
            original,
            "{mutation}"
        );
        assert_eq!(fs::read(&source)?, expected, "{mutation}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh557_changed_or_symlinked_blob_cannot_inherit_verified_chunk_evidence() -> Result<()> {
    for mutation in ["rewrite", "replace", "symlink"] {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let source = temp.path().join("corruption.jsonl");
        let original = vec![b'c'; CHUNK];
        fs::write(&source, &original)?;
        let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
        let root = raw_mirror_root(&data);
        let manifest_path = root.join(&first.manifest_relative_path);
        let manifest_bytes = fs::read(&manifest_path)?;
        let manifest = read_validated_raw_mirror_manifest(&root, &first.manifest_id)?;
        let storage = manifest.content_storage.expect("chunk descriptor");
        let reference = &storage.chunks[0];
        let blob = root.join(&reference.blob_relative_path);
        let modified = fs::metadata(&blob)?.modified()?;
        clear_source_capture_memo(&data, &source)?;
        let retained = blob.with_extension("retained");
        if mutation != "rewrite" {
            fs::rename(&blob, &retained)?;
        }
        if mutation == "symlink" {
            std::os::unix::fs::symlink(&retained, &blob)?;
        } else {
            fs::write(
                &blob,
                vec![0_u8; usize::try_from(reference.blob_size_bytes)?],
            )?;
            File::open(&blob)?.set_modified(modified)?;
        }
        OpenOptions::new()
            .append(true)
            .open(&source)?
            .write_all(b"new tail")?;
        let error = capture_chunks(&data, &source, 8 * 1024 * 1024)
            .expect_err("a changed blob must be verified before it can be reused");
        assert!(
            error.to_string().contains("digest")
                || error.to_string().contains("checksum")
                || error.to_string().contains("symlink"),
            "{mutation}: {error:#}"
        );
        assert_eq!(
            fs::read_dir(root.join("manifests"))?.count(),
            1,
            "{mutation}"
        );
        assert_eq!(
            fs::read(&manifest_path)?,
            manifest_bytes,
            "a failed capture must not rewrite the existing manifest"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh557_missing_chunk_is_recreated_from_fully_read_source_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("missing.jsonl");
    let original = vec![b'm'; CHUNK];
    fs::write(&source, &original)?;
    let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
    let root = raw_mirror_root(&data);
    let manifest = read_validated_raw_mirror_manifest(&root, &first.manifest_id)?;
    let storage = manifest.content_storage.expect("chunk descriptor");
    let blob = root.join(&storage.chunks[0].blob_relative_path);
    clear_source_capture_memo(&data, &source)?;
    fs::rename(&blob, blob.with_extension("retained"))?;
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(b"new tail")?;
    let next = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
    assert!(
        blob.is_file(),
        "the missing recovery chunk must be republished"
    );
    let mut expected = original.clone();
    expected.extend_from_slice(b"new tail");
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    assert_eq!(read_source_bytes(&data, &next.manifest_id)?, expected);
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh557_invalid_or_foreign_advisory_cache_falls_back_to_blob_verification() -> Result<()> {
    for fault in ["checksum", "namespace", "weak-key", "descriptor"] {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let source = temp.path().join("cache.jsonl");
        let original = vec![b'v'; CHUNK];
        fs::write(&source, &original)?;
        let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
        let root = raw_mirror_root(&data);
        let manifest = read_validated_raw_mirror_manifest(&root, &first.manifest_id)?;
        let storage = manifest.content_storage.expect("chunk descriptor");
        let blob = root.join(&storage.chunks[0].blob_relative_path);
        let key = clear_source_capture_memo(&data, &source)?;
        let cache_path = raw_mirror_blob_cache_path(&root, &key).expect("cache path");
        let mut persisted: PersistedRawMirrorBlobCache =
            serde_json::from_slice(&fs::read(&cache_path)?)?;
        match fault {
            "checksum" => persisted.entry_blake3 = "0".repeat(64),
            "namespace" => {
                persisted.entry.cache_key.source_path = temp.path().join("foreign.jsonl")
            }
            "weak-key" => persisted.entry.cache_key.source_change_time_ns = None,
            "descriptor" => persisted.entry.record.source_size_bytes += 1,
            _ => unreachable!(),
        }
        if fault != "checksum" {
            persisted.entry_blake3 = blake3::hash(&serde_json::to_vec(&persisted.entry)?)
                .to_hex()
                .to_string();
        }
        fs::write(&cache_path, serde_json::to_vec(&persisted)?)?;
        OpenOptions::new()
            .append(true)
            .open(&source)?
            .write_all(b"new tail")?;
        let (next, events) = trace_capture_chunks(&data, &source)?;
        assert!(
            capture_trace_has_operation(&events, "hash_blob_read", &blob),
            "{fault}: invalid evidence must cause actual blob verification"
        );
        let mut expected = original.clone();
        expected.extend_from_slice(b"new tail");
        assert_eq!(
            read_source_bytes(&data, &next.manifest_id)?,
            expected,
            "{fault}"
        );
        assert_eq!(
            read_source_bytes(&data, &first.manifest_id)?,
            original,
            "{fault}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh557_checksum_valid_cache_cannot_relabel_compressed_chunk_contents() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("forged-mapping.jsonl");
    let original = vec![b'a'; CHUNK];
    fs::write(&source, &original)?;
    let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
    let root = raw_mirror_root(&data);
    let key = clear_source_capture_memo(&data, &source)?;
    let cache_path = raw_mirror_blob_cache_path(&root, &key).expect("cache path");
    let mut persisted: PersistedRawMirrorBlobCache =
        serde_json::from_slice(&fs::read(&cache_path)?)?;
    let mut expected = vec![b'b'; CHUNK];
    let new_content_hash = blake3::hash(&expected).to_hex().to_string();
    let storage = persisted
        .entry
        .record
        .content_storage
        .as_mut()
        .expect("chunk descriptor");
    assert_eq!(storage.chunks.len(), 1);
    assert_eq!(storage.chunks[0].encoding.as_deref(), Some("zstd"));
    // Forge every advisory checksum consistently while leaving the compressed
    // blob, its strong filesystem fingerprint and the real descriptor intact.
    storage.chunks[0].content_blake3 = Some(new_content_hash.clone());
    storage.content_blake3 = new_content_hash.clone();
    let descriptor_bytes = serde_json::to_vec(storage)?;
    persisted.entry.record.source_content_blake3 = new_content_hash;
    persisted.entry.record.blob_blake3 = blake3::hash(&descriptor_bytes).to_hex().to_string();
    persisted.entry.record.blob_size_bytes = descriptor_bytes.len() as u64;
    validate_raw_mirror_blob_record(&persisted.entry.record)?;
    let forged_descriptor = root.join(
        raw_mirror_blob_relative_path(&persisted.entry.record.blob_blake3)
            .expect("descriptor path"),
    );
    assert!(
        !forged_descriptor.exists(),
        "only the advisory cache was changed"
    );
    persisted.entry_blake3 = blake3::hash(&serde_json::to_vec(&persisted.entry)?)
        .to_hex()
        .to_string();
    fs::write(&cache_path, serde_json::to_vec(&persisted)?)?;
    expected.extend_from_slice(b"new tail");
    fs::write(&source, &expected)?;

    let next = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
    assert_eq!(read_source_bytes(&data, &next.manifest_id)?, expected);
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    assert_eq!(fs::read(&source)?, expected);
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh557_replaced_valid_chunk_reverifies_stored_and_decoded_content() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("replaced-valid.jsonl");
    let original = vec![b'r'; CHUNK];
    fs::write(&source, &original)?;
    let first = capture_chunks(&data, &source, 8 * 1024 * 1024)?;
    let root = raw_mirror_root(&data);
    let manifest = read_validated_raw_mirror_manifest(&root, &first.manifest_id)?;
    let storage = manifest.content_storage.expect("chunk descriptor");
    let blob = root.join(&storage.chunks[0].blob_relative_path);
    let modified = fs::metadata(&blob)?.modified()?;
    let stored = fs::read(&blob)?;
    clear_source_capture_memo(&data, &source)?;
    fs::rename(&blob, blob.with_extension("retained"))?;
    fs::write(&blob, &stored)?;
    File::open(&blob)?.set_modified(modified)?;
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(b"new tail")?;
    let (next, events) = trace_capture_chunks(&data, &source)?;
    assert!(capture_trace_has_operation(
        &events,
        "hash_blob_read",
        &blob
    ));
    assert!(capture_trace_has_operation(
        &events,
        "read_decoded_chunk",
        &blob
    ));
    let mut expected = original.clone();
    expected.extend_from_slice(b"new tail");
    assert_eq!(read_source_bytes(&data, &next.manifest_id)?, expected);
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    Ok(())
}

#[test]
fn gh562_compressed_capture_admission_counts_physical_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("large.jsonl");
    let mut original = Vec::new();
    for index in 0..12_000 {
        writeln!(
            original,
            "{{\"message\":\"preserve this repeated source evidence\",\"index\":{index}}}"
        )?;
    }
    fs::write(&source, &original)?;
    let cap = 128 * 1024;
    assert!(original.len() as u64 > cap);

    let record = capture_chunks(&data, &source, cap)?;
    let root = raw_mirror_root(&data);
    let manifest = read_validated_raw_mirror_manifest(&root, &record.manifest_id)?;
    let storage = manifest.content_storage.as_ref().expect("chunk descriptor");
    assert_eq!(storage.kind, RAW_MIRROR_FIXED_CHUNKS_V2_KIND);
    assert_eq!(manifest.compression.state, "per_chunk");
    assert!(manifest.compression.algorithm.is_none());
    assert_eq!(
        manifest.compression.uncompressed_size_bytes,
        Some(original.len() as u64)
    );
    assert!(
        storage
            .chunks
            .iter()
            .any(|chunk| chunk.encoding.as_deref() == Some("zstd"))
    );
    assert!(
        storage
            .chunks
            .iter()
            .all(|chunk| chunk.content_blake3.is_some() && chunk.content_size_bytes.is_some())
    );
    assert_eq!(read_source_bytes(&data, &record.manifest_id)?, original);
    assert_eq!(
        fs::read(&source)?,
        original,
        "capture must never rewrite the provider's source"
    );

    let verified = verify_source_capture(&data, &record.manifest_id)?;
    let physical = raw_mirror_manifest_blob_references(&manifest)?
        .into_iter()
        .map(|reference| (reference.blob_relative_path, reference.blob_size_bytes))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(verified.stored_bytes, physical.values().sum::<u64>());
    assert_eq!(verified.source_size_bytes, original.len() as u64);
    assert_eq!(
        verified.source_content_blake3,
        blake3::hash(&original).to_hex().to_string()
    );
    assert!(
        measured_usage(&root)? <= cap,
        "the physical cap includes manifests and caches too"
    );
    Ok(())
}

#[test]
fn gh562_repeated_compressed_chunks_share_blobs_across_appends() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("repeated.jsonl");
    let block = vec![b'q'; CHUNK];
    let original = block.repeat(3);
    fs::write(&source, &original)?;
    let first = capture_chunks(&data, &source, 1024 * 1024)?;
    let root = raw_mirror_root(&data);
    let first_manifest = read_validated_raw_mirror_manifest(&root, &first.manifest_id)?;
    let first_storage = first_manifest
        .content_storage
        .as_ref()
        .expect("chunk descriptor");
    assert_eq!(first_storage.chunks.len(), 3);
    assert!(
        first_storage
            .chunks
            .iter()
            .all(|chunk| chunk == &first_storage.chunks[0])
    );
    assert_eq!(first_storage.chunks[0].encoding.as_deref(), Some("zstd"));
    let before = storage_summary(&data);
    assert_eq!(
        verify_source_capture(&data, &first.manifest_id)?.stored_blob_count,
        2
    );

    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(&block)?;
    let second = capture_chunks(&data, &source, 1024 * 1024)?;
    let second_manifest = read_validated_raw_mirror_manifest(&root, &second.manifest_id)?;
    let second_storage = second_manifest
        .content_storage
        .as_ref()
        .expect("chunk descriptor");
    assert_eq!(second_storage.chunks.len(), 4);
    assert!(
        second_storage
            .chunks
            .iter()
            .all(|chunk| chunk == &first_storage.chunks[0])
    );
    assert_eq!(
        storage_summary(&data).unique_blob_count,
        before.unique_blob_count + 1,
        "an append of an existing chunk needs only its new descriptor"
    );
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    assert_eq!(
        read_source_bytes(&data, &second.manifest_id)?,
        block.repeat(4)
    );
    Ok(())
}

#[test]
fn gh562_incompressible_chunks_use_exact_plain_fallback() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("binary-source.db");
    let mut original = vec![0_u8; 2 * CHUNK + 17];
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"raw-mirror incompressible capture control");
    hasher.finalize_xof().fill(&mut original);
    assert!(
        crate::raw_mirror::codec::compress_chunk(&original[..CHUNK])?.len() >= CHUNK,
        "the control must actually exercise incompressible input"
    );
    fs::write(&source, &original)?;
    let record = capture_chunks(&data, &source, 4 * 1024 * 1024)?;
    let manifest =
        read_validated_raw_mirror_manifest(&raw_mirror_root(&data), &record.manifest_id)?;
    let storage = manifest.content_storage.expect("chunk descriptor");
    for chunk in &storage.chunks {
        assert_eq!(chunk.encoding.as_deref(), Some("none"));
        assert_eq!(chunk.content_size_bytes, Some(chunk.blob_size_bytes));
        assert_eq!(
            chunk.content_blake3.as_deref(),
            Some(chunk.blob_blake3.as_str())
        );
    }
    assert_eq!(read_source_bytes(&data, &record.manifest_id)?, original);
    assert_eq!(
        verify_source_capture(&data, &record.manifest_id)?.source_size_bytes,
        original.len() as u64
    );
    Ok(())
}

#[test]
fn gh562_append_reuses_legacy_plain_prefix_before_compressing_new_chunks() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("legacy.jsonl");
    let original = vec![b'l'; CHUNK];
    fs::write(&source, &original)?;
    let first = capture(
        input(&data, &source),
        (2 * CHUNK) as u64,
        CHUNK,
        policy(4 * 1024 * 1024),
    )?;
    assert_eq!(first.storage_kind, "whole_blob_v1");
    let root = raw_mirror_root(&data);
    let original_manifest = fs::read(root.join(&first.manifest_relative_path))?;
    let original_blob = fs::read(root.join(&first.blob_relative_path))?;
    let original_blob_modified = fs::metadata(root.join(&first.blob_relative_path))?.modified()?;

    let tail = vec![b'n'; CHUNK];
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(&tail)?;
    let next = capture(
        input(&data, &source),
        (2 * CHUNK) as u64,
        CHUNK,
        policy(4 * 1024 * 1024),
    )?;
    let manifest = read_validated_raw_mirror_manifest(&root, &next.manifest_id)?;
    let storage = manifest.content_storage.expect("mixed chunk descriptor");
    assert_eq!(storage.kind, RAW_MIRROR_FIXED_CHUNKS_V2_KIND);
    assert_eq!(storage.chunks[0].encoding.as_deref(), Some("none"));
    assert_eq!(
        storage.chunks[0].blob_relative_path,
        first.blob_relative_path
    );
    assert_eq!(storage.chunks[1].encoding.as_deref(), Some("zstd"));
    let redundant = crate::raw_mirror::codec::compress_chunk(&original)?;
    let redundant_path =
        raw_mirror_blob_relative_path(&blake3::hash(&redundant).to_hex()).expect("digest");
    assert!(
        !root.join(redundant_path).exists(),
        "a codec upgrade must not duplicate preserved prefix blobs"
    );
    assert_eq!(
        fs::read(root.join(&first.manifest_relative_path))?,
        original_manifest
    );
    assert_eq!(
        fs::read(root.join(&first.blob_relative_path))?,
        original_blob
    );
    assert_eq!(
        fs::metadata(root.join(&first.blob_relative_path))?.modified()?,
        original_blob_modified
    );
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    let mut expected = original;
    expected.extend_from_slice(&tail);
    assert_eq!(read_source_bytes(&data, &next.manifest_id)?, expected);
    Ok(())
}

#[test]
fn gh562_compressed_staging_refusal_publishes_no_recovery_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("refused.jsonl");
    let original = vec![b'r'; CHUNK];
    fs::write(&source, &original)?;
    let error = capture_chunks(&data, &source, 6 * 1024)
        .expect_err("the compressed payload can fit while its descriptor cannot");
    let paused = error
        .downcast_ref::<RawMirrorCapturePaused>()
        .expect("budget refusal");
    assert!(
        paused.used_bytes > 4096,
        "the compressed payload was staged before refusal"
    );
    assert_eq!(storage_summary(&data).unique_blob_count, 0);
    assert_eq!(storage_summary(&data).manifest_count, 0);
    assert_eq!(fs::read_dir(raw_mirror_root(&data).join("tmp"))?.count(), 0);
    assert_eq!(fs::read(source)?, original);
    Ok(())
}

#[test]
#[cfg(unix)]
fn gh562_legacy_chunked_persisted_cache_is_not_a_migration_trigger() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("cached-v1.jsonl");
    let original = vec![b'c'; CHUNK];
    fs::write(&source, &original)?;
    let whole = capture(
        input(&data, &source),
        (2 * CHUNK) as u64,
        CHUNK,
        policy(4 * 1024 * 1024),
    )?;
    let root = raw_mirror_root(&data);
    let key = raw_mirror_blob_cache_key(
        &input(&data, &source),
        &fs::metadata(&source)?,
        CHUNK as u64,
        CHUNK,
    );

    // Construct the exact pre-compression v1 descriptor/cache using the
    // existing plain content address. This is an upgrade fixture, not a
    // second implementation of the new compressor.
    let legacy_storage = RawMirrorContentStorage {
        kind: RAW_MIRROR_FIXED_CHUNKS_KIND.to_string(),
        content_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
        content_blake3: whole.source_content_blake3.clone(),
        content_size_bytes: whole.source_size_bytes,
        chunk_size_bytes: CHUNK as u64,
        chunks: vec![RawMirrorChunkRef {
            blob_relative_path: whole.blob_relative_path.clone(),
            blob_blake3: whole.blob_blake3.clone(),
            blob_size_bytes: whole.blob_size_bytes,
            encoding: None,
            content_blake3: None,
            content_size_bytes: None,
        }],
    };
    {
        let _mutation = acquire_raw_mirror_mutation_lock(&root)?;
        let mut staging = Staging::new(&root);
        let mut budget = Budget {
            root: &root,
            policy: policy(4 * 1024 * 1024),
            used: 0,
            inventoried: false,
        };
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        let descriptor = stage_blob(
            &root,
            &mut staging,
            &mut budget,
            &serde_json::to_vec(&legacy_storage)?,
            &mut seen,
            &mut files,
        )?;
        for file in files {
            let target =
                root.join(raw_mirror_blob_relative_path(&file.blob_blake3).expect("digest"));
            publish_content_addressed_temp(&root, &file.temp_path, &target, &file.blob_blake3)?;
        }
        cache_raw_mirror_blob_record(
            key.clone(),
            RawMirrorBlobRecord {
                blob_blake3: descriptor.blob_blake3,
                blob_size_bytes: descriptor.blob_size_bytes,
                source_content_blake3: whole.source_content_blake3,
                source_size_bytes: whole.source_size_bytes,
                content_storage: Some(legacy_storage),
            },
        );
    }
    let first = capture_chunks(&data, &source, 4 * 1024 * 1024)?;
    assert_eq!(first.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
    let cache_path = raw_mirror_blob_cache_path(&root, &key).expect("cache path");
    let cache_bytes = fs::read(&cache_path)?;
    let manifest_path = root.join(&first.manifest_relative_path);
    let manifest_bytes = fs::read(&manifest_path)?;
    let generation_bytes = fs::read(root.join(USAGE_GENERATION_FILE))?;
    BLOB_CAPTURE_CACHE
        .get()
        .expect("cache")
        .lock()
        .expect("cache lock")
        .remove(&raw_mirror_blob_source_key(&key));

    let repeated = capture_chunks(&data, &source, 0)?;
    assert!(repeated.already_present);
    assert_eq!(repeated.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
    assert_eq!(repeated.manifest_id, first.manifest_id);
    assert_eq!(fs::read(&cache_path)?, cache_bytes);
    assert_eq!(fs::read(&manifest_path)?, manifest_bytes);
    assert_eq!(
        fs::read(root.join(USAGE_GENERATION_FILE))?,
        generation_bytes
    );
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);

    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(b"changed")?;
    let error = capture_chunks(&data, &source, 0)
        .expect_err("a changed source must not inherit cached admission");
    assert!(error.is::<RawMirrorCapturePaused>(), "{error:#}");
    assert_eq!(fs::read(&cache_path)?, cache_bytes);
    assert_eq!(fs::read(&manifest_path)?, manifest_bytes);
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    Ok(())
}

#[test]
fn gh562_oversized_chunk_policy_is_refused_before_capture_mutates_disk() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("oversized-policy.jsonl");
    fs::write(&source, b"keep the source unchanged\n")?;
    let error = capture(
        input(&data, &source),
        1,
        crate::raw_mirror::codec::MAX_DECODED_CHUNK_BYTES + 1,
        policy(u64::MAX),
    )
    .expect_err("the decoder bound must also constrain new captures");
    assert!(
        error.to_string().contains("bounded codec policy"),
        "{error:#}"
    );
    assert!(!data.exists());
    assert_eq!(fs::read(source)?, b"keep the source unchanged\n");
    Ok(())
}
