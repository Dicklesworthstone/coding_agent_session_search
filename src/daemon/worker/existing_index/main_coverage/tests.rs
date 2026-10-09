use super::*;
use frankensearch::index::Quantization;
use std::time::Instant;

fn write_index(
    path: &Path,
    rows: &[(String, Vec<f32>)],
    quantization: Quantization,
) -> anyhow::Result<()> {
    let mut writer =
        VectorIndex::create_with_revision(path, "coverage-test", "v1", 4, quantization)?;
    for (id, vector) in rows {
        writer.write_record(id, vector)?;
    }
    writer.finish()?;
    Ok(())
}

fn inspect(source: &VectorIndex) -> anyhow::Result<MainCoverage> {
    MainCoverage::inspect(source, &|| false)?.context("unexpected cancellation")
}

// Independent incumbent: reproduce the exhaustive, owned-key census that the
// daemon used before on-demand lookup. Do not use MainCoverage or binary lookup.
fn exhaustive(source: &VectorIndex) -> anyhow::Result<HashMap<String, (usize, bool)>> {
    let mut counts = HashMap::new();
    for row in 0..source.record_count() {
        if source.is_deleted(row) {
            continue;
        }
        let entry = counts
            .entry(source.doc_id_at(row)?.to_owned())
            .or_insert((0, true));
        entry.0 += 1;
        entry.1 &= source.is_vector_usable(row);
    }
    Ok(counts)
}

#[test]
fn on_demand_counts_match_exhaustive_f16_and_f32_census() -> anyhow::Result<()> {
    for quantization in [Quantization::F16, Quantization::F32] {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("source.fsvi");
        let mut rows = Vec::new();
        for ordinal in 0..193 {
            let id = format!("canonical:{ordinal}:café:{}", ordinal % 7);
            let vector = vec![1.0, ordinal as f32 / 256.0, -0.5, 0.125];
            rows.push((id.clone(), vector.clone()));
            if ordinal % 13 == 0 {
                rows.push((id.clone(), vector));
            }
            if ordinal % 19 == 0 {
                rows.push((id, vec![1e-10; 4]));
            }
        }
        write_index(&path, &rows, quantization)?;
        let mut writer = VectorIndex::open_writer(&path)?;
        let deleted: Vec<_> = rows.iter().step_by(11).map(|(id, _)| id.as_str()).collect();
        writer.soft_delete_batch(&deleted)?;
        drop(writer);
        let source = VectorIndex::open_read_only(&path)?;
        let before = fs::read(&path)?;
        let expected = exhaustive(&source)?;
        let candidate = inspect(&source)?;
        assert_eq!(
            candidate.vector_checks.get(),
            0,
            "admission must not decode vectors"
        );
        let ids: HashSet<_> = rows.iter().map(|(id, _)| id.as_str()).collect();
        for _ in 0..3 {
            for id in &ids {
                let count = expected
                    .get(*id)
                    .map_or(0, |&(count, usable)| if usable { count } else { 0 });
                assert_eq!(
                    candidate.active_count(&source, id)?,
                    count,
                    "{quantization:?} {id}"
                );
            }
        }
        assert!(candidate.vector_checks.get() <= source.live_count());
        for id in ["absent", "canonical:1:cafe:1", "", "CANONICAL:1:café:1"] {
            assert_eq!(candidate.active_count(&source, id)?, 0);
        }
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn large_main_uses_two_bits_per_row_and_only_decodes_requested_vectors() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("source.fsvi");
    let rows: Vec<_> = (0..8193)
        .map(|ordinal| {
            (
                format!("document:{ordinal}:{}", "provenance".repeat(12)),
                vec![1.0, -0.25, 0.5, 0.125],
            )
        })
        .collect();
    write_index(&path, &rows, Quantization::F16)?;
    let source = VectorIndex::open_read_only(&path)?;
    let start = Instant::now();
    let incumbent = exhaustive(&source)?;
    let incumbent_elapsed = start.elapsed();
    let retained_key_bytes: usize = incumbent.keys().map(String::capacity).sum();
    let start = Instant::now();
    let candidate = inspect(&source)?;
    let request = [0, 5, 71, 107, 1001, 2048, 4096, 8192];
    for &ordinal in &request {
        assert_eq!(candidate.active_count(&source, &rows[ordinal].0)?, 1);
    }
    let candidate_elapsed = start.elapsed();
    assert_eq!(candidate.signals.len(), 2049);
    assert_eq!(candidate.vector_checks.get(), request.len());
    assert!(candidate.signals.capacity() < retained_key_bytes / 100);
    for _ in 0..4 {
        for &ordinal in &request {
            assert_eq!(candidate.active_count(&source, &rows[ordinal].0)?, 1);
        }
    }
    assert_eq!(
        candidate.vector_checks.get(),
        request.len(),
        "replays reuse the bitmap"
    );
    eprintln!(
        "coverage comparison: rows={} requested={} incumbent={incumbent_elapsed:?} candidate={candidate_elapsed:?} old_key_bytes={retained_key_bytes} new_bitmap_bytes={} (not RSS)",
        rows.len(),
        request.len(),
        candidate.signals.capacity()
    );
    Ok(())
}

#[test]
fn cached_unusable_rows_and_deleted_duplicates_do_not_supply_coverage() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("source.fsvi");
    let rows = vec![
        ("bad".into(), vec![1e-10; 4]),
        ("overflow".into(), vec![1e6; 4]),
        ("good".into(), vec![1.0; 4]),
        ("duplicate".into(), vec![1.0; 4]),
        ("duplicate".into(), vec![1.0; 4]),
        ("deleted".into(), vec![1.0; 4]),
        ("deleted".into(), vec![1.0; 4]),
    ];
    write_index(&path, &rows, Quantization::F16)?;
    let mut writer = VectorIndex::open_writer(&path)?;
    assert_eq!(writer.soft_delete_batch(&["deleted"])?, 2);
    drop(writer);
    let source = VectorIndex::open_read_only(&path)?;
    let candidate = inspect(&source)?;
    for _ in 0..3 {
        for (id, count) in [
            ("bad", 0),
            ("overflow", 0),
            ("good", 1),
            ("duplicate", 2),
            ("deleted", 0),
        ] {
            assert_eq!(candidate.active_count(&source, id)?, count);
        }
    }
    assert_eq!(candidate.tombstones(), 2);
    assert_eq!(candidate.vector_checks.get(), 5);
    Ok(())
}

#[test]
fn memo_is_bound_to_one_retained_generation_not_the_replacement_path() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("source.fsvi");
    let replacement = temp.path().join("replacement.fsvi");
    write_index(
        &path,
        &[("same-id".into(), vec![1.0; 4])],
        Quantization::F16,
    )?;
    let old_source = VectorIndex::open_read_only(&path)?;
    let old = inspect(&old_source)?;
    assert_eq!(old.active_count(&old_source, "same-id")?, 1);
    write_index(
        &replacement,
        &[("same-id".into(), vec![1e-10; 4])],
        Quantization::F16,
    )?;
    // Rename is the same immutable-reader ownership pattern as publication;
    // this fixture intentionally retains the old reader while opening the new.
    fs::rename(&replacement, &path)?;
    let new_source = VectorIndex::open_read_only(&path)?;
    let new = inspect(&new_source)?;
    assert_eq!(new.vector_checks.get(), 0);
    assert_eq!(new.active_count(&new_source, "same-id")?, 0);
    assert_eq!(old.active_count(&old_source, "same-id")?, 1);
    assert_eq!(new.vector_checks.get(), 1);
    assert_eq!(old.vector_checks.get(), 1);
    Ok(())
}

#[test]
fn metadata_admission_is_cancellable_and_empty_files_need_no_bitmap() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("source.fsvi");
    write_index(&path, &[], Quantization::F16)?;
    let source = VectorIndex::open_read_only(&path)?;
    let empty = inspect(&source)?;
    assert!(empty.signals.is_empty());
    assert_eq!(empty.active_count(&source, "missing")?, 0);
    assert!(MainCoverage::inspect(&source, &|| true)?.is_none());
    drop(source);
    write_index(
        &path,
        &[("one".into(), vec![1.0; 4]), ("two".into(), vec![1.0; 4])],
        Quantization::F16,
    )?;
    let source = VectorIndex::open_read_only(&path)?;
    let before = fs::read(&path)?;
    for stop in 1..=3 {
        let probes = Cell::new(0);
        assert!(
            MainCoverage::inspect(&source, &|| {
                probes.set(probes.get() + 1);
                probes.get() == stop
            })?
            .is_none()
        );
    }
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}

#[test]
fn forged_hash_match_never_supplies_a_different_document() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("source.fsvi");
    write_index(&path, &[("actual".into(), vec![1.0; 4])], Quantization::F16)?;
    // Corrupt only the native record's hash, not its exact ID. This is an
    // adversarial hash-match fixture, not a claimed natural FNV collision.
    let mut bytes = fs::read(&path)?;
    let original_hash = fnv1a_hash(b"actual").to_le_bytes();
    let offsets: Vec<_> = bytes
        .windows(8)
        .enumerate()
        .filter_map(|(offset, value)| (value == original_hash).then_some(offset))
        .collect();
    assert_eq!(offsets.len(), 1);
    bytes[offsets[0]..offsets[0] + 8].copy_from_slice(&fnv1a_hash(b"missing").to_le_bytes());
    fs::write(&path, &bytes)?;
    let source = VectorIndex::open_read_only(&path)?;
    assert_eq!(
        source.find_index_by_doc_hash(fnv1a_hash(b"missing")),
        Some(0)
    );
    let candidate = inspect(&source)?;
    assert_eq!(candidate.active_count(&source, "missing")?, 0);
    assert_eq!(candidate.vector_checks.get(), 0);
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}
