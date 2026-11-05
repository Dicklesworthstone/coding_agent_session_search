//! Immutable per-message provenance proofs for certified semantic generations.
//!
//! The storage projection owns canonical identity semantics. This module stores
//! only its fixed-size digests, ordered by message ID, and retains the verified
//! file descriptor used by a query. A proof is not publication authority by
//! itself: the manifest certificate binds it to one vector artifact and archive.

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::franken_sync::Connection as FrankenConnection;
use crate::franken_sync::compat::TransactionExt;
use crate::storage::sqlite::visit_semantic_parent_identities;

use super::semantic_manifest::SemanticCoverageCertificate;

pub const SEMANTIC_PARENT_MAGIC: [u8; 8] = *b"CASSPAR1";
pub const SEMANTIC_PARENT_HEADER_BYTES: u64 = 16;
pub const SEMANTIC_PARENT_RECORD_BYTES: u64 = 40;
const IO_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticParentArtifact {
    pub path: String,
    pub sha256: String,
    pub count: u64,
}

/// Include the entire authority, not just vector bytes: unchanged vectors can
/// have a newly verified parent proof and publication boundary.
pub fn semantic_coverage_generation(certificate: &SemanticCoverageCertificate) -> String {
    let mut hash = Sha256::new();
    hash.update(b"cass-semantic-coverage-v1\0");
    for value in [
        &certificate.artifact_path,
        &certificate.artifact_sha256,
        &certificate.parent_path,
        &certificate.parent_sha256,
        &certificate.archive_id,
        &certificate.db_fingerprint,
        &certificate.vector_space_revision,
    ] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    hash.update(certificate.parent_count.to_le_bytes());
    hash.update(certificate.mutation_seq.to_le_bytes());
    hash.update(certificate.max_message_id.to_le_bytes());
    hash.update(certificate.published_at_ms.to_le_bytes());
    hash.update(certificate.doc_count.to_le_bytes());
    hex::encode(hash.finalize())
}

/// Open only a regular file below the configured asset root. Intermediate
/// symlinks cannot redirect an otherwise well-formed content-addressed path.
pub(crate) fn open_semantic_proof_file(data_dir: &Path, relative: &str) -> Result<File> {
    let path = Path::new(relative);
    ensure!(
        !relative.is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "semantic proof path must remain within its data directory"
    );
    let mut resolved = data_dir.to_path_buf();
    for component in path.components() {
        resolved.push(component);
        let metadata = std::fs::symlink_metadata(&resolved)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "semantic proof path contains a symlink"
        );
    }
    let before = std::fs::metadata(&resolved)?;
    ensure!(before.is_file(), "semantic proof must be a regular file");
    let file = File::open(&resolved)?;
    ensure_same_file_metadata(&before, &file.metadata()?)?;
    Ok(file)
}

/// Descriptor identity/change detection. Certified serving is enabled only on
/// Unix, where ctime detects in-place writes even when mtime is restored.
pub(crate) fn ensure_same_file_metadata(before: &Metadata, after: &Metadata) -> Result<()> {
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "semantic proof changed while it was retained"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (
                before.dev(),
                before.ino(),
                before.ctime(),
                before.ctime_nsec()
            ) == (after.dev(), after.ino(), after.ctime(), after.ctime_nsec()),
            "semantic proof identity changed while it was retained"
        );
    }
    Ok(())
}

pub(crate) fn semantic_file_sha256(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_parent_record(record: &[u8; 40]) -> Result<(i64, [u8; 32])> {
    let mut identity = [0_u8; 32];
    identity.copy_from_slice(&record[8..]);
    let mut id = [0_u8; 8];
    id.copy_from_slice(&record[..8]);
    let id = i64::from_le_bytes(id);
    ensure!(id > 0, "semantic parent message ID must be positive");
    Ok((id, identity))
}

#[derive(Debug)]
pub struct SemanticParentReader {
    file: Mutex<File>,
    metadata: Metadata,
    path: PathBuf,
    count: u64,
}

impl SemanticParentReader {
    pub fn open(data_dir: &Path, certificate: &SemanticCoverageCertificate) -> Result<Self> {
        Self::open_verified(
            data_dir,
            &certificate.parent_path,
            &certificate.parent_sha256,
            certificate.parent_count,
            certificate.max_message_id,
        )
    }

    fn open_verified(
        data_dir: &Path,
        relative: &str,
        sha256: &str,
        count: u64,
        maximum: i64,
    ) -> Result<Self> {
        ensure!(valid_digest(sha256), "invalid semantic parent digest");
        ensure!(
            relative == format!("vector_index/served/{sha256}.parents"),
            "semantic parent path does not match its digest"
        );
        let mut file = open_semantic_proof_file(data_dir, relative)?;
        let metadata = file.metadata()?;
        let expected_length = count
            .checked_mul(SEMANTIC_PARENT_RECORD_BYTES)
            .and_then(|bytes| bytes.checked_add(SEMANTIC_PARENT_HEADER_BYTES))
            .context("semantic parent file length overflow")?;
        ensure!(
            metadata.len() == expected_length,
            "semantic parent length mismatch"
        );
        let mut hash = Sha256::new();
        {
            let mut input = BufReader::with_capacity(IO_BUFFER_BYTES, &mut file);
            let mut header = [0_u8; 16];
            input.read_exact(&mut header)?;
            ensure!(
                header[..8] == SEMANTIC_PARENT_MAGIC,
                "unknown semantic parent format"
            );
            let mut stored_count = [0_u8; 8];
            stored_count.copy_from_slice(&header[8..]);
            ensure!(
                u64::from_le_bytes(stored_count) == count,
                "semantic parent count mismatch"
            );
            hash.update(header);
            let mut previous = 0_i64;
            let mut record = [0_u8; 40];
            for _ in 0..count {
                input.read_exact(&mut record)?;
                let (id, _) = decode_parent_record(&record)?;
                ensure!(
                    id > previous && id <= maximum,
                    "semantic parent IDs are not an ordered publication prefix"
                );
                previous = id;
                hash.update(record);
            }
            let mut extra = [0_u8; 1];
            ensure!(
                input.read(&mut extra)? == 0,
                "semantic parent trailing bytes"
            );
        }
        ensure!(
            hex::encode(hash.finalize()) == sha256,
            "semantic parent checksum mismatch"
        );
        ensure_same_file_metadata(&metadata, &file.metadata()?)?;
        Ok(Self {
            file: Mutex::new(file),
            metadata,
            path: data_dir.join(relative),
            count,
        })
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_metadata(&self) -> Result<Metadata> {
        self.file
            .lock()
            .map_err(|_| anyhow::anyhow!("semantic parent reader lock poisoned"))?
            .metadata()
            .context("read retained semantic parent metadata")
    }

    pub fn identity(&self, message_id: i64) -> Result<Option<[u8; 32]>> {
        Ok(self.identities(&[message_id])?.remove(&message_id))
    }

    /// Candidate-bounded lookup; no archive-sized identity map is allocated.
    pub fn identities(&self, message_ids: &[i64]) -> Result<HashMap<i64, [u8; 32]>> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("semantic parent reader lock poisoned"))?;
        ensure_same_file_metadata(&self.metadata, &file.metadata()?)?;
        let mut result = HashMap::with_capacity(message_ids.len());
        let mut record = [0_u8; 40];
        for &requested in message_ids {
            if requested <= 0 || result.contains_key(&requested) {
                continue;
            }
            let (mut lower, mut upper) = (0_u64, self.count);
            while lower < upper {
                let middle = lower + (upper - lower) / 2;
                let offset = SEMANTIC_PARENT_HEADER_BYTES + middle * SEMANTIC_PARENT_RECORD_BYTES;
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut record)?;
                let (message_id, identity) = decode_parent_record(&record)?;
                match message_id.cmp(&requested) {
                    std::cmp::Ordering::Less => lower = middle + 1,
                    std::cmp::Ordering::Greater => upper = middle,
                    std::cmp::Ordering::Equal => {
                        result.insert(requested, identity);
                        break;
                    }
                }
            }
        }
        ensure_same_file_metadata(&self.metadata, &file.metadata()?)?;
        Ok(result)
    }
}

struct InheritedParentPrefix {
    input: BufReader<File>,
    metadata: Metadata,
    remaining: u64,
    maximum: i64,
}

impl InheritedParentPrefix {
    fn open(data_dir: &Path, certificate: &SemanticCoverageCertificate) -> Result<Self> {
        let reader = SemanticParentReader::open(data_dir, certificate)?;
        let mut file = reader
            .file
            .into_inner()
            .map_err(|_| anyhow::anyhow!("semantic parent reader lock poisoned"))?;
        file.seek(SeekFrom::Start(SEMANTIC_PARENT_HEADER_BYTES))?;
        Ok(Self {
            input: BufReader::with_capacity(IO_BUFFER_BYTES, file),
            metadata: reader.metadata,
            remaining: reader.count,
            maximum: certificate.max_message_id,
        })
    }

    fn verify(&mut self, id: i64, digest: [u8; 32]) -> Result<()> {
        if id > self.maximum {
            ensure!(
                self.remaining == 0,
                "semantic parent prefix lost a published message"
            );
            return Ok(());
        }
        ensure!(
            self.remaining != 0,
            "semantic parent prefix contains an unverified insertion"
        );
        let mut record = [0_u8; 40];
        self.input.read_exact(&mut record)?;
        let expected = decode_parent_record(&record)?;
        ensure!(
            expected == (id, digest),
            "semantic parent prefix changed; canonical reconciliation is required"
        );
        self.remaining -= 1;
        Ok(())
    }

    fn finish(self) -> Result<()> {
        ensure!(
            self.remaining == 0,
            "semantic parent prefix lost its published tail"
        );
        ensure_same_file_metadata(&self.metadata, &self.input.get_ref().metadata()?)
    }
}

pub fn write_semantic_parents(
    connection: &FrankenConnection,
    data_dir: &Path,
) -> Result<SemanticParentArtifact> {
    write_semantic_parents_with_base(connection, data_dir, None)
}

/// Own one committed canonical read snapshot through every metadata page.
/// Existing transactions are rejected: uncommitted caller writes cannot become
/// publication authority. The caller additionally binds its selection and final
/// publication to this source observation (including before/after file proofs).
pub fn write_semantic_parents_with_base(
    connection: &FrankenConnection,
    data_dir: &Path,
    base: Option<&SemanticCoverageCertificate>,
) -> Result<SemanticParentArtifact> {
    ensure!(
        !connection.as_async().in_transaction(),
        "semantic parent publication requires a committed archive snapshot"
    );
    let mut transaction = connection.transaction()?;
    let artifact = write_parent_inventory(data_dir, base, |emit| {
        visit_semantic_parent_identities(connection, emit)
    })?;
    transaction.rollback()?;
    Ok(artifact)
}

fn write_parent_inventory(
    data_dir: &Path,
    base: Option<&SemanticCoverageCertificate>,
    stream: impl FnOnce(&mut dyn FnMut(i64, [u8; 32]) -> Result<()>) -> Result<()>,
) -> Result<SemanticParentArtifact> {
    let directory = data_dir.join("vector_index/served");
    std::fs::create_dir_all(&directory)?;
    for relative in ["vector_index", "vector_index/served"] {
        let metadata = std::fs::symlink_metadata(data_dir.join(relative))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "semantic serving directory must not be a symlink"
        );
    }
    let mut inherited = base
        .map(|certificate| InheritedParentPrefix::open(data_dir, certificate))
        .transpose()?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    let (mut count, mut maximum) = (0_u64, 0_i64);
    {
        let mut output = BufWriter::with_capacity(IO_BUFFER_BYTES, temporary.as_file_mut());
        output.write_all(&SEMANTIC_PARENT_MAGIC)?;
        output.write_all(&0_u64.to_le_bytes())?;
        stream(&mut |id, identity| {
            ensure!(
                id > maximum,
                "semantic parent stream is not strictly ordered"
            );
            if let Some(prefix) = &mut inherited {
                prefix.verify(id, identity)?;
            }
            output.write_all(&id.to_le_bytes())?;
            output.write_all(&identity)?;
            count = count
                .checked_add(1)
                .context("semantic parent count overflow")?;
            maximum = id;
            Ok(())
        })?;
        output.flush()?;
    }
    if let Some(prefix) = inherited {
        prefix.finish()?;
    }
    temporary.as_file_mut().seek(SeekFrom::Start(8))?;
    temporary.as_file_mut().write_all(&count.to_le_bytes())?;
    temporary.as_file().sync_all()?;
    let sha256 = semantic_file_sha256(temporary.as_file_mut())?;
    let path = format!("vector_index/served/{sha256}.parents");
    match temporary.persist_noclobber(data_dir.join(&path)) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            SemanticParentReader::open_verified(data_dir, &path, &sha256, count, maximum)?;
        }
        Err(error) => bail!("publish semantic parent proof: {}", error.error),
    }
    // A publisher reusing another writer's identical file must also flush the
    // directory entry before publishing its own manifest pointer.
    #[cfg(unix)]
    File::open(&directory)?.sync_all()?;
    Ok(SemanticParentArtifact {
        path,
        sha256,
        count,
    })
}

#[cfg(test)]
mod tests;
