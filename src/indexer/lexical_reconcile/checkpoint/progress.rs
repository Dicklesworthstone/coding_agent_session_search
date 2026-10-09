//! Durable cursors for the two targeted-repair publication passes.
//!
//! A cursor is only reusable with the same canonical binding AND the same
//! admitted Quill publication. Commits precede cursor writes. A crash in that
//! gap, a rollback, or an unrelated publication therefore causes a safe replay,
//! never a skipped range whose publication we cannot establish. Reading and
//! hashing the canonical prefix is intentional: only writes are skipped.
//! The skipped documents must also exist with their native content witnesses;
//! a cursor can name a publication without proving what it contains.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::super::canary::exact::PublishedSnapshot;
use super::super::{CanonicalProjection, LexicalReconcileCheckpoint};
use super::ProjectionFingerprint;
use crate::search::tantivy::TantivyIndex;

const VERSION: u32 = 1;
const MAX_PROGRESS_BYTES: u64 = 64 * 1024;
// Match Quill's manifest admission limit, without allocating that much memory.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Pass {
    Publish,
    Replay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Publication {
    manifest_blake3: Option<String>,
    previous_blake3: Option<String>,
    live_docs: u64,
}

impl Publication {
    fn read(index_path: &Path, index: &TantivyIndex) -> Result<Self> {
        let manifest_blake3 = file_digest(&index_path.join("MANIFEST"))?;
        let previous_blake3 = file_digest(&index_path.join("MANIFEST.prev"))?;
        ensure!(
            manifest_blake3.is_some() || previous_blake3.is_some(),
            "repair publication has no durable manifest; recovery retained"
        );
        Ok(Self {
            manifest_blake3,
            previous_blake3,
            live_docs: index.doc_count()?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Progress {
    version: u32,
    // Keep the original checkpoint format untouched. This companion receipt is
    // optional; older versions safely ignore it and replay the whole source.
    binding: LexicalReconcileCheckpoint,
    pass: Pass,
    completed_docs: usize,
    prefix_blake3: String,
    first_pass_live_docs: Option<u64>,
    publication: Option<Publication>,
}

impl Progress {
    fn fresh(binding: &LexicalReconcileCheckpoint) -> Self {
        Self {
            version: VERSION,
            binding: binding.clone(),
            pass: Pass::Publish,
            completed_docs: 0,
            prefix_blake3: prefix_digest(&ProjectionFingerprint::new(binding.expected_docs)),
            first_pass_live_docs: None,
            publication: None,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported repair progress version; receipt retained"
        );
        ensure!(
            self.binding.version == super::VERSION
                && self.binding.conversation_id > 0
                && self.binding.expected_docs > 0
                && super::valid_digest(self.binding.projection_blake3.as_deref())
                && self.completed_docs <= self.binding.expected_docs
                && super::valid_digest(Some(&self.prefix_blake3)),
            "invalid repair progress binding or cursor; receipt retained"
        );
        ensure!(
            (self.pass == Pass::Replay) == self.first_pass_live_docs.is_some(),
            "repair progress has inconsistent pass accounting; receipt retained"
        );
        ensure!(
            self.publication.is_some(),
            "repair progress lacks a durable publication; receipt retained"
        );
        if let Some(publication) = &self.publication {
            ensure!(
                publication.manifest_blake3.is_some() || publication.previous_blake3.is_some(),
                "repair progress lacks a manifest witness; receipt retained"
            );
            for digest in [
                publication.manifest_blake3.as_deref(),
                publication.previous_blake3.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                ensure!(
                    super::valid_digest(Some(digest)),
                    "invalid repair manifest digest"
                );
            }
        }
        Ok(())
    }

    fn matches_binding(&self, current: &LexicalReconcileCheckpoint) -> bool {
        let old = &self.binding;
        old.version == current.version
            && old.conversation_id == current.conversation_id
            && old.source_id == current.source_id
            && old.source_path == current.source_path
            && old.message_count == current.message_count
            && old.max_message_idx == current.max_message_idx
            && old.content_bytes == current.content_bytes
            && old.expected_docs == current.expected_docs
            && old.projection_blake3 == current.projection_blake3
            && old.started_at_ms == current.started_at_ms
    }

    fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        ensure!(
            serde_json::to_vec_pretty(self)?.len() as u64 <= MAX_PROGRESS_BYTES,
            "repair progress exceeds its persistence budget; recovery retained"
        );
        crate::indexer::write_json_pretty_atomically(path, self)
            .context("persisting lexical repair progress after publication")
    }
}

fn prefix_digest(fingerprint: &ProjectionFingerprint) -> String {
    fingerprint.hasher.finalize().to_hex().to_string()
}

fn progress_path(checkpoint_path: &Path) -> PathBuf {
    checkpoint_path.with_extension("progress.json")
}

/// Bounded, non-following regular-file open. No FIFO/device is opened on Unix;
/// verify descriptor identity as well as size, since metadata alone races.
fn open_regular(path: &Path, max_bytes: u64) -> Result<Option<File>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", path.display()));
        }
    };
    ensure!(
        before.is_file() && before.len() <= max_bytes,
        "invalid repair state file {}",
        path.display()
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && opened.len() <= max_bytes,
        "invalid repair state descriptor"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "repair state file was replaced during open"
        );
    }
    #[cfg(not(unix))]
    ensure!(
        before.len() == opened.len() && before.modified()? == opened.modified()?,
        "repair state file changed during open"
    );
    Ok(Some(file))
}

fn file_digest(path: &Path) -> Result<Option<String>> {
    let Some(file) = open_regular(path, MAX_MANIFEST_BYTES)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    let mut reader = (&file).take(MAX_MANIFEST_BYTES + 1);
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(
            bytes <= MAX_MANIFEST_BYTES,
            "repair manifest exceeded its read budget"
        );
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    ensure!(
        bytes == metadata.len()
            && after.len() == metadata.len()
            && after.modified()? == metadata.modified()?,
        "repair manifest changed while being read"
    );
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

fn load(path: &Path) -> Result<Option<Progress>> {
    let Some(file) = open_regular(path, MAX_PROGRESS_BYTES)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_PROGRESS_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PROGRESS_BYTES,
        "repair progress exceeded its read budget"
    );
    let progress: Progress = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing repair progress {}; receipt retained",
            path.display()
        )
    })?;
    progress.validate()?;
    Ok(Some(progress))
}

fn resume(
    path: &Path,
    index_path: &Path,
    index: &TantivyIndex,
    binding: &LexicalReconcileCheckpoint,
) -> Result<Progress> {
    let Some(mut previous) = load(path)? else {
        return Ok(Progress::fresh(binding));
    };
    // Native admission must succeed before a manifest digest can authorize
    // skipping writes. Raw checkpoint bytes alone are not an admitted index.
    let _reader = index.reader()?;
    if !previous.matches_binding(binding)
        || previous.publication.as_ref() != Some(&Publication::read(index_path, index)?)
    {
        tracing::warn!(
            conversation_id = binding.conversation_id,
            "lexical repair progress no longer matches canonical/publication authority; replaying from the start"
        );
        return Ok(Progress::fresh(binding));
    }
    previous.binding = binding.clone();
    tracing::info!(
        conversation_id = binding.conversation_id,
        pass = ?previous.pass,
        completed_docs = previous.completed_docs,
        "resuming durable lexical repair publication"
    );
    Ok(previous)
}

pub(in crate::indexer::lexical_reconcile) struct Outcome {
    pub upserted_docs: usize,
    pub first_pass_live_docs: u64,
}

/// Only a proven content mismatch may discard the optimization and replay.
/// Codec/I/O errors and malformed canonical bindings must remain failures.
#[derive(Debug, thiserror::Error)]
#[error("published repair prefix has no matching content witness for message {message_idx}")]
struct PublishedPrefixMismatch {
    message_idx: u64,
}

/// Finish both publication passes. The caller still owns the canonical read
/// snapshot, run lock, endpoint checks, and the final checkpoint-clear decision.
pub(in crate::indexer::lexical_reconcile) fn publish(
    projection: &CanonicalProjection<'_>,
    index: &mut TantivyIndex,
    binding: &LexicalReconcileCheckpoint,
    index_path: &Path,
    checkpoint_path: &Path,
) -> Result<Outcome> {
    let path = progress_path(checkpoint_path);
    let mut progress = resume(&path, index_path, index, binding)?;
    match publish_from_progress(projection, index, &mut progress, index_path, &path) {
        Err(error) if error.downcast_ref::<PublishedPrefixMismatch>().is_some() => {
            // The prefix check runs before any new batch mutation. Keep the
            // canonical checkpoint and old receipt until a real commit earns
            // fresh progress. A crash here merely repeats this check. Retry
            // once, not indefinitely; ordinary engine/write errors propagate.
            tracing::warn!(
                conversation_id = binding.conversation_id,
                %error,
                "repair cursor skipped unverified documents; restarting both publication passes"
            );
            let mut fresh = Progress::fresh(binding);
            publish_from_progress(projection, index, &mut fresh, index_path, &path)
        }
        outcome => outcome,
    }
}

fn publish_from_progress(
    projection: &CanonicalProjection<'_>,
    index: &mut TantivyIndex,
    progress: &mut Progress,
    index_path: &Path,
    path: &Path,
) -> Result<Outcome> {
    let mut upserted_docs = 0;
    if progress.pass == Pass::Publish {
        upserted_docs = publish_pass(
            projection,
            index,
            progress,
            index_path,
            path,
            &mut || Ok(()),
        )?;
        progress.pass = Pass::Replay;
        progress.completed_docs = 0;
        progress.prefix_blake3 =
            prefix_digest(&ProjectionFingerprint::new(progress.binding.expected_docs));
        progress.first_pass_live_docs = Some(index.doc_count()?);
        progress.publication = Some(Publication::read(index_path, index)?);
        progress.save(path)?;
    }
    let first_pass_live_docs = progress
        .first_pass_live_docs
        .context("repair replay lacks first-pass accounting")?;
    publish_pass(
        projection,
        index,
        progress,
        index_path,
        path,
        &mut || Ok(()),
    )?;
    Ok(Outcome {
        upserted_docs,
        first_pass_live_docs,
    })
}

fn publish_pass(
    projection: &CanonicalProjection<'_>,
    index: &mut TantivyIndex,
    progress: &mut Progress,
    index_path: &Path,
    path: &Path,
    after_save: &mut impl FnMut() -> Result<()>,
) -> Result<usize> {
    let resume_docs = progress.completed_docs;
    let resume_digest = progress.prefix_blake3.clone();
    // One native view for the skipped prefix only, released before publishing
    // any suffix. Do not retain a prior generation through all later commits.
    let mut published_prefix = if resume_docs == 0 {
        None
    } else {
        Some(PublishedSnapshot::open(index_path)?)
    };
    let mut first_mismatch = None;
    let mut fingerprint = ProjectionFingerprint::new(progress.binding.expected_docs);
    let mut checked_prefix = false;
    let mut upserted_docs = 0usize;
    let summary = projection.visit(|docs| {
        let skip = resume_docs
            .saturating_sub(fingerprint.observed_docs)
            .min(docs.len());
        fingerprint.update(&docs[..skip])?;
        if let Some(published) = published_prefix.as_ref() {
            for document in &docs[..skip] {
                if !published.verify_content(document)? && first_mismatch.is_none() {
                    first_mismatch = Some(document.msg_idx);
                }
            }
        }
        if !checked_prefix && fingerprint.observed_docs == resume_docs {
            ensure!(
                prefix_digest(&fingerprint) == resume_digest,
                "canonical repair prefix differs from the saved cursor; recovery retained"
            );
            // Establish the canonical prefix first. An invalid digest plus a
            // stale document must not turn malformed evidence into permission
            // to overwrite either the index or its recovery receipt.
            if let Some(message_idx) = first_mismatch {
                return Err(PublishedPrefixMismatch { message_idx }.into());
            }
            checked_prefix = true;
            published_prefix = None;
        }
        let remaining = &docs[skip..];
        if remaining.is_empty() {
            return Ok(());
        }
        ensure!(checked_prefix, "repair cursor has not been verified");
        fingerprint.update(remaining)?;
        upserted_docs = upserted_docs
            .checked_add(index.upsert_prebuilt_documents_slice(remaining)?)
            .context("repair upsert count overflow")?;
        index.commit()?;
        // Only successful durable commits earn cursor advancement. Failure of
        // this write leaves the main recovery checkpoint intact; manifest
        // mismatch on the next run invalidates the older cursor safely.
        progress.completed_docs = fingerprint.observed_docs;
        progress.prefix_blake3 = prefix_digest(&fingerprint);
        progress.publication = Some(Publication::read(index_path, index)?);
        progress.save(path)?;
        after_save()?;
        Ok(())
    })?;
    ensure!(
        checked_prefix,
        "repair stream ended before its saved cursor"
    );
    let digest = fingerprint.finish()?;
    ensure!(
        summary.matches_checkpoint(&progress.binding)
            && progress.binding.projection_blake3.as_deref() == Some(digest.as_str()),
        "canonical lexical projection changed during resumed replay; recovery retained"
    );
    Ok(upserted_docs)
}

pub(in crate::indexer::lexical_reconcile) fn clear(checkpoint_path: &Path) -> Result<()> {
    let path = progress_path(checkpoint_path);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("clearing repair progress {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::LexicalRebuildPacketProvenance;
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use crate::storage::sqlite::{FrankenStorage, LexicalRebuildConversationRow};
    use anyhow::bail;
    use frankensearch::quill::cass::CassDocument;
    use std::collections::HashMap;

    struct Fixture {
        temp: tempfile::TempDir,
        storage: FrankenStorage,
        row: LexicalRebuildConversationRow,
        provenance: LexicalRebuildPacketProvenance,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
            let agent = storage.ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })?;
            let conversation = Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some("durable-repair".into()),
                title: Some("durable repair".into()),
                source_path: temp.path().join("source.jsonl"),
                started_at: Some(1_700_000_000_000),
                ended_at: Some(1_700_000_000_006),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: (0..7)
                    .map(|idx| Message {
                        id: None,
                        idx: idx * 3,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_000 + idx),
                        content: format!("meridian durable evidence {idx} unicode λ"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: "local".into(),
                origin_host: None,
            };
            let id = storage
                .insert_conversation_tree(agent, None, &conversation)?
                .conversation_id;
            let (agents, workspaces) = storage.build_lexical_rebuild_lookups()?;
            let row = storage
                .list_conversations_for_lexical_rebuild_after_id(1, id - 1, &agents, &workspaces)?
                .into_iter()
                .next()
                .context("missing fixture conversation")?;
            let (provenance, _) = crate::indexer::lexical_rebuild_packet_provenance_from_canonical(
                &row,
                &HashMap::new(),
            );
            Ok(Self {
                temp,
                storage,
                row,
                provenance,
            })
        }

        fn projection(&self, max_messages: usize) -> CanonicalProjection<'_> {
            CanonicalProjection {
                storage: &self.storage,
                row: &self.row,
                provenance: &self.provenance,
                conversation_id: self.row.id.expect("fixture has canonical identity"),
                max_content_bytes: 1024,
                max_messages,
            }
        }

        fn binding(&self) -> Result<(LexicalReconcileCheckpoint, Vec<CassDocument>)> {
            let mut docs = Vec::new();
            let summary = self.projection(2).visit(|batch| {
                docs.extend_from_slice(batch);
                Ok(())
            })?;
            let mut fingerprint = ProjectionFingerprint::new(docs.len());
            fingerprint.update(&docs)?;
            let binding = LexicalReconcileCheckpoint {
                version: super::super::VERSION,
                conversation_id: self.row.id.context("fixture identity")?,
                source_id: self.row.source_id.clone(),
                source_path: self.row.source_path.to_string_lossy().into_owned(),
                message_count: summary.message_count,
                max_message_idx: summary.max_message_idx,
                content_bytes: summary.content_bytes,
                expected_docs: summary.expected_docs,
                projection_blake3: Some(fingerprint.finish()?),
                started_at_ms: 123,
                attempt: 1,
            };
            assert_eq!(docs.len(), 7);
            Ok((binding, docs))
        }

        fn index_path(&self) -> PathBuf {
            self.temp.path().join("index")
        }
    }

    /// Deliberately claim a canonical prefix for a separately constructed
    /// real index. This models a semantically stale receipt, not a mock engine.
    fn claimed_cursor(
        binding: &LexicalReconcileCheckpoint,
        docs: &[CassDocument],
        index: &TantivyIndex,
        index_path: &Path,
        path: &Path,
        pass: Pass,
        completed_docs: usize,
    ) -> Result<Progress> {
        let mut fingerprint = ProjectionFingerprint::new(binding.expected_docs);
        fingerprint.update(&docs[..completed_docs])?;
        let progress = Progress {
            version: VERSION,
            binding: binding.clone(),
            pass,
            completed_docs,
            prefix_blake3: prefix_digest(&fingerprint),
            first_pass_live_docs: (pass == Pass::Replay).then_some(index.doc_count()?),
            publication: Some(Publication::read(index_path, index)?),
        };
        progress.save(path)?;
        Ok(progress)
    }

    #[test]
    #[serial_test::serial]
    fn stale_published_prefix_restarts_both_passes_without_losing_other_documents() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, docs) = fixture.binding()?;
        for missing in [false, true] {
            for pass in [Pass::Publish, Pass::Replay] {
                for completed in [2, docs.len()] {
                    let index_path = fixture
                        .temp
                        .path()
                        .join(format!("stale-{missing}-{pass:?}-{completed}"));
                    let checkpoint_path = index_path.join(".lexical-reconcile-1.json");
                    let path = progress_path(&checkpoint_path);
                    let mut actual = docs.clone();
                    if missing {
                        // Preserve count while removing the expected identity.
                        actual[1].conversation_id = Some(binding.conversation_id + 100);
                    } else {
                        actual[1].content = "meridian stale interior content".into();
                    }
                    let mut index = TantivyIndex::open_or_create(&index_path)?;
                    index.add_prebuilt_documents_slice(&actual)?;
                    index.commit()?;
                    let before = PublishedSnapshot::open(&index_path)?;
                    assert!(before.verify_content(&docs[0])?);
                    assert!(before.verify_content(&docs[6])?);
                    assert!(!before.verify_content(&docs[1])?);
                    drop(before);
                    crate::indexer::write_json_pretty_atomically(&checkpoint_path, &binding)?;
                    let recovery = std::fs::read(&checkpoint_path)?;
                    claimed_cursor(&binding, &docs, &index, &index_path, &path, pass, completed)?;
                    // Establish that all OLD resume guards accept this receipt.
                    // Only the native content witness can reject its skip.
                    assert_eq!(
                        resume(&path, &index_path, &index, &binding)?.completed_docs,
                        completed
                    );
                    let outcome = publish(
                        &fixture.projection(3),
                        &mut index,
                        &binding,
                        &index_path,
                        &checkpoint_path,
                    )?;
                    assert_eq!(outcome.upserted_docs, docs.len());
                    let expected_total = docs.len() as u64 + u64::from(missing);
                    assert_eq!(outcome.first_pass_live_docs, expected_total);
                    assert_eq!(index.doc_count()?, expected_total);
                    let after = PublishedSnapshot::open(&index_path)?;
                    assert_eq!(
                        fixture.projection(2).verify_published(&after, &binding)?,
                        docs.len()
                    );
                    if missing {
                        assert!(after.verify_content(&actual[1])?, "foreign row survives");
                    }
                    drop(after);
                    assert_eq!(std::fs::read(&checkpoint_path)?, recovery);
                    let final_progress = load(&path)?.context("completed replacement replay")?;
                    assert_eq!(final_progress.pass, Pass::Replay);
                    assert_eq!(final_progress.completed_docs, docs.len());
                    // Once the actual source is repaired, another retry is a
                    // write-free verification, not a permanent restart loop.
                    let publication = Publication::read(&index_path, &index)?;
                    let receipt = std::fs::read(&path)?;
                    let retry = publish(
                        &fixture.projection(1),
                        &mut index,
                        &binding,
                        &index_path,
                        &checkpoint_path,
                    )?;
                    assert_eq!(retry.upserted_docs, 0);
                    assert_eq!(Publication::read(&index_path, &index)?, publication);
                    assert_eq!(std::fs::read(&path)?, receipt);
                }
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn invalid_canonical_prefix_cannot_authorize_a_stale_document_restart() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, docs) = fixture.binding()?;
        let index_path = fixture.index_path();
        let checkpoint_path = index_path.join(".lexical-reconcile-1.json");
        let path = progress_path(&checkpoint_path);
        let mut actual = docs.clone();
        actual[1].content = "meridian wrong published content".into();
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&actual)?;
        index.commit()?;
        let mut progress = claimed_cursor(
            &binding,
            &docs,
            &index,
            &index_path,
            &path,
            Pass::Replay,
            docs.len(),
        )?;
        progress.prefix_blake3 = "0".repeat(64);
        progress.save(&path)?;
        let receipt = std::fs::read(&path)?;
        let publication = Publication::read(&index_path, &index)?;
        let error = publish(
            &fixture.projection(2),
            &mut index,
            &binding,
            &index_path,
            &checkpoint_path,
        )
        .err()
        .context("malformed canonical evidence must be refused")?;
        assert!(error.to_string().contains("prefix differs"));
        assert!(error.downcast_ref::<PublishedPrefixMismatch>().is_none());
        assert_eq!(Publication::read(&index_path, &index)?, publication);
        assert_eq!(std::fs::read(&path)?, receipt);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn unreadable_native_publication_never_triggers_the_content_restart() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, docs) = fixture.binding()?;
        let index_path = fixture.index_path();
        let checkpoint_path = index_path.join(".lexical-reconcile-1.json");
        let path = progress_path(&checkpoint_path);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        claimed_cursor(
            &binding,
            &docs,
            &index,
            &index_path,
            &path,
            Pass::Replay,
            docs.len(),
        )?;
        let receipt = std::fs::read(&path)?;
        let corrupt = b"invalid native manifest";
        std::fs::write(index_path.join("MANIFEST"), corrupt)?;
        std::fs::write(index_path.join("MANIFEST.prev"), corrupt)?;
        let error = publish(
            &fixture.projection(2),
            &mut index,
            &binding,
            &index_path,
            &checkpoint_path,
        )
        .err()
        .context("native admission must refuse corrupt manifests")?;
        assert!(error.downcast_ref::<PublishedPrefixMismatch>().is_none());
        assert_eq!(std::fs::read(index_path.join("MANIFEST"))?, corrupt);
        assert_eq!(std::fs::read(index_path.join("MANIFEST.prev"))?, corrupt);
        assert_eq!(std::fs::read(&path)?, receipt);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn interrupted_publication_resumes_only_the_suffix_across_new_batch_boundaries() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, docs) = fixture.binding()?;
        let index_path = fixture.index_path();
        let checkpoint_path = index_path.join(".lexical-reconcile-1.json");
        let path = progress_path(&checkpoint_path);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        crate::indexer::write_json_pretty_atomically(&checkpoint_path, &binding)?;
        let checkpoint_bytes = std::fs::read(&checkpoint_path)?;
        index.add_prebuilt_documents_slice(&docs[4..])?;
        index.commit()?;
        let mut progress = Progress::fresh(&binding);
        let error = publish_pass(
            &fixture.projection(2),
            &mut index,
            &mut progress,
            &index_path,
            &path,
            &mut || bail!("interrupted after durable cursor"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("interrupted after durable cursor")
        );
        assert_eq!(load(&path)?.context("saved cursor")?.completed_docs, 2);
        drop(index);

        let mut index = TantivyIndex::open_or_create(&index_path)?;
        let mut resumed = resume(&path, &index_path, &index, &binding)?;
        assert_eq!(resumed.completed_docs, 2);
        // Cursor 2 is INSIDE the new three-row batch, not on a batch boundary.
        let written = publish_pass(
            &fixture.projection(3),
            &mut index,
            &mut resumed,
            &index_path,
            &path,
            &mut || Ok(()),
        )?;
        assert_eq!(
            written, 5,
            "already published prefix must not be resubmitted"
        );
        assert_eq!(index.doc_count()?, 7);
        assert_eq!(load(&path)?.context("completed cursor")?.completed_docs, 7);
        assert_eq!(std::fs::read(&checkpoint_path)?, checkpoint_bytes);
        let reader = index.reader()?;
        for doc in &docs {
            assert!(super::super::super::canary::verify(
                &reader,
                doc,
                Some("meridian")
            )?);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn interrupted_replay_never_repeats_the_completed_first_pass() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, _) = fixture.binding()?;
        let index_path = fixture.index_path();
        let checkpoint_path = index_path.join(".lexical-reconcile-1.json");
        let path = progress_path(&checkpoint_path);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        let mut progress = Progress::fresh(&binding);
        publish_pass(
            &fixture.projection(2),
            &mut index,
            &mut progress,
            &index_path,
            &path,
            &mut || Ok(()),
        )?;
        progress.pass = Pass::Replay;
        progress.completed_docs = 0;
        progress.prefix_blake3 = prefix_digest(&ProjectionFingerprint::new(binding.expected_docs));
        progress.first_pass_live_docs = Some(index.doc_count()?);
        progress.save(&path)?;
        assert!(
            publish_pass(
                &fixture.projection(2),
                &mut index,
                &mut progress,
                &index_path,
                &path,
                &mut || bail!("replay interrupted")
            )
            .is_err()
        );
        drop(index);

        let mut index = TantivyIndex::open_or_create(&index_path)?;
        let outcome = publish(
            &fixture.projection(3),
            &mut index,
            &binding,
            &index_path,
            &checkpoint_path,
        )?;
        assert_eq!(
            outcome.upserted_docs, 0,
            "completed first pass must stay completed"
        );
        assert_eq!(outcome.first_pass_live_docs, 7);
        assert_eq!(index.doc_count()?, 7);
        let finished = load(&path)?.context("finished replay")?;
        assert_eq!(finished.pass, Pass::Replay);
        assert_eq!(finished.completed_docs, 7);
        // A completed replay still returns to the caller for endpoint checks.
        // Re-entering it must make no publication or progress-file changes.
        let before = Publication::read(&index_path, &index)?;
        let progress_bytes = std::fs::read(&path)?;
        publish(
            &fixture.projection(1),
            &mut index,
            &binding,
            &index_path,
            &checkpoint_path,
        )?;
        assert_eq!(Publication::read(&index_path, &index)?, before);
        assert_eq!(std::fs::read(&path)?, progress_bytes);
        clear(&checkpoint_path)?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn publication_between_commit_and_receipt_forces_safe_replay() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, docs) = fixture.binding()?;
        let index_path = fixture.index_path();
        let path = index_path.join("cursor.json");
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        let mut progress = Progress::fresh(&binding);
        assert!(
            publish_pass(
                &fixture.projection(2),
                &mut index,
                &mut progress,
                &index_path,
                &path,
                &mut || bail!("stop")
            )
            .is_err()
        );
        let saved = std::fs::read(&path)?;
        // Simulate a durable next batch whose receipt was not written.
        index.upsert_prebuilt_documents_slice(&docs[2..4])?;
        index.commit()?;
        drop(index);
        let index = TantivyIndex::open_or_create(&index_path)?;
        let fresh = resume(&path, &index_path, &index, &binding)?;
        assert_eq!(fresh.pass, Pass::Publish);
        assert_eq!(fresh.completed_docs, 0);
        assert!(fresh.publication.is_none());
        assert_eq!(
            std::fs::read(&path)?,
            saved,
            "inspection does not rewrite evidence"
        );
        let mut changed_binding = binding.clone();
        changed_binding.projection_blake3 = Some("f".repeat(64));
        assert_eq!(
            resume(&path, &index_path, &index, &changed_binding)?.completed_docs,
            0
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn wrong_prefix_is_rejected_before_any_more_index_mutation() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, _) = fixture.binding()?;
        let index_path = fixture.index_path();
        let path = index_path.join("cursor.json");
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        let mut progress = Progress::fresh(&binding);
        assert!(
            publish_pass(
                &fixture.projection(2),
                &mut index,
                &mut progress,
                &index_path,
                &path,
                &mut || bail!("stop")
            )
            .is_err()
        );
        progress.prefix_blake3 = "0".repeat(64);
        progress.save(&path)?;
        let before = Publication::read(&index_path, &index)?;
        let saved = std::fs::read(&path)?;
        let mut resumed = resume(&path, &index_path, &index, &binding)?;
        let error = publish_pass(
            &fixture.projection(3),
            &mut index,
            &mut resumed,
            &index_path,
            &path,
            &mut || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("prefix differs"));
        assert_eq!(Publication::read(&index_path, &index)?, before);
        assert_eq!(std::fs::read(&path)?, saved);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn failed_cursor_persistence_never_claims_a_completed_pass() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, _) = fixture.binding()?;
        let index_path = fixture.index_path();
        let path = index_path.join("cursor.json");
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        std::fs::create_dir(&path)?;
        let mut progress = Progress::fresh(&binding);
        let result = publish_pass(
            &fixture.projection(2),
            &mut index,
            &mut progress,
            &index_path,
            &path,
            &mut || bail!("must not observe an unpersisted cursor"),
        );
        assert!(result.is_err());
        assert_eq!(
            index.doc_count()?,
            2,
            "commit precedes the failed progress write"
        );
        assert!(path.is_dir());
        std::fs::rename(&path, index_path.join("retained-obstruction"))?;
        assert_eq!(
            resume(&path, &index_path, &index, &binding)?.completed_docs,
            0
        );
        Ok(())
    }

    #[test]
    fn malformed_progress_and_nonregular_state_are_retained() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("cursor.json");
        for bytes in [
            b"{incomplete".to_vec(),
            vec![b' '; MAX_PROGRESS_BYTES as usize + 1],
        ] {
            std::fs::write(&path, &bytes)?;
            assert!(load(&path).is_err());
            assert_eq!(std::fs::read(&path)?, bytes);
        }
        assert!(load(temp.path()).is_err());
        #[cfg(unix)]
        {
            let link = temp.path().join("link.json");
            std::os::unix::fs::symlink(&path, &link)?;
            assert!(load(&link).is_err());
            assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn unsupported_and_inconsistent_cursor_states_are_rejected() -> Result<()> {
        let fixture = Fixture::new()?;
        let (binding, _) = fixture.binding()?;
        let index_path = fixture.index_path();
        let path = index_path.join("cursor.json");
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        let mut good = Progress::fresh(&binding);
        assert!(
            publish_pass(
                &fixture.projection(2),
                &mut index,
                &mut good,
                &index_path,
                &path,
                &mut || bail!("stop")
            )
            .is_err()
        );
        for case in 0..7 {
            let mut bad = good.clone();
            match case {
                0 => bad.version += 1,
                1 => bad.completed_docs = binding.expected_docs + 1,
                2 => bad.prefix_blake3 = "invalid".into(),
                3 => bad.publication = None,
                4 => bad.pass = Pass::Replay,
                5 => bad.first_pass_live_docs = Some(7),
                _ => bad.binding.projection_blake3 = None,
            }
            let bytes = serde_json::to_vec(&bad)?;
            std::fs::write(&path, &bytes)?;
            assert!(load(&path).is_err(), "case {case}");
            assert_eq!(std::fs::read(&path)?, bytes);
        }
        Ok(())
    }
}
