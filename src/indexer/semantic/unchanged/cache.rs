//! Write the existing version-1 completed-build skip receipt from a successful
//! read-only proof. The engine remains the only cache reader. Its admission is
//! exercised by the tests, so serialization drift cannot silently lose the
//! constant-time path. This receipt is never serving authority.

use super::*;
use crate::search::semantic_manifest::ArtifactRecord;
use serde::Serialize;

#[derive(Serialize)]
struct FileStampV1 {
    len: u64,
    modified: (u64, u32),
    changed: (i64, i64),
    identity: (u64, u64),
}

impl FileStampV1 {
    fn from_observation(stamp: &FileStamp) -> Result<Self> {
        // Match engine.rs's SystemTime::duration_since(UNIX_EPOCH) encoding.
        // An unrepresentable timestamp merely prevents caching this proof.
        let seconds = u64::try_from(stamp.modified.0)?;
        let nanos = u32::try_from(stamp.modified.1)?;
        ensure!(nanos < 1_000_000_000, "invalid filesystem nanoseconds");
        Ok(Self {
            len: stamp.len,
            modified: (seconds, nanos),
            changed: stamp.changed,
            identity: stamp.identity,
        })
    }
}

#[derive(Serialize)]
struct FilePairV1<'a> {
    path: &'a Path,
    main: FileStampV1,
    wal: Option<FileStampV1>,
}

#[derive(Serialize)]
struct CompletedBackfillCacheV1<'a> {
    version: u32,
    archive: FilePairV1<'a>,
    vectors: FilePairV1<'a>,
    artifact: &'a ArtifactRecord,
    vector_space_revision: &'static str,
    last_offset: i64,
}

pub(super) fn refresh(
    data_dir: &Path,
    archive: &ArchiveStamp,
    vector_path: &Path,
    vectors: &FileStamp,
    artifact: &ArtifactRecord,
    last_offset: i64,
) -> Result<()> {
    let vector_path = vector_path.canonicalize()?;
    let cache = CompletedBackfillCacheV1 {
        version: 1,
        archive: FilePairV1 {
            path: &archive.path,
            main: FileStampV1::from_observation(&archive.main)?,
            wal: archive.wal.as_ref().map(FileStampV1::from_observation).transpose()?,
        },
        vectors: FilePairV1 {
            path: &vector_path,
            main: FileStampV1::from_observation(vectors)?,
            wal: None,
        },
        artifact,
        vector_space_revision: engine::expected_vector_space_revision(&artifact.embedder_id)
            .context("unknown proved vector-space revision")?,
        last_offset,
    };
    // Preserve the exact observations that were proved, not new stamps from
    // after the proof. A concurrent change then invalidates the receipt at
    // the engine's next read instead of borrowing our earlier content proof.
    let bytes = serde_json::to_vec(&cache)?;
    ensure!(bytes.len() <= 64 * 1024, "completed receipt exceeds the engine's read budget");
    let root = data_dir.join(crate::search::vector_index::VECTOR_INDEX_DIR);
    let path = root.join(format!(
        ".completed-backfill-{}-{}.json", artifact.tier.as_str(), artifact.embedder_id,
    ));
    let mut staged = tempfile::NamedTempFile::new_in(&root)?;
    use std::io::Write;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path)?;
    // Loss of this advisory cache after a crash only repeats the read-only
    // proof. No manifest, checkpoint or serving file depends on its durability.
    Ok(())
}
