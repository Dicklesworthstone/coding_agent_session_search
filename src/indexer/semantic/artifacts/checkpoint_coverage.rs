//! Cardinality admission for recovery, not canonical-content validation.
//!
//! A replayed WAL can replace a still-live main row after interruption between
//! log durability and tombstoning. Those are two physical records but only one
//! document. Neither that overlap nor duplicate legacy main IDs may stand in
//! for another acknowledged document when retiring fallback copies.

use anyhow::{Context, Result, ensure};
use frankensearch::index::VectorIndex;
use std::collections::HashSet;

pub(super) fn require_distinct_live(index: &VectorIndex, required: u64) -> Result<()> {
    if required == 0 {
        return Ok(());
    }
    // The caller retains the read-only index and its artifact lease. Borrow
    // IDs from the mapped main / already replayed WAL, never clone strings or
    // read the main vector slab. A failure to allocate retains fallback data.
    // Stop once the acknowledged count is reached: appends ahead of the last
    // checkpoint are valid, and the temporary set need not grow beyond it.
    let main_ids = (0..index.record_count())
        .filter(|&row| !index.is_deleted(row))
        .map(|row| index.doc_id_at(row));
    let wal_ids = index.wal_records().map(|(id, _)| Ok(id));
    let mut seen = HashSet::new();
    let mut distinct = 0u64;
    for id in main_ids.chain(wal_ids) {
        let id = id.context("cannot read checkpoint identity; retaining fallback artifacts")?;
        if seen.len() == seen.capacity() {
            seen.try_reserve(1)
                .context("cannot count checkpoint identities; retaining fallback artifacts")?;
        }
        if seen.insert(id) {
            distinct = distinct
                .checked_add(1)
                .context("checkpoint identity count overflow; retaining fallback artifacts")?;
            if distinct >= required {
                return Ok(());
            }
        }
    }
    ensure!(
        distinct >= required,
        "resumable semantic checkpoint has only {distinct} distinct live documents, below its recorded {required}; retaining fallback artifacts"
    );
    Ok(())
}
