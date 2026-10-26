//! Durable, disposable canonical replay identities. No message text is stored.
//!
//! Version 1 is a little-endian binary stream: `CASSRP01`, a length-prefixed
//! bounded JSON key, ordered conversation frames, and a count/checksum trailer.
//! A conversation declares its ID and document count; each document separately
//! frames its message ID, projected byte count, and UTF-8 semantic identity.
//! BLAKE3 covers every byte before the final 32-byte digest, including the
//! trailer counts. No inventory is returned before the digest and EOF agree.
//! The caller must still verify the canonical source and producer identity:
//! this file is neither a checkpoint nor authority to serve vectors.

use super::{CachedReplay, ConversationIdentity, DocumentIdentity, MAX_RETAINED_BYTES, ReplayKey};
use anyhow::{Context, Result, ensure};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::Path;

const MAGIC: &[u8; 8] = b"CASSRP01";
const CONVERSATION: u8 = 1;
const DOCUMENT: u8 = 2;
const TRAILER: u8 = 255;
const MAX_KEY_BYTES: usize = 16 * 1024;
const MAX_DOCUMENT_ID_BYTES: usize = 64 * 1024;
const TRAILER_BYTES: usize = 1 + 8 + 8 + 32;
const MIN_DOCUMENT_FRAME_BYTES: usize = 1 + 8 + 8 + 4;
const FIXED_FORMAT_BYTES: usize = MAGIC.len() + 4 + TRAILER_BYTES;

struct PayloadReader<R> {
    input: R,
    digest: blake3::Hasher,
    bytes_read: u64,
}

impl<R: Read> PayloadReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            digest: blake3::Hasher::new(),
            bytes_read: 0,
        }
    }

    fn read_into(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.input.read_exact(bytes)?;
        self.digest.update(bytes);
        self.bytes_read = self.bytes_read.saturating_add(bytes.len() as u64);
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

    fn finish(mut self) -> Result<()> {
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
        Ok(())
    }
}

struct PayloadWriter<W> {
    output: W,
    digest: blake3::Hasher,
}

impl<W: Write> PayloadWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            digest: blake3::Hasher::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)?;
        self.digest.update(bytes);
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

/// Missing, incompatible, and over-budget files are ordinary cache misses.
/// Corruption and I/O errors carry diagnostics for the caller to log before
/// falling back to canonical replay. Cancellation is checked within every
/// bounded record; callers must preserve cancellation when handling a miss.
pub(super) fn load(
    path: &Path,
    key: &ReplayKey,
    budget: usize,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<Option<CachedReplay>> {
    check_cancelled()?;
    let budget = budget.min(MAX_RETAINED_BYTES);
    if budget == 0 {
        return Ok(None);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect semantic replay inventory"),
    };
    // Refuse special files before opening them, including FIFOs and symlinks.
    // Frame overhead is smaller than the retained-record allowance, so only
    // the bounded key and fixed trailer need an extra on-disk allowance.
    let max_file_bytes = budget + MAX_KEY_BYTES + FIXED_FORMAT_BYTES;
    if !metadata.is_file() || metadata.len() > max_file_bytes as u64 {
        return Ok(None);
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open semantic replay inventory"),
    };
    let mut reader = PayloadReader::new(BufReader::new(file));
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
    if observed_key != *key {
        return Ok(None);
    }
    drop(key_bytes);

    let mut conversations: Vec<ConversationIdentity> = Vec::new();
    let mut retained_bytes = 0usize;
    let mut document_count = 0u64;
    let mut previous = None;
    loop {
        check_cancelled()?;
        let tag = reader.bytes::<1>()?[0];
        if tag == TRAILER {
            let declared_conversations = u64::from_le_bytes(reader.bytes()?);
            let declared_documents = u64::from_le_bytes(reader.bytes()?);
            ensure!(
                declared_conversations == u64::try_from(conversations.len())?
                    && declared_documents == document_count,
                "semantic replay trailer count mismatch"
            );
            reader.finish()?;
            check_cancelled()?;
            return Ok(Some(CachedReplay {
                key: observed_key,
                conversations,
                retained_bytes,
            }));
        }
        ensure!(
            tag == CONVERSATION,
            "invalid semantic replay conversation frame"
        );
        let conversation_id = i64::from_le_bytes(reader.bytes()?);
        ensure!(
            previous.is_none_or(|previous| conversation_id > previous),
            "semantic replay conversation order did not advance"
        );
        previous = Some(conversation_id);
        let declared_documents = u64::from_le_bytes(reader.bytes()?);
        let Ok(count) = usize::try_from(declared_documents) else {
            return Ok(None);
        };
        // Charge the actual in-memory types before reserving any capacity.
        // The outer Vec's growth has the same 2x allowance as the live cache.
        let next_bytes = count
            .checked_mul(size_of::<DocumentIdentity>())
            .and_then(|bytes| bytes.checked_add(size_of::<ConversationIdentity>() * 2))
            .and_then(|bytes| retained_bytes.checked_add(bytes));
        let Some(next_bytes) = next_bytes.filter(|bytes| *bytes <= budget) else {
            return Ok(None);
        };
        ensure!(
            declared_documents
                <= reader.remaining_before_trailer(metadata.len())
                    / MIN_DOCUMENT_FRAME_BYTES as u64,
            "semantic replay document count exceeds the remaining file"
        );
        retained_bytes = next_bytes;
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
                id_len as u64 <= reader.remaining_before_trailer(metadata.len()),
                "semantic replay document identity length exceeds the remaining file"
            );
            let Some(next_bytes) = retained_bytes
                .checked_add(id_len)
                .filter(|bytes| *bytes <= budget)
            else {
                return Ok(None);
            };
            retained_bytes = next_bytes;
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
        document_count = document_count
            .checked_add(declared_documents)
            .context("semantic replay document count overflow")?;
        // Vec's first allocation normally reserves four records. Start with
        // one explicitly, keeping even a one-conversation cache in its budget.
        if conversations.is_empty() {
            conversations.try_reserve_exact(1)?;
        } else {
            conversations.try_reserve(1)?;
        }
        conversations.push(ConversationIdentity {
            conversation_id,
            documents: documents.into_boxed_slice(),
        });
    }
}

/// Publish only a complete checksummed inventory. Failure or cancellation
/// before the atomic rename leaves the previous cache intact. A directory-sync
/// failure can follow publication, but never exposes an incomplete inventory.
/// Canonical and vector files are never mutated.
pub(super) fn save(
    path: &Path,
    inventory: &CachedReplay,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<()> {
    check_cancelled()?;
    let mut key_bytes = KeyBuffer::default();
    serde_json::to_writer(&mut key_bytes, &inventory.key)
        .context("encode bounded semantic replay key")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(".semantic-replay-")
        .tempfile_in(parent)
        .context("stage semantic replay inventory")?;
    {
        let mut writer = PayloadWriter::new(BufWriter::new(staged.as_file_mut()));
        writer.write(MAGIC)?;
        writer.write(&u32::try_from(key_bytes.0.len())?.to_le_bytes())?;
        writer.write(&key_bytes.0)?;
        let mut previous = None;
        let mut retained_bytes = 0usize;
        let mut document_count = 0u64;
        for conversation in &inventory.conversations {
            check_cancelled()?;
            ensure!(
                previous.is_none_or(|previous| conversation.conversation_id > previous),
                "semantic replay conversation order did not advance"
            );
            previous = Some(conversation.conversation_id);
            retained_bytes = conversation
                .retained_bytes()
                .and_then(|bytes| retained_bytes.checked_add(bytes))
                .context("semantic replay retained byte count overflow")?;
            ensure!(
                retained_bytes <= MAX_RETAINED_BYTES,
                "semantic replay inventory exceeds its retained byte limit"
            );
            let count = u64::try_from(conversation.documents.len())?;
            document_count = document_count
                .checked_add(count)
                .context("semantic replay document count overflow")?;
            writer.write(&[CONVERSATION])?;
            writer.write(&conversation.conversation_id.to_le_bytes())?;
            writer.write(&count.to_le_bytes())?;
            for document in &conversation.documents {
                check_cancelled()?;
                ensure!(
                    document.id.len() <= MAX_DOCUMENT_ID_BYTES,
                    "semantic replay document identity exceeds its byte limit"
                );
                writer.write(&[DOCUMENT])?;
                writer.write(&document.message_id.to_le_bytes())?;
                writer.write(&u64::try_from(document.content_bytes)?.to_le_bytes())?;
                writer.write(&u32::try_from(document.id.len())?.to_le_bytes())?;
                writer.write(document.id.as_bytes())?;
            }
        }
        ensure!(
            retained_bytes == inventory.retained_bytes,
            "semantic replay retained byte accounting mismatch"
        );
        writer.write(&[TRAILER])?;
        writer.write(&u64::try_from(inventory.conversations.len())?.to_le_bytes())?;
        writer.write(&document_count.to_le_bytes())?;
        writer.finish()?;
    }
    check_cancelled()?;
    staged
        .as_file()
        .sync_all()
        .context("sync semantic replay inventory")?;
    check_cancelled()?;
    let _published = staged
        .persist(path)
        .map_err(|error| error.error)
        .context("publish semantic replay inventory")?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .context("sync semantic replay inventory directory")?;
    Ok(())
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
}
