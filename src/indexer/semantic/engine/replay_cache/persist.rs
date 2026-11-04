//! Durable, disposable canonical replay identities. No message text is stored.
//!
//! Version 1 is a little-endian binary stream: `CASSRP01`, a length-prefixed
//! bounded JSON key, ordered conversation frames, and a count/checksum trailer.
//! A conversation declares its ID and document count; each document separately
//! frames its message ID, projected byte count, and UTF-8 semantic identity.
//! BLAKE3 covers every byte before the final 32-byte digest, including the
//! trailer counts. Streamed records are tentative until the digest and EOF
//! agree; callers must discard the entire selection when that validation fails.
//! The caller must still verify the canonical source and producer identity:
//! this file is neither a checkpoint nor authority to serve vectors.

#[cfg(test)]
use super::CachedReplay;
use super::{ConversationIdentity, DocumentIdentity, MAX_RETAINED_BYTES, ReplayKey};
use anyhow::{Context, Result, ensure};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"CASSRP01";
const CONVERSATION: u8 = 1;
const DOCUMENT: u8 = 2;
const TRAILER: u8 = 255;
const MAX_KEY_BYTES: usize = 16 * 1024;
const MAX_DOCUMENT_ID_BYTES: usize = 64 * 1024;
const TRAILER_BYTES: usize = 1 + 8 + 8 + 32;
const MIN_DOCUMENT_FRAME_BYTES: usize = 1 + 8 + 8 + 4;
#[cfg(test)]
const FIXED_FORMAT_BYTES: usize = MAGIC.len() + 4 + TRAILER_BYTES;
// Disk retention is independent of the in-memory inventory budget. Keep one
// replaceable file per tier, with an explicit ceiling on both reads and writes.
const MAX_INVENTORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;

struct PayloadReader<R> {
    input: R,
    digest: blake3::Hasher,
    bytes_read: u64,
    byte_limit: u64,
}

impl<R: Read> PayloadReader<R> {
    fn new(input: R, byte_limit: u64) -> Self {
        Self {
            input,
            digest: blake3::Hasher::new(),
            bytes_read: 0,
            byte_limit: byte_limit.min(MAX_INVENTORY_BYTES - 32),
        }
    }

    fn read_into(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        let next = self
            .bytes_read
            .checked_add(bytes.len() as u64)
            .filter(|next| *next <= self.byte_limit)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "semantic replay payload exceeds its captured byte limit",
                )
            })?;
        self.input.read_exact(bytes)?;
        self.digest.update(bytes);
        self.bytes_read = next;
        Ok(())
    }

    fn remaining_before_trailer(&self, file_bytes: u64) -> u64 {
        file_bytes
            .saturating_sub(self.bytes_read)
            .saturating_sub(TRAILER_BYTES as u64)
    }

    fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut bytes = [0; N];
        self.read_into(&mut bytes)?;
        Ok(bytes)
    }

    fn finish(mut self) -> Result<R> {
        let expected = self.digest.finalize();
        let mut actual = [0; 32];
        self.input
            .read_exact(&mut actual)
            .context("read semantic replay checksum")?;
        ensure!(
            actual == *expected.as_bytes(),
            "semantic replay checksum mismatch"
        );
        let mut extra = [0; 1];
        ensure!(
            self.input.read(&mut extra)? == 0,
            "semantic replay has trailing bytes"
        );
        Ok(self.input)
    }
}

struct PayloadWriter<W> {
    output: W,
    digest: blake3::Hasher,
    bytes_written: u64,
}

impl<W: Write> PayloadWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            digest: blake3::Hasher::new(),
            bytes_written: 0,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let next = self
            .bytes_written
            .checked_add(bytes.len() as u64)
            .filter(|next| *next <= MAX_INVENTORY_BYTES - 32)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "semantic replay inventory exceeds its disk byte limit",
                )
            })?;
        self.output.write_all(bytes)?;
        self.digest.update(bytes);
        self.bytes_written = next;
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        self.output.write_all(self.digest.finalize().as_bytes())?;
        self.output.flush()
    }
}

/// Bound serialization too: an unexpectedly large producer/path identity must
/// not allocate an unbounded temporary buffer before its size is checked.
#[derive(Default)]
struct KeyBuffer(Vec<u8>);

impl Write for KeyBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_KEY_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "semantic replay key exceeds its byte limit",
            ));
        }
        self.0
            .try_reserve(bytes.len())
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error.to_string()))?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct FrameBudgetExceeded;

impl std::fmt::Display for FrameBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("semantic replay conversation exceeds its retained byte limit")
    }
}

impl std::error::Error for FrameBudgetExceeded {}

/// A bounded sequential decoder. A returned conversation is not a validated
/// inventory: only `next_conversation` returning `None` authenticates the final
/// counts, checksum, and EOF. Never embed or publish from a partial traversal.
pub(super) struct InventoryReader {
    reader: Option<PayloadReader<BufReader<File>>>,
    file_bytes: u64,
    metadata: fs::Metadata,
    frame_budget: usize,
    conversation_count: u64,
    document_count: u64,
    previous: Option<i64>,
}

impl InventoryReader {
    #[cfg(test)]
    pub(super) fn open(
        path: &Path,
        key: &ReplayKey,
        frame_budget: usize,
        check_cancelled: impl FnMut() -> Result<()>,
    ) -> Result<Option<Self>> {
        Ok(Self::open_with_key(path, frame_budget, check_cancelled)?
            .and_then(|(observed, reader)| (observed == *key).then_some(reader)))
    }

    /// The header is bounded but remains tentative until the caller consumes
    /// and authenticates the complete stream. It may only select a replay plan,
    /// never authorize results or publication on its own.
    pub(super) fn open_with_key(
        path: &Path,
        frame_budget: usize,
        mut check_cancelled: impl FnMut() -> Result<()>,
    ) -> Result<Option<(ReplayKey, Self)>> {
        check_cancelled()?;
        let frame_budget = frame_budget.min(MAX_RETAINED_BYTES);
        if frame_budget == 0 {
            return Ok(None);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect semantic replay inventory"),
        };
        // Refuse special files, symlinks, and oversized inventories before
        // opening. Repeat the regular-file/size bound on the opened owner.
        if !metadata.is_file() || metadata.len() > MAX_INVENTORY_BYTES {
            return Ok(None);
        }
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open semantic replay inventory"),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_INVENTORY_BYTES {
            return Ok(None);
        }
        let mut reader =
            PayloadReader::new(BufReader::new(file), metadata.len().saturating_sub(32));
        ensure!(
            reader.bytes::<8>()? == *MAGIC,
            "unsupported semantic replay inventory format"
        );
        let key_len = u32::from_le_bytes(reader.bytes()?) as usize;
        ensure!(
            key_len <= MAX_KEY_BYTES,
            "semantic replay key exceeds its byte limit"
        );
        ensure!(
            key_len as u64 <= reader.remaining_before_trailer(metadata.len()),
            "semantic replay key length exceeds the remaining file"
        );
        let mut key_bytes = Vec::new();
        key_bytes.try_reserve_exact(key_len)?;
        key_bytes.resize(key_len, 0);
        reader.read_into(&mut key_bytes)?;
        let observed_key: ReplayKey =
            serde_json::from_slice(&key_bytes).context("decode semantic replay key")?;
        Ok(Some((
            observed_key,
            Self {
                reader: Some(reader),
                file_bytes: metadata.len(),
                metadata,
                frame_budget,
                conversation_count: 0,
                document_count: 0,
                previous: None,
            },
        )))
    }

    pub(super) fn next_conversation(
        &mut self,
        mut check_cancelled: impl FnMut() -> Result<()>,
    ) -> Result<Option<ConversationIdentity>> {
        check_cancelled()?;
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        let tag = reader.bytes::<1>()?[0];
        if tag == TRAILER {
            let declared_conversations = u64::from_le_bytes(reader.bytes()?);
            let declared_documents = u64::from_le_bytes(reader.bytes()?);
            ensure!(
                declared_conversations == self.conversation_count
                    && declared_documents == self.document_count,
                "semantic replay trailer count mismatch"
            );
            ensure!(
                reader.bytes_read.checked_add(32) == Some(self.file_bytes),
                "semantic replay has trailing bytes outside its captured payload"
            );
            verify_inventory_metadata(&self.metadata, &reader.input.get_ref().metadata()?)?;
            let input = self
                .reader
                .take()
                .context("semantic replay reader missing before checksum")?
                .finish()?;
            verify_inventory_metadata(&self.metadata, &input.get_ref().metadata()?)?;
            check_cancelled()?;
            return Ok(None);
        }
        ensure!(
            tag == CONVERSATION,
            "invalid semantic replay conversation frame"
        );
        let conversation_id = i64::from_le_bytes(reader.bytes()?);
        ensure!(
            self.previous
                .is_none_or(|previous| conversation_id > previous),
            "semantic replay conversation order did not advance"
        );
        self.previous = Some(conversation_id);
        let declared_documents = u64::from_le_bytes(reader.bytes()?);
        let count = usize::try_from(declared_documents).map_err(|_| FrameBudgetExceeded)?;
        // Charge the actual in-memory types before reserving any capacity.
        // Only this conversation is decoded; no archive-sized Vec is required.
        let mut retained_bytes = count
            .checked_mul(size_of::<DocumentIdentity>())
            .and_then(|bytes| bytes.checked_add(size_of::<ConversationIdentity>() * 2))
            .filter(|bytes| *bytes <= self.frame_budget)
            .ok_or(FrameBudgetExceeded)?;
        ensure!(
            declared_documents
                <= reader.remaining_before_trailer(self.file_bytes)
                    / MIN_DOCUMENT_FRAME_BYTES as u64,
            "semantic replay document count exceeds the remaining file"
        );
        let mut documents = Vec::new();
        documents.try_reserve_exact(count)?;
        for _ in 0..count {
            check_cancelled()?;
            ensure!(
                reader.bytes::<1>()?[0] == DOCUMENT,
                "invalid semantic replay document frame"
            );
            let message_id = u64::from_le_bytes(reader.bytes()?);
            let content_bytes = usize::try_from(u64::from_le_bytes(reader.bytes()?))
                .context("semantic replay projected byte count does not fit this platform")?;
            let id_len = u32::from_le_bytes(reader.bytes()?) as usize;
            ensure!(
                id_len <= MAX_DOCUMENT_ID_BYTES,
                "semantic replay document identity exceeds its byte limit"
            );
            ensure!(
                id_len as u64 <= reader.remaining_before_trailer(self.file_bytes),
                "semantic replay document identity length exceeds the remaining file"
            );
            retained_bytes = retained_bytes
                .checked_add(id_len)
                .filter(|bytes| *bytes <= self.frame_budget)
                .ok_or(FrameBudgetExceeded)?;
            let mut id = Vec::new();
            id.try_reserve_exact(id_len)?;
            id.resize(id_len, 0);
            reader.read_into(&mut id)?;
            documents.push(DocumentIdentity {
                id: String::from_utf8(id)
                    .context("semantic replay document identity is not UTF-8")?
                    .into_boxed_str(),
                message_id,
                content_bytes,
            });
        }
        self.document_count = self
            .document_count
            .checked_add(declared_documents)
            .context("semantic replay document count overflow")?;
        self.conversation_count = self
            .conversation_count
            .checked_add(1)
            .context("semantic replay conversation count overflow")?;
        Ok(Some(ConversationIdentity {
            conversation_id,
            documents: documents.into_boxed_slice(),
        }))
    }
}

fn verify_inventory_metadata(before: &fs::Metadata, after: &fs::Metadata) -> Result<()> {
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "semantic replay inventory changed while it was read"
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
            "semantic replay inventory identity changed while it was read"
        );
    }
    Ok(())
}

/// A private streaming inventory. `push` never changes the installed cache;
/// `finish` adds the trailer, syncs, and atomically installs the complete file.
pub(super) struct InventoryWriter {
    staged: tempfile::NamedTempFile,
    writer: PayloadWriter<BufWriter<File>>,
    path: PathBuf,
    frame_budget: usize,
    previous: Option<i64>,
    conversation_count: u64,
    document_count: u64,
}

impl InventoryWriter {
    pub(super) fn create(
        path: &Path,
        key: &ReplayKey,
        frame_budget: usize,
        mut check_cancelled: impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        check_cancelled()?;
        let mut key_bytes = KeyBuffer::default();
        serde_json::to_writer(&mut key_bytes, key).context("encode bounded semantic replay key")?;
        let staged = tempfile::Builder::new()
            .prefix(".semantic-replay-")
            .tempfile_in(parent_directory(path))
            .context("stage semantic replay inventory")?;
        let mut writer = PayloadWriter::new(BufWriter::new(staged.as_file().try_clone()?));
        writer.write(MAGIC)?;
        writer.write(&u32::try_from(key_bytes.0.len())?.to_le_bytes())?;
        writer.write(&key_bytes.0)?;
        Ok(Self {
            staged,
            writer,
            path: path.to_owned(),
            frame_budget: frame_budget.min(MAX_RETAINED_BYTES),
            previous: None,
            conversation_count: 0,
            document_count: 0,
        })
    }

    pub(super) fn push(
        &mut self,
        conversation: &ConversationIdentity,
        mut check_cancelled: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        check_cancelled()?;
        ensure!(
            self.previous
                .is_none_or(|previous| conversation.conversation_id > previous),
            "semantic replay conversation order did not advance"
        );
        let retained_bytes = conversation
            .retained_bytes()
            .context("semantic replay retained byte count overflow")?;
        ensure!(
            retained_bytes <= self.frame_budget,
            "semantic replay conversation exceeds its retained byte limit"
        );
        self.previous = Some(conversation.conversation_id);
        let count = u64::try_from(conversation.documents.len())?;
        self.document_count = self
            .document_count
            .checked_add(count)
            .context("semantic replay document count overflow")?;
        self.conversation_count = self
            .conversation_count
            .checked_add(1)
            .context("semantic replay conversation count overflow")?;
        self.writer.write(&[CONVERSATION])?;
        self.writer
            .write(&conversation.conversation_id.to_le_bytes())?;
        self.writer.write(&count.to_le_bytes())?;
        for document in &conversation.documents {
            check_cancelled()?;
            ensure!(
                document.id.len() <= MAX_DOCUMENT_ID_BYTES,
                "semantic replay document identity exceeds its byte limit"
            );
            self.writer.write(&[DOCUMENT])?;
            self.writer.write(&document.message_id.to_le_bytes())?;
            self.writer
                .write(&u64::try_from(document.content_bytes)?.to_le_bytes())?;
            self.writer
                .write(&u32::try_from(document.id.len())?.to_le_bytes())?;
            self.writer.write(document.id.as_bytes())?;
        }
        Ok(())
    }

    pub(super) fn finish(mut self, mut check_cancelled: impl FnMut() -> Result<()>) -> Result<()> {
        check_cancelled()?;
        self.writer.write(&[TRAILER])?;
        self.writer.write(&self.conversation_count.to_le_bytes())?;
        self.writer.write(&self.document_count.to_le_bytes())?;
        self.writer.finish()?;
        check_cancelled()?;
        self.staged
            .as_file()
            .sync_all()
            .context("sync semantic replay inventory")?;
        check_cancelled()?;
        let _published = self
            .staged
            .persist(&self.path)
            .map_err(|error| error.error)
            .context("publish semantic replay inventory")?;
        #[cfg(unix)]
        File::open(parent_directory(&self.path))
            .and_then(|directory| directory.sync_all())
            .context("sync semantic replay inventory directory")?;
        Ok(())
    }
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Collecting adapters keep the existing format tests exercising the same
/// decoder/writer as production. Production selection consumes the stream.
#[cfg(test)]
fn load(
    path: &Path,
    key: &ReplayKey,
    budget: usize,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<Option<CachedReplay>> {
    check_cancelled()?;
    let budget = budget.min(MAX_RETAINED_BYTES);
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.len() > (budget + MAX_KEY_BYTES + FIXED_FORMAT_BYTES) as u64
    {
        return Ok(None);
    }
    let Some(mut reader) = InventoryReader::open(path, key, budget, &mut check_cancelled)? else {
        return Ok(None);
    };
    let mut inventory = CachedReplay {
        key: key.clone(),
        conversations: Vec::new(),
        retained_bytes: 0,
    };
    loop {
        let conversation = match reader.next_conversation(&mut check_cancelled) {
            Ok(Some(conversation)) => conversation,
            Ok(None) => return Ok(Some(inventory)),
            Err(error) if error.is::<FrameBudgetExceeded>() => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(bytes) = conversation
            .retained_bytes()
            .and_then(|bytes| inventory.retained_bytes.checked_add(bytes))
            .filter(|bytes| *bytes <= budget)
        else {
            return Ok(None);
        };
        if inventory.conversations.is_empty() {
            inventory.conversations.try_reserve_exact(1)?;
        } else {
            inventory.conversations.try_reserve(1)?;
        }
        inventory.retained_bytes = bytes;
        inventory.conversations.push(conversation);
    }
}

#[cfg(test)]
fn save(
    path: &Path,
    inventory: &CachedReplay,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut writer = InventoryWriter::create(
        path,
        &inventory.key,
        MAX_RETAINED_BYTES,
        &mut check_cancelled,
    )?;
    let mut retained_bytes = 0usize;
    for conversation in &inventory.conversations {
        retained_bytes = conversation
            .retained_bytes()
            .and_then(|bytes| retained_bytes.checked_add(bytes))
            .context("semantic replay retained byte count overflow")?;
        ensure!(
            retained_bytes <= MAX_RETAINED_BYTES,
            "semantic replay inventory exceeds its retained byte limit"
        );
        writer.push(conversation, &mut check_cancelled)?;
    }
    ensure!(
        retained_bytes == inventory.retained_bytes,
        "semantic replay retained byte accounting mismatch"
    );
    writer.finish(check_cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inventory() -> Result<CachedReplay> {
        let key = serde_json::from_value(json!({
            "archive": {
                "path": "/fixture/archive.db",
                "main": {"len": 4096, "modified": [1, 2], "changed": [3, 4], "identity": [5, 6]},
                "wal": null
            },
            "embedder_id": "hash",
            "vector_revision": "hash-revision",
            "model_revision": "hash",
            "db_fingerprint": "content-v1:3:3:2",
            "schema_version": 1,
            "chunking_version": 2,
            "source_ingest_contract": "fixture-input-contract"
        }))?;
        let conversations = vec![
            ConversationIdentity {
                conversation_id: i64::MIN,
                documents: Box::new([]),
            },
            ConversationIdentity {
                conversation_id: 0,
                documents: vec![
                    DocumentIdentity {
                        id: "alpha\n雪".into(),
                        message_id: 0,
                        content_bytes: 0,
                    },
                    DocumentIdentity {
                        id: "omega".into(),
                        message_id: u64::MAX,
                        content_bytes: usize::MAX,
                    },
                ]
                .into_boxed_slice(),
            },
            ConversationIdentity {
                conversation_id: i64::MAX,
                documents: Box::new([]),
            },
        ];
        let retained_bytes = conversations
            .iter()
            .try_fold(0usize, |total, conversation| {
                total.checked_add(conversation.retained_bytes()?)
            })
            .context("fixture retained byte count")?;
        Ok(CachedReplay {
            key,
            conversations,
            retained_bytes,
        })
    }

    #[test]
    fn durable_inventory_preserves_optional_canonical_journal_boundary() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("inventory.bin");
        let mut expected = inventory()?;
        assert!(
            expected.key.journal.is_none(),
            "legacy keys have no journal authority"
        );
        expected.key.journal = Some(serde_json::from_value(json!({
            "archive_id": "0123456789abcdef0123456789abcdef",
            "mutation_seq": 42,
        }))?);
        save(&path, &expected, || Ok(()))?;
        let (observed, mut reader) =
            InventoryReader::open_with_key(&path, MAX_RETAINED_BYTES, || Ok(()))?
                .context("inventory with journal boundary")?;
        assert!(observed == expected.key);
        let mut conversations = 0;
        while reader.next_conversation(|| Ok(()))?.is_some() {
            conversations += 1;
        }
        assert_eq!(conversations, expected.conversations.len());
        Ok(())
    }

    fn load_fixture(path: &Path, inventory: &CachedReplay) -> Result<Option<CachedReplay>> {
        load(path, &inventory.key, inventory.retained_bytes, || Ok(()))
    }

    fn first_conversation_offset(bytes: &[u8]) -> usize {
        MAGIC.len()
            + 4
            + u32::from_le_bytes(bytes[MAGIC.len()..MAGIC.len() + 4].try_into().unwrap()) as usize
    }

    fn update_checksum(bytes: &mut [u8]) {
        let checksum_offset = bytes.len() - 32;
        let digest = blake3::hash(&bytes[..checksum_offset]);
        bytes[checksum_offset..].copy_from_slice(digest.as_bytes());
    }

    #[test]
    fn durable_inventory_roundtrips_signed_empty_and_unicode_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let actual = load_fixture(&path, &expected)?.context("inventory miss")?;
        assert!(actual.key == expected.key);
        assert_eq!(actual.retained_bytes, expected.retained_bytes);
        assert_eq!(actual.conversations.len(), expected.conversations.len());
        for (actual, expected) in actual.conversations.iter().zip(&expected.conversations) {
            assert_eq!(actual.conversation_id, expected.conversation_id);
            assert_eq!(actual.documents, expected.documents);
        }
        Ok(())
    }

    #[test]
    fn durable_inventory_empty_payload_and_count_trailer_are_validated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let mut expected = inventory()?;
        expected.conversations.clear();
        expected.retained_bytes = 0;
        save(&path, &expected, || Ok(()))?;
        let actual = load(&path, &expected.key, 1, || Ok(()))?.context("empty cache miss")?;
        assert!(actual.conversations.is_empty());
        assert_eq!(actual.retained_bytes, 0);

        let mut bytes = fs::read(&path)?;
        let count_offset = bytes.len() - 32 - 8 - 8;
        bytes[count_offset..count_offset + 8].copy_from_slice(&1u64.to_le_bytes());
        update_checksum(&mut bytes);
        fs::write(&path, &bytes)?;
        assert!(
            load(&path, &expected.key, 1, || Ok(()))
                .err()
                .context("invalid count admitted")?
                .to_string()
                .contains("count mismatch")
        );
        Ok(())
    }

    #[test]
    fn durable_inventory_missing_key_mismatch_and_budget_are_misses() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        assert!(load_fixture(&path, &expected)?.is_none());
        save(&path, &expected, || Ok(()))?;
        let mut other = expected.key.clone();
        other.model_revision = "another-model".into();
        assert!(load(&path, &other, MAX_RETAINED_BYTES, || Ok(()))?.is_none());
        other = expected.key.clone();
        other.source_ingest_contract = "another-parser".into();
        assert!(load(&path, &other, MAX_RETAINED_BYTES, || Ok(()))?.is_none());
        assert!(load(&path, &expected.key, expected.retained_bytes - 1, || Ok(()))?.is_none());
        assert!(load(&path, &expected.key, 0, || Ok(()))?.is_none());
        Ok(())
    }

    #[test]
    fn durable_inventory_rejects_corruption_truncation_and_trailing_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let original = fs::read(&path)?;

        let mut corrupt = original.clone();
        let marker = corrupt
            .windows(5)
            .position(|window| window == b"alpha")
            .context("fixture document marker")?;
        corrupt[marker] = b'A';
        fs::write(&path, &corrupt)?;
        assert!(
            load_fixture(&path, &expected)
                .err()
                .context("corruption admitted")?
                .to_string()
                .contains("checksum")
        );
        fs::write(&path, &original[..original.len() - 1])?;
        assert!(load_fixture(&path, &expected).is_err());
        let mut trailing = original;
        trailing.push(0);
        fs::write(&path, &trailing)?;
        assert!(
            load_fixture(&path, &expected)
                .err()
                .context("trailing data admitted")?
                .to_string()
                .contains("trailing")
        );
        Ok(())
    }

    #[test]
    fn durable_inventory_rejects_out_of_order_records_even_with_valid_checksum() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let mut bytes = fs::read(&path)?;
        // The first conversation is empty, so the second frame follows its
        // tag, signed ID and count immediately.
        let second_id = first_conversation_offset(&bytes) + 1 + 8 + 8 + 1;
        bytes[second_id..second_id + 8].copy_from_slice(&i64::MIN.to_le_bytes());
        update_checksum(&mut bytes);
        fs::write(&path, &bytes)?;
        assert!(
            load_fixture(&path, &expected)
                .err()
                .context("unordered cache admitted")?
                .to_string()
                .contains("order")
        );
        Ok(())
    }

    #[test]
    fn durable_inventory_refuses_hostile_lengths_before_allocation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let original = fs::read(&path)?;
        let mut bytes = original.clone();
        bytes[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(load_fixture(&path, &expected).is_err());

        let mut bytes = original.clone();
        let count_offset = first_conversation_offset(&bytes) + 1 + 8;
        bytes[count_offset..count_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(load_fixture(&path, &expected)?.is_none());

        let mut bytes = original.clone();
        bytes[count_offset..count_offset + 8].copy_from_slice(&100_000u64.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(
            load(&path, &expected.key, MAX_RETAINED_BYTES, || Ok(()))
                .err()
                .context("impossible document count admitted")?
                .to_string()
                .contains("remaining file")
        );

        let mut bytes = original;
        let first_document_len = first_conversation_offset(&bytes) + 17 + 17 + 1 + 8 + 8;
        bytes[first_document_len..first_document_len + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(load_fixture(&path, &expected).is_err());
        Ok(())
    }

    #[test]
    fn durable_inventory_cancelled_save_preserves_previous_complete_file() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let before = fs::read(&path)?;
        let mut checks = 0;
        let result = save(&path, &expected, || {
            checks += 1;
            ensure!(checks < 4, "cancelled inventory save");
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path)?, before);
        assert!(load_fixture(&path, &expected)?.is_some());
        Ok(())
    }

    #[test]
    fn durable_inventory_load_can_cancel_between_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let mut checks = 0;
        let result = load(&path, &expected.key, MAX_RETAINED_BYTES, || {
            checks += 1;
            ensure!(checks < 4, "cancelled inventory load");
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(checks, 4);
        Ok(())
    }

    #[test]
    fn durable_inventory_save_bounds_key_and_document_frames() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("replay.bin");
        let mut oversized_key = inventory()?;
        oversized_key.key.model_revision = "k".repeat(MAX_KEY_BYTES + 1);
        assert!(save(&path, &oversized_key, || Ok(())).is_err());
        assert!(!path.exists());
        let mut oversized_id = inventory()?;
        oversized_id.conversations[1].documents[0].id =
            "d".repeat(MAX_DOCUMENT_ID_BYTES + 1).into_boxed_str();
        assert!(save(&path, &oversized_id, || Ok(())).is_err());
        assert!(!path.exists());
        Ok(())
    }

    fn scale_conversation(conversation_id: i64, count: usize) -> ConversationIdentity {
        let padding = "x".repeat(120);
        let documents = (0..count)
            .map(|ordinal| {
                let message_id = conversation_id as u64 * count as u64 + ordinal as u64;
                DocumentIdentity {
                    id: format!("m{message_id:012}{padding}").into_boxed_str(),
                    message_id,
                    content_bytes: 2048,
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        ConversationIdentity {
            conversation_id,
            documents,
        }
    }

    #[test]
    fn streamed_inventory_reuses_complete_files_larger_than_the_memory_budget() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("streamed.bin");
        let key = inventory()?.key;
        let frame_budget = 4096;
        let mut writer = InventoryWriter::create(&path, &key, frame_budget, || Ok(()))?;
        for conversation_id in 0..256 {
            writer.push(&scale_conversation(conversation_id, 16), || Ok(()))?;
        }
        writer.finish(|| Ok(()))?;
        assert!(fs::metadata(&path)?.len() > frame_budget as u64);
        assert!(load(&path, &key, frame_budget, || Ok(()))?.is_none());
        let mut reader = InventoryReader::open(&path, &key, frame_budget, || Ok(()))?
            .context("streamed inventory miss")?;
        let mut count = 0;
        while let Some(conversation) = reader.next_conversation(|| Ok(()))? {
            assert!(conversation.retained_bytes().unwrap() <= frame_budget);
            assert_eq!(conversation.conversation_id, count);
            assert_eq!(
                conversation.documents,
                scale_conversation(count, 16).documents
            );
            count += 1;
        }
        assert_eq!(count, 256);
        Ok(())
    }

    #[test]
    fn streamed_inventory_does_not_accept_a_valid_prefix_with_a_bad_trailer() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("streamed.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let mut bytes = fs::read(&path)?;
        *bytes.last_mut().context("fixture checksum")? ^= 1;
        fs::write(&path, bytes)?;
        let mut reader =
            InventoryReader::open(&path, &expected.key, MAX_RETAINED_BYTES, || Ok(()))?
                .context("streamed inventory miss")?;
        for conversation in &expected.conversations {
            let actual = reader
                .next_conversation(|| Ok(()))?
                .context("missing frame")?;
            assert_eq!(actual.conversation_id, conversation.conversation_id);
            assert_eq!(actual.documents, conversation.documents);
        }
        assert!(
            reader
                .next_conversation(|| Ok(()))
                .err()
                .context("invalid trailer accepted")?
                .to_string()
                .contains("checksum")
        );
        Ok(())
    }

    #[test]
    fn streamed_inventory_stops_at_captured_length_when_the_file_grows() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("growing.bin");
        let key = inventory()?.key;
        let mut writer = InventoryWriter::create(&path, &key, 1024, || Ok(()))?;
        // Exceed BufReader's prefix so the reader actually reaches newly
        // appended frames instead of validating an already-buffered trailer.
        for conversation_id in 1..=1024 {
            writer.push(
                &ConversationIdentity {
                    conversation_id,
                    documents: Box::new([]),
                },
                || Ok(()),
            )?;
        }
        writer.finish(|| Ok(()))?;
        let original = fs::read(&path)?;
        let mut reader =
            InventoryReader::open(&path, &key, 1024, || Ok(()))?.context("growing reader")?;
        assert!(reader.next_conversation(|| Ok(()))?.is_some());
        let mut extended = original[..original.len() - TRAILER_BYTES].to_vec();
        extended.push(CONVERSATION);
        extended.extend_from_slice(&1025_i64.to_le_bytes());
        extended.extend_from_slice(&0_u64.to_le_bytes());
        extended.push(TRAILER);
        extended.extend_from_slice(&1025_u64.to_le_bytes());
        extended.extend_from_slice(&0_u64.to_le_bytes());
        extended.extend_from_slice(blake3::hash(&extended).as_bytes());
        fs::write(&path, extended)?;
        let error = loop {
            match reader.next_conversation(|| Ok(())) {
                Ok(Some(_)) => {}
                Ok(None) => anyhow::bail!("growing inventory was accepted"),
                Err(error) => break error,
            }
        };
        assert!(
            error.to_string().contains("captured byte limit"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn streamed_inventory_rejects_rewritten_owner_even_with_restored_mtime() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rewritten.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let modified = fs::metadata(&path)?.modified()?;
        let mut reader =
            InventoryReader::open(&path, &expected.key, MAX_RETAINED_BYTES, || Ok(()))?
                .context("rewritten reader")?;
        assert!(reader.next_conversation(|| Ok(()))?.is_some());
        let mut rewritten = fs::read(&path)?;
        let marker = rewritten
            .windows(5)
            .position(|window| window == b"alpha")
            .context("fixture identity")?;
        rewritten[marker] = b'A';
        update_checksum(&mut rewritten);
        fs::write(&path, rewritten)?;
        File::open(&path)?.set_modified(modified)?;
        let error = loop {
            match reader.next_conversation(|| Ok(())) {
                Ok(Some(_)) => {}
                Ok(None) => anyhow::bail!("rewritten inventory owner was accepted"),
                Err(error) => break error,
            }
        };
        assert!(
            error.to_string().contains("changed while it was read"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn streamed_inventory_bounds_frames_and_disk_without_replacing_a_complete_file() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("streamed.bin");
        let expected = inventory()?;
        save(&path, &expected, || Ok(()))?;
        let before = fs::read(&path)?;
        let mut oversized = InventoryWriter::create(&path, &expected.key, 1, || Ok(()))?;
        assert!(
            oversized
                .push(&scale_conversation(0, 1), || Ok(()))
                .is_err()
        );
        drop(oversized);
        assert_eq!(fs::read(&path)?, before);

        let mut cancelled = InventoryWriter::create(&path, &expected.key, 4096, || Ok(()))?;
        cancelled.push(&scale_conversation(0, 16), || Ok(()))?;
        assert!(
            cancelled
                .finish(|| anyhow::bail!("cancelled before install"))
                .is_err()
        );
        assert_eq!(fs::read(&path)?, before);

        let oversized_path = directory.path().join("oversized.bin");
        File::create(&oversized_path)?.set_len(MAX_INVENTORY_BYTES + 1)?;
        assert!(InventoryReader::open(&oversized_path, &expected.key, 4096, || Ok(()))?.is_none());
        assert_eq!(fs::read(&path)?, before);
        Ok(())
    }

    /// Format-scale probe, not a database, embedding, or search benchmark.
    /// Run explicitly with --ignored --exact --nocapture.
    #[test]
    #[ignore = "writes and validates more than 128 MiB of real inventory records"]
    fn streamed_inventory_retains_a_million_passages_above_the_old_ceiling() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("million.bin");
        let key = inventory()?.key;
        let frame_budget = 256 * 1024;
        let conversations = 1024;
        let documents_per_conversation = 1024;
        let started = std::time::Instant::now();
        let mut writer = InventoryWriter::create(&path, &key, frame_budget, || Ok(()))?;
        for conversation_id in 0..conversations {
            writer.push(
                &scale_conversation(conversation_id, documents_per_conversation),
                || Ok(()),
            )?;
        }
        writer.finish(|| Ok(()))?;
        let write_elapsed_us = started.elapsed().as_micros();
        let disk_bytes = fs::metadata(&path)?.len();
        assert!(disk_bytes > MAX_RETAINED_BYTES as u64);
        assert!(load(&path, &key, MAX_RETAINED_BYTES, || Ok(()))?.is_none());
        let started = std::time::Instant::now();
        let mut reader = InventoryReader::open(&path, &key, frame_budget, || Ok(()))?
            .context("million-passage inventory miss")?;
        let mut observed_conversations = 0;
        let mut observed_documents = 0;
        let mut peak_retained_frame_bytes = 0;
        while let Some(conversation) = reader.next_conversation(|| Ok(()))? {
            let retained = conversation.retained_bytes().context("frame accounting")?;
            peak_retained_frame_bytes = peak_retained_frame_bytes.max(retained);
            assert!(retained <= frame_budget);
            assert_eq!(conversation.conversation_id, observed_conversations);
            assert_eq!(conversation.documents.len(), documents_per_conversation);
            assert_eq!(
                conversation.documents,
                scale_conversation(observed_conversations, documents_per_conversation).documents
            );
            observed_conversations += 1;
            observed_documents += conversation.documents.len();
        }
        assert_eq!(observed_conversations, conversations);
        assert_eq!(observed_documents, 1_048_576);
        let read_elapsed_us = started.elapsed().as_micros();
        eprintln!(
            "{}",
            json!({
                "scenario": "million_passage_replay_inventory_format",
                "passage_documents": observed_documents,
                "conversations": observed_conversations,
                "disk_bytes": disk_bytes,
                "memory_budget_bytes": MAX_RETAINED_BYTES,
                "frame_budget_bytes": frame_budget,
                "peak_retained_frame_bytes": peak_retained_frame_bytes,
                "memory_cache_retained": false,
                "write_elapsed_us": write_elapsed_us,
                "read_elapsed_us": read_elapsed_us
            })
        );
        Ok(())
    }
}
