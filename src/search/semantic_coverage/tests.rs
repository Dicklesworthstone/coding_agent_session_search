use super::*;

fn identity(id: i64) -> [u8; 32] {
    let mut value = [0_u8; 32];
    for chunk in value.as_chunks_mut::<8>().0 {
        chunk.copy_from_slice(&id.to_le_bytes());
    }
    value
}

fn certificate(parent: &SemanticParentArtifact, maximum: i64) -> SemanticCoverageCertificate {
    SemanticCoverageCertificate {
        artifact_path: format!("vector_index/served/{}.fsvi", "0".repeat(64)),
        artifact_sha256: "0".repeat(64),
        parent_path: parent.path.clone(),
        parent_sha256: parent.sha256.clone(),
        parent_count: parent.count,
        archive_id: "0".repeat(32),
        mutation_seq: 1,
        max_message_id: maximum,
        db_fingerprint: "fixture".into(),
        vector_space_revision: "fixture-space".into(),
        published_at_ms: 1,
        doc_count: parent.count,
    }
}

fn write_ids(
    root: &Path,
    base: Option<&SemanticCoverageCertificate>,
    ids: &[i64],
) -> Result<SemanticParentArtifact> {
    write_parent_inventory(root, base, |emit| {
        for &id in ids {
            emit(id, identity(id))?;
        }
        Ok(())
    })
}

#[test]
fn semantic_parent_proof_authenticates_and_reads_sparse_messages() -> Result<()> {
    let root = tempfile::tempdir()?;
    let artifact = write_ids(root.path(), None, &[1, 3, 8, 21])?;
    let cert = certificate(&artifact, 21);
    let reader = SemanticParentReader::open(root.path(), &cert)?;
    assert_eq!(reader.count(), 4);
    assert_eq!(reader.identity(8)?, Some(identity(8)));
    assert_eq!(reader.identity(2)?, None);
    assert_eq!(reader.identity(0)?, None);
    assert_eq!(reader.identities(&[21, 1, 3, 3])?.len(), 3);
    let same = write_ids(root.path(), None, &[1, 3, 8, 21])?;
    assert_eq!(artifact, same);
    assert_eq!(reader.identity(21)?, Some(identity(21)));
    Ok(())
}

#[test]
fn semantic_parent_proof_rejects_mutation_and_invalid_structure() -> Result<()> {
    let root = tempfile::tempdir()?;
    let artifact = write_ids(root.path(), None, &[1, 2])?;
    let cert = certificate(&artifact, 2);
    let reader = SemanticParentReader::open(root.path(), &cert)?;
    let mut changed = std::fs::OpenOptions::new()
        .append(true)
        .open(root.path().join(&artifact.path))?;
    changed.write_all(b"x")?;
    changed.sync_all()?;
    assert!(reader.identity(1).is_err());
    assert!(SemanticParentReader::open(root.path(), &cert).is_err());

    // Give malformed records their real content digest: checksum validation
    // alone must not admit duplicate, nonpositive, or out-of-bound IDs.
    for (ids, maximum) in [([1_i64, 1_i64], 2), ([0, 2], 2), ([1, 3], 2)] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SEMANTIC_PARENT_MAGIC);
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        for id in ids {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&identity(id));
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        let relative = format!("vector_index/served/{digest}.parents");
        std::fs::write(root.path().join(&relative), bytes)?;
        let malformed = SemanticParentArtifact {
            path: relative,
            sha256: digest,
            count: 2,
        };
        assert!(
            SemanticParentReader::open(root.path(), &certificate(&malformed, maximum)).is_err()
        );
    }
    Ok(())
}

#[test]
fn semantic_parent_prefix_rejects_reidentity_deletion_and_retroactive_insert() -> Result<()> {
    let root = tempfile::tempdir()?;
    let artifact = write_ids(root.path(), None, &[1, 3, 5])?;
    let base = certificate(&artifact, 5);
    let extended = write_ids(root.path(), Some(&base), &[1, 3, 5, 7])?;
    assert_eq!(extended.count, 4);
    for ids in [&[1, 5, 7][..], &[1, 3][..], &[1, 2, 3, 5, 7][..]] {
        let error = write_ids(root.path(), Some(&base), ids).unwrap_err();
        assert!(error.to_string().contains("prefix"), "{error:#}");
    }
    let error = write_parent_inventory(root.path(), Some(&base), |emit| {
        emit(1, identity(1))?;
        emit(3, identity(99))?;
        emit(5, identity(5))?;
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("prefix"), "{error:#}");
    let old = SemanticParentReader::open(root.path(), &base)?;
    assert_eq!(old.identity(3)?, Some(identity(3)));
    Ok(())
}

#[test]
fn semantic_generation_identity_changes_with_parent_proof_and_boundary() -> Result<()> {
    let root = tempfile::tempdir()?;
    let parent = write_ids(root.path(), None, &[1])?;
    let first = certificate(&parent, 1);
    let mut second = first.clone();
    second.parent_sha256 = "f".repeat(64);
    second.parent_path = format!("vector_index/served/{}.parents", second.parent_sha256);
    assert_ne!(
        semantic_coverage_generation(&first),
        semantic_coverage_generation(&second)
    );
    second = first.clone();
    second.mutation_seq += 1;
    assert_ne!(
        semantic_coverage_generation(&first),
        semantic_coverage_generation(&second)
    );
    Ok(())
}

#[test]
#[ignore = "explicit million-message parent proof scale probe"]
fn semantic_parent_inventory_million_records_is_bounded() -> Result<()> {
    let root = tempfile::tempdir()?;
    let count = 1_000_001_i64;
    let artifact = write_parent_inventory(root.path(), None, |emit| {
        for id in 1..=count {
            emit(id, identity(id))?;
        }
        Ok(())
    })?;
    let started = std::time::Instant::now();
    let reader = SemanticParentReader::open(root.path(), &certificate(&artifact, count))?;
    let admission_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let ids = (0..1_000)
        .map(|n| 1 + (n * 7919) % count)
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    let found = reader.identities(&ids)?;
    for id in ids {
        assert_eq!(found.get(&id), Some(&identity(id)));
    }
    let lookup_ms = started.elapsed().as_secs_f64() * 1_000.0;
    println!(
        "semantic_parent_inventory_benchmark {}",
        serde_json::json!({
            "records": count,
            "file_bytes": reader.file_metadata()?.len(),
            "admission_ms": admission_ms,
            "lookup_count": 1000,
            "lookup_ms": lookup_ms,
            "reader_buffer_bytes": IO_BUFFER_BYTES,
            "record_bytes": SEMANTIC_PARENT_RECORD_BYTES,
        })
    );
    Ok(())
}
