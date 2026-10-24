//! A document count cannot prove that the effective vector payload is usable.
//! WAL replacements take precedence over the main image, including a main row
//! that was not yet tombstoned when an acknowledged replacement became durable.

use anyhow::{Context, Result, ensure};
use frankensearch::index::VectorIndex;
use std::collections::HashSet;

pub(super) fn require_usable_live_vectors(index: &VectorIndex) -> Result<()> {
    // The native reader has already applied last-write-wins within the WAL.
    // Borrow its IDs rather than allocating another vector or string inventory.
    let mut wal_ids = HashSet::new();
    for (id, vector) in index.wal_records() {
        ensure!(
            usable_wal_vector(vector, index.dimension()),
            "published semantic WAL contains an unusable live vector; retaining fallback artifacts"
        );
        if wal_ids.len() == wal_ids.capacity() {
            wal_ids
                .try_reserve(1)
                .context("cannot track published WAL replacements; retaining fallback artifacts")?;
        }
        ensure!(
            wal_ids.insert(id),
            "published semantic WAL contains a duplicate live identity; retaining fallback artifacts"
        );
    }
    for row in 0..index.record_count() {
        if index.is_deleted(row) {
            continue;
        }
        let id = index
            .doc_id_at(row)
            .context("cannot read published vector identity; retaining fallback artifacts")?;
        if wal_ids.contains(id) {
            continue;
        }
        ensure!(
            index.is_vector_usable(row),
            "published semantic index has an unusable live vector at row {row}; retaining fallback artifacts"
        );
    }
    Ok(())
}

fn usable_wal_vector(vector: &[f32], dimension: usize) -> bool {
    // Match the f32 finite, positive-norm contract used by canonical publication.
    // This also rejects finite components whose squared norm overflows to infinity.
    let norm = vector.iter().fold(0.0f32, |sum, value| sum + value * value);
    vector.len() == dimension && norm.is_finite() && norm > 0.0
}

#[cfg(test)]
mod tests;
