//! Keep canonical metadata and every bounded replay on one read snapshot.

use anyhow::{Context, Result};

use crate::franken_sync::Connection;

pub(super) struct CanonicalSnapshot<'a> {
    connection: &'a Connection,
    active: bool,
}

impl<'a> CanonicalSnapshot<'a> {
    pub(super) fn begin(connection: &'a Connection) -> Result<Self> {
        connection
            .execute("BEGIN DEFERRED")
            .context("begin canonical lexical reconcile snapshot")?;
        Ok(Self {
            connection,
            active: true,
        })
    }

    pub(super) fn release(mut self) -> Result<()> {
        self.connection
            .execute("ROLLBACK")
            .context("release canonical lexical reconcile snapshot")?;
        self.active = false;
        Ok(())
    }
}

impl Drop for CanonicalSnapshot<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.connection.execute("ROLLBACK")
        {
            tracing::warn!(%error, "failed to release canonical lexical reconcile snapshot");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::franken_sync::compat::RowExt;
    use crate::indexer::lexical_reconcile::{
        LexicalReconcileCheckpoint, checkpoint, lexical_reconcile_checkpoint_path,
        run_lexical_conversation_reconcile,
    };
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use crate::search::tantivy::{TantivyIndex, expected_index_dir};
    use crate::storage::sqlite::FrankenStorage;
    use frankensearch::quill::cass::CassDocument;
    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};

    fn body(connection: &Connection) -> Result<String> {
        Ok(connection
            .query_row("SELECT body FROM snapshot_probe WHERE id = 1")?
            .get_typed(0)?)
    }

    #[test]
    fn read_snapshot_does_not_mix_same_length_rewrites_between_passes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp
            .path()
            .join("snapshot.db")
            .to_string_lossy()
            .into_owned();
        let writer = Connection::open(path.clone())?;
        writer.execute("PRAGMA journal_mode=WAL")?;
        writer.execute("CREATE TABLE snapshot_probe (id INTEGER PRIMARY KEY, body TEXT)")?;
        writer.execute("INSERT INTO snapshot_probe VALUES (1, 'alpha')")?;
        let reader = Connection::open_schema_only_with_wal_index_recovery(path)?;
        let snapshot = CanonicalSnapshot::begin(&reader)?;
        assert_eq!(body(&reader)?, "alpha");
        writer.execute("UPDATE snapshot_probe SET body = 'bravo' WHERE id = 1")?;
        assert_eq!(body(&writer)?, "bravo");
        for _ in 0..4 {
            assert_eq!(
                body(&reader)?,
                "alpha",
                "every replay uses the bound snapshot"
            );
        }
        snapshot.release()?;
        assert_eq!(
            body(&reader)?,
            "bravo",
            "release admits the next archive view"
        );
        Ok(())
    }

    #[test]
    fn snapshot_cleanup_preserves_outer_transactions_and_releases_on_error() -> Result<()> {
        let connection = Connection::open(":memory:")?;
        let outer = CanonicalSnapshot::begin(&connection)?;
        assert!(CanonicalSnapshot::begin(&connection).is_err());
        // Failed construction must not roll back someone else's transaction.
        outer.release()?;
        let failure: Result<()> = (|| {
            let _snapshot = CanonicalSnapshot::begin(&connection)?;
            anyhow::bail!("replay consumer failed")
        })();
        assert!(
            failure
                .unwrap_err()
                .to_string()
                .contains("replay consumer failed")
        );
        CanonicalSnapshot::begin(&connection)?.release()?;
        Ok(())
    }

    fn seed(
        data_dir: &Path,
    ) -> Result<(
        FrankenStorage,
        Vec<CassDocument>,
        LexicalReconcileCheckpoint,
    )> {
        let storage = FrankenStorage::open(&data_dir.join("archive.db"))?;
        let agent_id = storage.ensure_agent(&Agent {
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
            external_id: Some("snapshot-reconcile".into()),
            title: Some("snapshot reconcile".into()),
            source_path: data_dir.join("source.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_005),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: (0..6)
                .map(|idx| Message {
                    id: None,
                    idx,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_000 + idx),
                    content: format!("meridian alpha endpoint {idx}"),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                })
                .collect(),
            source_id: "local".into(),
            origin_host: None,
        };
        let id = storage
            .insert_conversation_tree(agent_id, None, &conversation)?
            .conversation_id;
        let (agents, workspaces) = storage.build_lexical_rebuild_lookups()?;
        let row = storage
            .list_conversations_for_lexical_rebuild_after_id(1, id - 1, &agents, &workspaces)?
            .into_iter()
            .next()
            .context("missing canonical test conversation")?;
        let (provenance, _) =
            crate::indexer::lexical_rebuild_packet_provenance_from_canonical(&row, &HashMap::new());
        let messages = storage.fetch_messages_for_lexical_rebuild(id)?;
        let content_bytes = messages.iter().map(|message| message.content.len()).sum();
        let packet = crate::indexer::lexical_rebuild_contract_from_canonical_messages(
            &row,
            &provenance,
            messages,
        );
        let docs = TantivyIndex::build_packet_documents(&packet, Some(id));
        assert_eq!(docs.len(), 6);
        let checkpoint = LexicalReconcileCheckpoint {
            version: 2,
            conversation_id: id,
            source_id: row.source_id,
            source_path: row.source_path.to_string_lossy().into_owned(),
            message_count: 6,
            max_message_idx: 5,
            content_bytes,
            expected_docs: 6,
            projection_blake3: Some(checkpoint::projection_fingerprint(&docs)),
            started_at_ms: 123,
            attempt: 4,
        };
        let index_path = expected_index_dir(data_dir);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&docs[3..])?;
        index.commit()?;
        assert_eq!(index.doc_count()?, 3);
        drop(index);
        crate::indexer::write_json_pretty_atomically(
            &lexical_reconcile_checkpoint_path(&index_path, id),
            &checkpoint,
        )?;
        Ok((storage, docs, checkpoint))
    }

    fn canonical_bytes(data_dir: &Path) -> Result<Vec<Option<Vec<u8>>>> {
        ["archive.db", "archive.db-wal"]
            .into_iter()
            .map(|name| match std::fs::read(data_dir.join(name)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            })
            .collect()
    }

    fn tree_bytes(directory: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        let mut result = BTreeMap::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                result.extend(tree_bytes(&entry.path())?);
            } else {
                result.insert(entry.path(), std::fs::read(entry.path())?);
            }
        }
        Ok(result)
    }

    #[test]
    #[serial_test::serial]
    fn public_reconcile_resumes_v2_without_canonical_writes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (storage, _docs, checkpoint) = seed(tmp.path())?;
        drop(storage);
        let before = canonical_bytes(tmp.path())?;
        let report = run_lexical_conversation_reconcile(
            tmp.path(),
            &tmp.path().join("archive.db"),
            checkpoint.conversation_id,
        )?;
        assert_eq!(report.attempt, 5);
        assert_eq!(report.message_count, 6);
        assert_eq!(report.expected_docs, 6);
        assert_eq!(report.doc_count_before, 3);
        assert_eq!(report.doc_count_after, 6);
        assert!(report.converged);
        assert_eq!(report.early_canary_ok, Some(true));
        assert_eq!(report.late_canary_ok, Some(true));
        assert!(report.checkpoint_cleared);
        assert!(
            !lexical_reconcile_checkpoint_path(
                &expected_index_dir(tmp.path()),
                checkpoint.conversation_id,
            )
            .exists()
        );
        assert_eq!(canonical_bytes(tmp.path())?, before);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn same_length_content_change_rejects_resume_before_any_index_mutation() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (storage, _docs, checkpoint) = seed(tmp.path())?;
        // Keep message count, indices and byte count identical; only the
        // complete content witness can distinguish this from a safe resume.
        storage.raw().execute_with_params(
            "UPDATE messages SET content = 'meridian bravo endpoint 0' \
             WHERE conversation_id = ?1 AND idx = 0",
            &[checkpoint.conversation_id.into()],
        )?;
        let messages = storage.fetch_messages_for_lexical_rebuild(checkpoint.conversation_id)?;
        assert_eq!(messages[0].content, "meridian bravo endpoint 0");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.content.len())
                .sum::<usize>(),
            checkpoint.content_bytes
        );
        drop(storage);
        let before_archive = canonical_bytes(tmp.path())?;
        let before_index = tree_bytes(&expected_index_dir(tmp.path()))?;
        let error = run_lexical_conversation_reconcile(
            tmp.path(),
            &tmp.path().join("archive.db"),
            checkpoint.conversation_id,
        )
        .unwrap_err();
        assert!(error.to_string().contains("content or metadata changed"));
        assert_eq!(tree_bytes(&expected_index_dir(tmp.path()))?, before_index);
        assert_eq!(canonical_bytes(tmp.path())?, before_archive);
        Ok(())
    }
}
