//! Main-file identity lookup without an owned copy of the record inventory.
//!
//! The native table is ordered by (FNV-1a hash, exact document ID). Validate
//! that ordering once under the retained shared owner. Use the backend's
//! binary hash lookup, then compare exact IDs across the collision group.
//! A two-bit-per-row memo avoids decoding the same vector on every replay.
//! WAL overrides stay in ExistingIndexState, ahead of this lookup.

use super::*;
use frankensearch::core::fnv1a_hash;
use std::cell::Cell;

#[derive(Debug, Default)]
pub(super) struct MainCoverage {
    // 0 = not inspected, 1 = unusable, 2 = usable. Four rows per byte.
    signals: Vec<Cell<u8>>,
    tombstones: usize,
    #[cfg(test)]
    vector_checks: Cell<usize>,
}

impl MainCoverage {
    pub(super) fn inspect(
        source: &VectorIndex,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Option<Self>> {
        let mut previous = None;
        let mut tombstones = 0usize;
        for row in 0..source.record_count() {
            if cancelled() {
                return Ok(None);
            }
            let id = source.doc_id_at(row)?;
            let key = (fnv1a_hash(id.as_bytes()), id);
            ensure!(
                previous.is_none_or(|previous| previous <= key),
                "daemon semantic main table is not in native document order"
            );
            previous = Some(key);
            tombstones += usize::from(source.is_deleted(row));
        }
        if cancelled() {
            return Ok(None);
        }
        let bytes = source.record_count().div_ceil(4);
        let mut signals = Vec::new();
        signals.try_reserve_exact(bytes)?;
        signals.resize_with(bytes, || Cell::new(0));
        Ok(Some(Self {
            signals,
            tombstones,
            #[cfg(test)]
            vector_checks: Cell::new(0),
        }))
    }

    pub(super) fn tombstones(&self) -> usize {
        self.tombstones
    }

    pub(super) fn active_count(&self, source: &VectorIndex, id: &str) -> anyhow::Result<usize> {
        let hash = fnv1a_hash(id.as_bytes());
        let Some(first) = source.find_index_by_doc_hash(hash) else {
            return Ok(0);
        };
        let mut count = 0usize;
        let mut all_usable = true;
        for row in first..source.record_count() {
            let candidate = source.doc_id_at(row)?;
            if fnv1a_hash(candidate.as_bytes()) != hash {
                break;
            }
            // Hash equality never establishes document identity. In particular,
            // a colliding, unrelated record cannot supply missing coverage.
            if candidate == id && !source.is_deleted(row) {
                count += 1;
                all_usable &= self.row_usable(source, row)?;
            }
        }
        Ok(if all_usable { count } else { 0 })
    }

    fn row_usable(&self, source: &VectorIndex, row: usize) -> anyhow::Result<bool> {
        let byte = self
            .signals
            .get(row / 4)
            .context("semantic coverage row is out of range")?;
        let shift = (row % 4) * 2;
        match (byte.get() >> shift) & 3 {
            1 => Ok(false),
            2 => Ok(true),
            0 => {
                #[cfg(test)]
                self.vector_checks.set(self.vector_checks.get() + 1);
                let usable = source.is_vector_usable(row);
                let value = if usable { 2 } else { 1 };
                byte.set(byte.get() | (value << shift));
                Ok(usable)
            }
            _ => anyhow::bail!("invalid semantic coverage memo state"),
        }
    }
}

#[cfg(test)]
mod tests;
