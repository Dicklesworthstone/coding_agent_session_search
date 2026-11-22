use super::*;

fn codec_fixture_blob(root: &Path, bytes: &[u8]) -> Result<RawMirrorChunkRef> {
    let digest = blake3::hash(bytes).to_hex().to_string();
    let relative = raw_mirror_blob_relative_path(&digest).expect("fixture digest");
    let path = root.join(&relative);
    fs::create_dir_all(path.parent().expect("fixture blob parent"))?;
    fs::write(&path, bytes)?;
    Ok(RawMirrorChunkRef {
        blob_relative_path: relative,
        blob_blake3: digest,
        blob_size_bytes: bytes.len() as u64,
        encoding: None,
        content_blake3: None,
        content_size_bytes: None,
    })
}

fn codec_fixture_storage(
    root: &Path,
    source: &[u8],
    chunk_size: usize,
    encodings: &[Option<&str>],
) -> Result<RawMirrorContentStorage> {
    let mut chunks = Vec::new();
    for (content, encoding) in source.chunks(chunk_size).zip(encodings) {
        let stored = if *encoding == Some("zstd") {
            crate::raw_mirror::codec::compress_chunk(content)?
        } else {
            content.to_vec()
        };
        let mut chunk = codec_fixture_blob(root, &stored)?;
        if let Some(encoding) = encoding {
            chunk.encoding = Some((*encoding).to_string());
            chunk.content_blake3 = Some(blake3::hash(content).to_hex().to_string());
            chunk.content_size_bytes = Some(content.len() as u64);
        }
        chunks.push(chunk);
    }
    assert_eq!(chunks.len(), source.len().div_ceil(chunk_size));
    Ok(RawMirrorContentStorage {
        kind: if encodings.iter().any(Option::is_some) {
            RAW_MIRROR_FIXED_CHUNKS_V2_KIND.to_string()
        } else {
            RAW_MIRROR_FIXED_CHUNKS_KIND.to_string()
        },
        content_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
        content_blake3: blake3::hash(source).to_hex().to_string(),
        content_size_bytes: source.len() as u64,
        chunk_size_bytes: chunk_size as u64,
        chunks,
    })
}

fn codec_fixture_manifest(
    data: &Path,
    source: &[u8],
    storage: Option<RawMirrorContentStorage>,
) -> Result<RawMirrorManifestFile> {
    let root = raw_mirror_root(data);
    let blob = if let Some(storage) = &storage {
        codec_fixture_blob(&root, &serde_json::to_vec(storage)?)?
    } else {
        codec_fixture_blob(&root, source)?
    };
    let original_path = data.join("missing-source.jsonl").display().to_string();
    let original_path_blake3 = raw_mirror_original_path_blake3(&original_path);
    let version_two = storage
        .as_ref()
        .is_some_and(|storage| storage.kind == RAW_MIRROR_FIXED_CHUNKS_V2_KIND);
    let mut manifest = RawMirrorManifestFile {
        schema_version: RAW_MIRROR_SCHEMA_VERSION,
        manifest_kind: RAW_MIRROR_MANIFEST_KIND.to_string(),
        manifest_id: raw_mirror_manifest_id(
            "codex",
            "local",
            "local",
            None,
            &original_path_blake3,
            &blob.blob_blake3,
        ),
        blob_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
        blob_relative_path: blob.blob_relative_path,
        blob_blake3: blob.blob_blake3,
        blob_size_bytes: blob.blob_size_bytes,
        provider: "codex".to_string(),
        source_id: "local".to_string(),
        origin_kind: "local".to_string(),
        origin_host: None,
        original_path,
        redacted_original_path: "[codex]/missing-source.jsonl".to_string(),
        original_path_blake3,
        captured_at_ms: 0,
        source_mtime_ms: None,
        source_size_bytes: source.len() as u64,
        content_storage: storage,
        compression: RawMirrorCompressionEnvelope {
            state: if version_two { "per_chunk" } else { "none" }.to_string(),
            algorithm: None,
            uncompressed_size_bytes: Some(source.len() as u64),
        },
        encryption: RawMirrorEncryptionEnvelope {
            state: "none".to_string(),
            algorithm: None,
            key_id: None,
            envelope_version: None,
        },
        db_links: Vec::new(),
        verification: RawMirrorVerificationRecord {
            status: "captured".to_string(),
            verifier: "codec-fixture".to_string(),
            content_blake3: Some(blake3::hash(source).to_hex().to_string()),
            verified_at_ms: Some(0),
        },
        manifest_blake3: None,
    };
    manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
    let path = root.join(raw_mirror_manifest_relative_path(&manifest.manifest_id));
    fs::create_dir_all(path.parent().expect("fixture manifest parent"))?;
    fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(manifest)
}

fn codec_fixture_cache_record(manifest: &RawMirrorManifestFile) -> RawMirrorBlobRecord {
    RawMirrorBlobRecord {
        blob_blake3: manifest.blob_blake3.clone(),
        blob_size_bytes: manifest.blob_size_bytes,
        source_content_blake3: manifest.verification.content_blake3.clone().unwrap(),
        source_size_bytes: manifest.source_size_bytes,
        content_storage: manifest.content_storage.clone(),
    }
}

#[test]
fn gh562_legacy_chunk_descriptor_serialization_and_readback_are_unchanged() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let source = b"abcdefgh";
    let storage = codec_fixture_storage(&root, source, 4, &[None, None])?;
    let chunks = &storage.chunks;
    // Explicit legacy shape protects historical content-addressed descriptor
    // identities from new optional fields, including serialized null values.
    let legacy = format!(
        "{{\"kind\":\"fixed_chunks_v1\",\"content_hash_algorithm\":\"blake3\",\"content_blake3\":\"{}\",\"content_size_bytes\":8,\"chunk_size_bytes\":4,\"chunks\":[{{\"blob_relative_path\":\"{}\",\"blob_blake3\":\"{}\",\"blob_size_bytes\":4}},{{\"blob_relative_path\":\"{}\",\"blob_blake3\":\"{}\",\"blob_size_bytes\":4}}]}}",
        storage.content_blake3,
        chunks[0].blob_relative_path,
        chunks[0].blob_blake3,
        chunks[1].blob_relative_path,
        chunks[1].blob_blake3,
    );
    assert_eq!(serde_json::to_vec(&storage)?, legacy.as_bytes());
    let manifest = codec_fixture_manifest(&data, source, Some(storage))?;
    let before = fs::read(root.join(&manifest.blob_relative_path))?;
    assert_eq!(read_source_bytes(&data, &manifest.manifest_id)?, source);
    let verified = verify_source_capture(&data, &manifest.manifest_id)?;
    assert_eq!(verified.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
    assert_eq!(verified.source_size_bytes, source.len() as u64);
    assert_eq!(fs::read(root.join(&manifest.blob_relative_path))?, before);
    cached_raw_mirror_blob_record_is_verified(&root, &codec_fixture_cache_record(&manifest))?;
    Ok(())
}

#[test]
fn gh562_legacy_whole_blobs_use_file_backed_seekable_readers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = b"legacy source with no chunk descriptor";
    let manifest = codec_fixture_manifest(&data, source, None)?;
    let references = raw_mirror_manifest_blob_references(&manifest)?;
    let mut reader = open_raw_mirror_chunk_reader(&raw_mirror_root(&data), &references[0])?;
    assert!(matches!(&reader, RawMirrorChunkReader::Plain(_)));
    reader.seek(SeekFrom::Start(7))?;
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail)?;
    assert_eq!(tail, source[7..]);
    assert_eq!(read_source_bytes(&data, &manifest.manifest_id)?, source);
    assert_eq!(verify_source_capture(&data, &manifest.manifest_id)?.stored_blob_count, 1);
    Ok(())
}

#[test]
fn gh562_mixed_encoded_chunks_verify_logical_content_and_unique_physical_storage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let repeated = vec![b'x'; 4096];
    let plain: Vec<u8> = (0_u8..=255).cycle().take(4096).collect();
    let source = [repeated.as_slice(), plain.as_slice(), repeated.as_slice()].concat();
    let storage = codec_fixture_storage(
        &root,
        &source,
        4096,
        &[Some("zstd"), Some("none"), Some("zstd")],
    )?;
    assert_eq!(storage.chunks[0], storage.chunks[2]);
    assert!(storage.chunks[0].blob_size_bytes < 4096);
    let physical_bytes = storage.chunks[0].blob_size_bytes + storage.chunks[1].blob_size_bytes;
    let mut reader = open_raw_mirror_chunk_reader(&root, &storage.chunks[0])?;
    assert!(matches!(&reader, RawMirrorChunkReader::Decoded(_)));
    reader.seek(SeekFrom::Start(4080))?;
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail)?;
    assert_eq!(tail, repeated[4080..]);

    let manifest = codec_fixture_manifest(&data, &source, Some(storage))?;
    assert_eq!(read_source_bytes(&data, &manifest.manifest_id)?, source);
    let verified = verify_source_capture(&data, &manifest.manifest_id)?;
    assert_eq!(verified.storage_kind, RAW_MIRROR_FIXED_CHUNKS_V2_KIND);
    assert_eq!(verified.chunk_count, 3);
    assert_eq!(verified.stored_blob_count, 3);
    assert_eq!(verified.stored_bytes, manifest.blob_size_bytes + physical_bytes);
    assert_eq!(verified.source_size_bytes, source.len() as u64);
    cached_raw_mirror_blob_record_is_verified(&root, &codec_fixture_cache_record(&manifest))?;
    Ok(())
}

#[test]
fn gh562_valid_stored_hash_cannot_hide_wrong_decoded_content_from_read_cache_or_prune() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let source = vec![b'x'; 4096];
    let mut storage = codec_fixture_storage(&root, &source, 4096, &[Some("zstd")])?;
    storage.chunks[0].content_blake3 = Some(blake3::hash(b"wrong").to_hex().to_string());
    let manifest = codec_fixture_manifest(&data, &source, Some(storage))?;
    let references = raw_mirror_manifest_blob_references(&manifest)?;
    for reference in &references {
        verify_existing_blob_reference(&root, reference)?;
    }
    let error = read_source_bytes(&data, &manifest.manifest_id).unwrap_err();
    assert!(error.to_string().contains("content verification"), "{error:#}");
    assert!(verify_source_capture(&data, &manifest.manifest_id).is_err());
    assert!(cached_raw_mirror_blob_record_is_verified(
        &root,
        &codec_fixture_cache_record(&manifest),
    )
    .is_err());
    assert!(prune(
        &data,
        RawMirrorPruneOptions {
            older_than_ms: Some(0),
            safety_hold_down_ms: 0,
            apply: true,
            ..RawMirrorPruneOptions::default()
        },
    )
    .is_err());
    assert!(root.join(raw_mirror_manifest_relative_path(&manifest.manifest_id)).is_file());
    for reference in references {
        assert!(root.join(reference.blob_relative_path).is_file());
    }
    Ok(())
}

#[test]
fn gh562_corrupt_physical_bytes_fail_readback_even_with_valid_manifest_metadata() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let source = vec![b'x'; 4096];
    let storage = codec_fixture_storage(&root, &source, 4096, &[Some("zstd")])?;
    let path = root.join(&storage.chunks[0].blob_relative_path);
    let manifest = codec_fixture_manifest(&data, &source, Some(storage))?;
    let mut bytes = fs::read(&path)?;
    bytes[0] ^= 1;
    fs::write(&path, bytes)?;
    assert!(read_source_bytes(&data, &manifest.manifest_id).is_err());
    assert!(verify_source_capture(&data, &manifest.manifest_id).is_err());
    assert!(cached_raw_mirror_blob_record_is_verified(
        &root,
        &codec_fixture_cache_record(&manifest),
    )
    .is_err());
    Ok(())
}

#[test]
fn gh562_valid_stored_hash_cannot_hide_trailing_zstd_data() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let source = vec![b'x'; 4096];
    let mut storage = codec_fixture_storage(&root, &source, 4096, &[Some("zstd")])?;
    let mut encoded = crate::raw_mirror::codec::compress_chunk(&source)?;
    encoded.extend_from_slice(b"trailing data");
    let stored = codec_fixture_blob(&root, &encoded)?;
    storage.chunks[0].blob_relative_path = stored.blob_relative_path;
    storage.chunks[0].blob_blake3 = stored.blob_blake3;
    storage.chunks[0].blob_size_bytes = stored.blob_size_bytes;
    let manifest = codec_fixture_manifest(&data, &source, Some(storage))?;
    for reference in raw_mirror_manifest_blob_references(&manifest)? {
        verify_existing_blob_reference(&root, &reference)?;
    }
    assert!(read_source_bytes(&data, &manifest.manifest_id).is_err());
    assert!(verify_source_capture(&data, &manifest.manifest_id).is_err());
    Ok(())
}

#[test]
fn gh562_chunk_metadata_refuses_unknown_partial_and_unbounded_encodings() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let source = vec![b'x'; 4096];
    let storage = codec_fixture_storage(&root, &source, 4096, &[Some("zstd")])?;
    let mut cases = Vec::new();
    let mut unknown = storage.clone();
    unknown.chunks[0].encoding = Some("future-codec".to_string());
    cases.push(unknown);
    let mut partial = storage.clone();
    partial.chunks[0].content_blake3 = None;
    cases.push(partial);
    let mut decoded_too_large = storage.clone();
    decoded_too_large.chunks[0].content_size_bytes =
        Some(crate::raw_mirror::codec::MAX_DECODED_CHUNK_BYTES as u64 + 1);
    cases.push(decoded_too_large);
    let mut encoded_too_large = storage.clone();
    encoded_too_large.chunks[0].blob_size_bytes =
        crate::raw_mirror::codec::MAX_ENCODED_CHUNK_BYTES as u64 + 1;
    cases.push(encoded_too_large);
    let mut disguised_legacy = storage.clone();
    disguised_legacy.kind = RAW_MIRROR_FIXED_CHUNKS_KIND.to_string();
    cases.push(disguised_legacy);
    let mut missing_encoding = storage.clone();
    missing_encoding.chunks[0].encoding = None;
    missing_encoding.chunks[0].content_blake3 = None;
    missing_encoding.chunks[0].content_size_bytes = None;
    cases.push(missing_encoding);
    let mut wrong_plain_identity = storage;
    wrong_plain_identity.chunks[0].encoding = Some("none".to_string());
    cases.push(wrong_plain_identity);
    for invalid in cases {
        assert!(validate_raw_mirror_content_storage(&invalid).is_err());
        assert!(validate_content_storage_metadata(&serde_json::to_value(&invalid)?).is_err());
    }
    Ok(())
}

#[test]
fn gh562_valid_stored_hash_cannot_hide_wrong_decoded_length() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let root = raw_mirror_root(&data);
    let source = vec![b'x'; 4096];
    let mut storage = codec_fixture_storage(&root, &source, 4096, &[Some("zstd")])?;
    let declared_source = &source[..4095];
    storage.content_size_bytes = declared_source.len() as u64;
    storage.content_blake3 = blake3::hash(declared_source).to_hex().to_string();
    storage.chunks[0].content_size_bytes = Some(declared_source.len() as u64);
    storage.chunks[0].content_blake3 = Some(storage.content_blake3.clone());
    let manifest = codec_fixture_manifest(&data, declared_source, Some(storage))?;
    for reference in raw_mirror_manifest_blob_references(&manifest)? {
        verify_existing_blob_reference(&root, &reference)?;
    }
    assert!(read_source_bytes(&data, &manifest.manifest_id).is_err());
    assert!(verify_source_capture(&data, &manifest.manifest_id).is_err());
    assert!(cached_raw_mirror_blob_record_is_verified(
        &root,
        &codec_fixture_cache_record(&manifest),
    )
    .is_err());
    Ok(())
}
