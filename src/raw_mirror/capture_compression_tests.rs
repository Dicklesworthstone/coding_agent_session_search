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

#[test]
fn gh562_compressed_capture_admission_counts_physical_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("large.jsonl");
    let mut original = Vec::new();
    for index in 0..12_000 {
        writeln!(original, "{{\"message\":\"preserve this repeated source evidence\",\"index\":{index}}}")?;
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
    assert_eq!(manifest.compression.uncompressed_size_bytes, Some(original.len() as u64));
    assert!(storage.chunks.iter().any(|chunk| chunk.encoding.as_deref() == Some("zstd")));
    assert!(storage.chunks.iter().all(|chunk| chunk.content_blake3.is_some() && chunk.content_size_bytes.is_some()));
    assert_eq!(read_source_bytes(&data, &record.manifest_id)?, original);
    assert_eq!(fs::read(&source)?, original, "capture must never rewrite the provider's source");

    let verified = verify_source_capture(&data, &record.manifest_id)?;
    let physical = raw_mirror_manifest_blob_references(&manifest)?
        .into_iter()
        .map(|reference| (reference.blob_relative_path, reference.blob_size_bytes))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(verified.stored_bytes, physical.values().sum::<u64>());
    assert_eq!(verified.source_size_bytes, original.len() as u64);
    assert_eq!(verified.source_content_blake3, blake3::hash(&original).to_hex().to_string());
    assert!(measured_usage(&root)? <= cap, "the physical cap includes manifests and caches too");
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
    let first_storage = first_manifest.content_storage.as_ref().expect("chunk descriptor");
    assert_eq!(first_storage.chunks.len(), 3);
    assert!(first_storage.chunks.iter().all(|chunk| chunk == &first_storage.chunks[0]));
    assert_eq!(first_storage.chunks[0].encoding.as_deref(), Some("zstd"));
    let before = storage_summary(&data);
    assert_eq!(verify_source_capture(&data, &first.manifest_id)?.stored_blob_count, 2);

    OpenOptions::new().append(true).open(&source)?.write_all(&block)?;
    let second = capture_chunks(&data, &source, 1024 * 1024)?;
    let second_manifest = read_validated_raw_mirror_manifest(&root, &second.manifest_id)?;
    let second_storage = second_manifest.content_storage.as_ref().expect("chunk descriptor");
    assert_eq!(second_storage.chunks.len(), 4);
    assert!(second_storage.chunks.iter().all(|chunk| chunk == &first_storage.chunks[0]));
    assert_eq!(storage_summary(&data).unique_blob_count, before.unique_blob_count + 1,
        "an append of an existing chunk needs only its new descriptor");
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);
    assert_eq!(read_source_bytes(&data, &second.manifest_id)?, block.repeat(4));
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
    assert!(crate::raw_mirror::codec::compress_chunk(&original[..CHUNK])?.len() >= CHUNK,
        "the control must actually exercise incompressible input");
    fs::write(&source, &original)?;
    let record = capture_chunks(&data, &source, 4 * 1024 * 1024)?;
    let manifest = read_validated_raw_mirror_manifest(&raw_mirror_root(&data), &record.manifest_id)?;
    let storage = manifest.content_storage.expect("chunk descriptor");
    for chunk in &storage.chunks {
        assert_eq!(chunk.encoding.as_deref(), Some("none"));
        assert_eq!(chunk.content_size_bytes, Some(chunk.blob_size_bytes));
        assert_eq!(chunk.content_blake3.as_deref(), Some(chunk.blob_blake3.as_str()));
    }
    assert_eq!(read_source_bytes(&data, &record.manifest_id)?, original);
    assert_eq!(verify_source_capture(&data, &record.manifest_id)?.source_size_bytes, original.len() as u64);
    Ok(())
}

#[test]
fn gh562_append_reuses_legacy_plain_prefix_before_compressing_new_chunks() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("legacy.jsonl");
    let original = vec![b'l'; CHUNK];
    fs::write(&source, &original)?;
    let first = capture(input(&data, &source), (2 * CHUNK) as u64, CHUNK, policy(4 * 1024 * 1024))?;
    assert_eq!(first.storage_kind, "whole_blob_v1");
    let root = raw_mirror_root(&data);
    let original_manifest = fs::read(root.join(&first.manifest_relative_path))?;
    let original_blob = fs::read(root.join(&first.blob_relative_path))?;
    let original_blob_modified = fs::metadata(root.join(&first.blob_relative_path))?.modified()?;

    let tail = vec![b'n'; CHUNK];
    OpenOptions::new().append(true).open(&source)?.write_all(&tail)?;
    let next = capture(input(&data, &source), (2 * CHUNK) as u64, CHUNK, policy(4 * 1024 * 1024))?;
    let manifest = read_validated_raw_mirror_manifest(&root, &next.manifest_id)?;
    let storage = manifest.content_storage.expect("mixed chunk descriptor");
    assert_eq!(storage.kind, RAW_MIRROR_FIXED_CHUNKS_V2_KIND);
    assert_eq!(storage.chunks[0].encoding.as_deref(), Some("none"));
    assert_eq!(storage.chunks[0].blob_relative_path, first.blob_relative_path);
    assert_eq!(storage.chunks[1].encoding.as_deref(), Some("zstd"));
    let redundant = crate::raw_mirror::codec::compress_chunk(&original)?;
    let redundant_path = raw_mirror_blob_relative_path(&blake3::hash(&redundant).to_hex()).expect("digest");
    assert!(!root.join(redundant_path).exists(), "a codec upgrade must not duplicate preserved prefix blobs");
    assert_eq!(fs::read(root.join(&first.manifest_relative_path))?, original_manifest);
    assert_eq!(fs::read(root.join(&first.blob_relative_path))?, original_blob);
    assert_eq!(fs::metadata(root.join(&first.blob_relative_path))?.modified()?, original_blob_modified);
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
    let paused = error.downcast_ref::<RawMirrorCapturePaused>().expect("budget refusal");
    assert!(paused.used_bytes > 4096, "the compressed payload was staged before refusal");
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
    let whole = capture(input(&data, &source), (2 * CHUNK) as u64, CHUNK, policy(4 * 1024 * 1024))?;
    let root = raw_mirror_root(&data);
    let key = raw_mirror_blob_cache_key(&input(&data, &source), &fs::metadata(&source)?, CHUNK as u64, CHUNK);

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
        let mut budget = Budget { root: &root, policy: policy(4 * 1024 * 1024), used: 0, inventoried: false };
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        let descriptor = stage_blob(&root, &mut staging, &mut budget,
            &serde_json::to_vec(&legacy_storage)?, &mut seen, &mut files)?;
        for file in files {
            let target = root.join(raw_mirror_blob_relative_path(&file.blob_blake3).expect("digest"));
            publish_content_addressed_temp(&root, &file.temp_path, &target, &file.blob_blake3)?;
        }
        cache_raw_mirror_blob_record(key.clone(), RawMirrorBlobRecord {
            blob_blake3: descriptor.blob_blake3,
            blob_size_bytes: descriptor.blob_size_bytes,
            source_content_blake3: whole.source_content_blake3,
            source_size_bytes: whole.source_size_bytes,
            content_storage: Some(legacy_storage),
        });
    }
    let first = capture_chunks(&data, &source, 4 * 1024 * 1024)?;
    assert_eq!(first.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
    let cache_path = raw_mirror_blob_cache_path(&root, &key).expect("cache path");
    let cache_bytes = fs::read(&cache_path)?;
    let manifest_path = root.join(&first.manifest_relative_path);
    let manifest_bytes = fs::read(&manifest_path)?;
    let generation_bytes = fs::read(root.join(USAGE_GENERATION_FILE))?;
    BLOB_CAPTURE_CACHE.get().expect("cache").lock().expect("cache lock")
        .remove(&raw_mirror_blob_source_key(&key));

    let repeated = capture_chunks(&data, &source, 0)?;
    assert!(repeated.already_present);
    assert_eq!(repeated.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
    assert_eq!(repeated.manifest_id, first.manifest_id);
    assert_eq!(fs::read(&cache_path)?, cache_bytes);
    assert_eq!(fs::read(&manifest_path)?, manifest_bytes);
    assert_eq!(fs::read(root.join(USAGE_GENERATION_FILE))?, generation_bytes);
    assert_eq!(read_source_bytes(&data, &first.manifest_id)?, original);

    OpenOptions::new().append(true).open(&source)?.write_all(b"changed")?;
    let error = capture_chunks(&data, &source, 0).expect_err("a changed source must not inherit cached admission");
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
    let error = capture(input(&data, &source), 1,
        crate::raw_mirror::codec::MAX_DECODED_CHUNK_BYTES + 1, policy(u64::MAX))
        .expect_err("the decoder bound must also constrain new captures");
    assert!(error.to_string().contains("bounded codec policy"), "{error:#}");
    assert!(!data.exists());
    assert_eq!(fs::read(source)?, b"keep the source unchanged\n");
    Ok(())
}
