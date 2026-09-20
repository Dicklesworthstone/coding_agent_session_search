//! Raw source preservation with admission checks shared by every capture route.
//!
//! Connector discovery is not the final privacy boundary: an empty inventory
//! can activate the indexer's legacy raw-mirror fallback (GH #486). Enforce
//! exclusions here too, before source access, mirror creation, locks or caches.

mod exclusions;
mod store;

pub use exclusions::RawMirrorSourceExcluded;
pub use store::*;

/// Capture an admitted source using the existing raw-mirror storage policy.
///
/// An excluded source returns [`RawMirrorSourceExcluded`] without reading the
/// source or changing the mirror. Existing captures are not purged by changing
/// scan exclusions; pruning remains a separate, explicit operation.
pub fn capture_source_file(
    input: RawMirrorCaptureInput<'_>,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    exclusions::ensure_allowed(input.source_path)?;
    store::capture_source_file(input)
}

pub(crate) fn capture_source_file_with_chunk_policy(
    input: RawMirrorCaptureInput<'_>,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    exclusions::ensure_allowed(input.source_path)?;
    store::capture_source_file_with_chunk_policy(input, chunk_threshold_bytes, chunk_size_bytes)
}
