//! Merge retained FSVI live rows in the engine's physical document order.
//!
//! FSVI v2 sorts by (FNV-1a document hash, document ID), NOT by document ID.
//! Lexical manifest ranges must not be concatenated to recreate a full-file
//! witness. A k-way merge keeps one borrowed head per shard, skips tombstones,
//! and neither reopens paths nor allocates another corpus-sized ID collection.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use frankensearch::index::ValidatedFsviBytes;
use sha2::{Digest, Sha256};

use super::{SemanticReaderError, SemanticSelectionResult};

/// The tie-breaking ID is necessary even when two FNV hashes collide.
pub(super) type DocumentKey<'a> = (u64, &'a str);

pub(super) struct LiveDocuments<'a> {
    shards: &'a [Arc<ValidatedFsviBytes>],
    next_rows: Vec<usize>,
    heads: BinaryHeap<Reverse<(u64, &'a str, usize)>>,
}

impl<'a> LiveDocuments<'a> {
    pub(super) fn new(shards: &'a [Arc<ValidatedFsviBytes>]) -> SemanticSelectionResult<Self> {
        let mut merged = Self {
            shards,
            next_rows: vec![0; shards.len()],
            heads: BinaryHeap::with_capacity(shards.len()),
        };
        for ordinal in 0..shards.len() {
            merged.advance(ordinal)?;
        }
        Ok(merged)
    }

    fn advance(&mut self, ordinal: usize) -> SemanticSelectionResult<()> {
        let owner: &'a ValidatedFsviBytes = &self.shards[ordinal];
        while self.next_rows[ordinal] < owner.record_count() {
            let position = self.next_rows[ordinal];
            self.next_rows[ordinal] += 1;
            if !owner.row(position)?.flags().is_live() {
                continue;
            }
            let id = owner.doc_id_at(position)?;
            // The admitted v2 image has already verified both this FNV-1a key
            // and strict physical row ordering. SHA-256, not FNV, authenticates
            // the document-set transcript below.
            let hash = id
                .as_bytes()
                .iter()
                .fold(0xcbf29ce484222325_u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x00000100000001b3)
                });
            self.heads.push(Reverse((hash, id, ordinal)));
            break;
        }
        Ok(())
    }

    pub(super) fn next(&mut self) -> SemanticSelectionResult<Option<DocumentKey<'a>>> {
        let Some(Reverse((hash, id, ordinal))) = self.heads.pop() else {
            return Ok(None);
        };
        self.advance(ordinal)?;
        Ok(Some((hash, id)))
    }
}

pub(super) fn digest(
    shards: &[Arc<ValidatedFsviBytes>],
    live_count: u64,
) -> SemanticSelectionResult<[u8; 32]> {
    let domain = b"frankensearch.fsvi-v2.ordered-live-docset.v1";
    let mut digest = Sha256::new();
    digest.update((domain.len() as u64).to_be_bytes());
    digest.update(domain);
    digest.update(live_count.to_be_bytes());
    let mut documents = LiveDocuments::new(shards)?;
    let mut previous = None;
    let mut observed = 0_u64;
    while let Some(key) = documents.next()? {
        if previous == Some(key) {
            return Err(SemanticReaderError::DuplicateDocument.into());
        }
        previous = Some(key);
        observed = observed
            .checked_add(1)
            .ok_or(SemanticReaderError::CountOverflow)?;
        digest.update((key.1.len() as u64).to_be_bytes());
        digest.update(key.1.as_bytes());
    }
    if observed != live_count {
        return Err(SemanticReaderError::CountOverflow.into());
    }
    Ok(digest.finalize().into())
}
