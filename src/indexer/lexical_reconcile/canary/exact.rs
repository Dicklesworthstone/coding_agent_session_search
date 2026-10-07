//! Resolve repair endpoints by native IDHASH/IDMAP, never search rank.
//!
//! A single admitted immutable Keeper view supplies identities, tombstones,
//! stored metadata and final live-document accounting. This replaces the
//! final ranked reader; it is not a second full index open for each endpoint.
//! It verifies stored previews and provenance, not all content beyond previews.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use frankensearch::quill::cass::{
    CassConversationKey, CassDerivedColumns, CassDocument, cass_document_identity, field,
};
use frankensearch::quill::{
    CASS_SEMANTIC_SCHEMA, KeeperSnapshot, QuillSearchSnapshot, SnapshotPublisher,
};

pub(in crate::indexer::lexical_reconcile) struct PublishedSnapshot {
    view: Arc<QuillSearchSnapshot>,
}

impl PublishedSnapshot {
    pub(in crate::indexer::lexical_reconcile) fn open(path: &Path) -> Result<Self> {
        // The same native Keeper admission and snapshot composition used by
        // QuillSearchIndex::open_with_schema, without binding a query parser.
        // Only committed durable segments are eligible for repair completion.
        let keeper = KeeperSnapshot::open(path, CASS_SEMANTIC_SCHEMA)
            .context("admitting lexical repair verification snapshot")?;
        let view = SnapshotPublisher::new(Arc::new(keeper), Vec::new())
            .context("binding lexical repair verification snapshot")?
            .load();
        Ok(Self { view })
    }

    pub(in crate::indexer::lexical_reconcile) fn doc_count(&self) -> u64 {
        self.view.live_doc_count()
    }

    pub(in crate::indexer::lexical_reconcile) fn verify(
        &self,
        document: &CassDocument,
    ) -> Result<bool> {
        let identity = cass_document_identity(
            &document.source_id,
            CassConversationKey::for_document(document.as_ref()),
            document.msg_idx,
        );
        // Native resolution checks the full opaque identity, not just its
        // hash, and excludes tombstoned rows. Never reopen/refresh for columns.
        let Some(resolved) = self.view.keeper_snapshot().resolve_document_id(&identity)? else {
            return Ok(false);
        };
        let id = resolved.global_docid;
        let preview = CassDerivedColumns::derive(document.as_ref()).preview;
        for (field, expected) in [
            (field::AGENT, Some(document.agent.as_str())),
            (field::SOURCE_ID, Some(document.source_id.as_str())),
            (field::SOURCE_PATH, Some(document.source_path.as_str())),
            (field::WORKSPACE, document.workspace.as_deref()),
            (field::WORKSPACE_ORIGINAL, document.workspace_original.as_deref()),
            (field::ORIGIN_KIND, Some(document.origin_kind.as_str())),
            (field::ORIGIN_HOST, document.origin_host.as_deref()),
            (field::TITLE, document.title.as_deref()),
            (field::PREVIEW, Some(preview.as_str())),
        ] {
            if self.text(field, id)?.as_deref() != expected {
                return Ok(false);
            }
        }
        Ok(self.number(field::CONVERSATION_ID, id)?.map(i64::from_le_bytes)
            == document.conversation_id
            && self.number(field::MSG_IDX, id)?.map(u64::from_le_bytes)
                == Some(document.msg_idx)
            && self.number(field::CREATED_AT, id)?.map(i64::from_le_bytes)
                == document.created_at)
    }

    fn text(&self, field: u16, id: u32) -> Result<Option<String>> {
        self.view
            .stored_field_value(field, id)?
            .map(String::from_utf8)
            .transpose()
            .context("invalid UTF-8 in lexical repair endpoint metadata")
    }

    fn number(&self, field: u16, id: u32) -> Result<Option<[u8; 8]>> {
        self.view
            .stored_field_value(field, id)?
            .map(|bytes| {
                bytes.try_into().map_err(|_: Vec<u8>| {
                    anyhow::anyhow!("invalid numeric width in lexical repair endpoint metadata")
                })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::tantivy::TantivyIndex;

    fn document(id: i64, content: &str) -> CassDocument {
        CassDocument {
            agent: "codex".into(),
            workspace: Some("/work".into()),
            workspace_original: Some("/Work".into()),
            source_path: format!("/source/{id}"),
            msg_idx: 0,
            created_at: Some(1_700_000_000_000),
            title: Some("source title".into()),
            content: content.into(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(id),
        }
    }

    #[test]
    fn exact_endpoint_beyond_4096_ranked_candidates_still_verifies() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("index");
        let mut index = TantivyIndex::open_or_create(&path)?;
        let target = document(10_000, &format!("common target {}", "padding ".repeat(512)));
        let mut docs: Vec<_> = (1..=4097).map(|id| document(id, "common")).collect();
        docs.push(target.clone());
        index.add_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        let old = index.reader()?;
        // Exercise the incumbent in the SAME invocation: this is a real
        // ranked-candidate failure, not merely a synthetic cursor model.
        let error = super::super::verify_with_budget(&old, &target, Some("common"), 4096)
            .unwrap_err();
        assert!(error.to_string().contains("candidate budget"));
        drop(old);
        let exact = PublishedSnapshot::open(&path)?;
        assert_eq!(exact.doc_count(), 4098);
        assert!(exact.verify(&target)?);
        assert!(!exact.verify(&document(20_000, "common missing"))?);
        Ok(())
    }

    #[test]
    fn exact_endpoint_checks_every_stored_field_and_optional_absence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("index");
        let mut index = TantivyIndex::open_or_create(&path)?;
        let expected = document(42, "common wanted");
        index.add_prebuilt_documents_slice(std::slice::from_ref(&expected))?;
        index.commit()?;
        let exact = PublishedSnapshot::open(&path)?;
        assert!(exact.verify(&expected)?);
        for field in 0..12 {
            let mut wrong = expected.clone();
            match field {
                0 => wrong.conversation_id = Some(43),
                1 => wrong.source_id = "foreign".into(),
                2 => wrong.source_path = "/wrong".into(),
                3 => wrong.msg_idx = 1,
                4 => wrong.created_at = None,
                5 => wrong.agent = "other".into(),
                6 => wrong.workspace = None,
                7 => wrong.workspace_original = None,
                8 => wrong.origin_kind = "remote".into(),
                9 => wrong.origin_host = Some("invented-host".into()),
                10 => wrong.title = None,
                _ => wrong.content = "common different".into(),
            }
            assert!(!exact.verify(&wrong)?, "field {field} must be exact");
        }
        let mut sparse = document(43, "1234 5678");
        sparse.created_at = None;
        sparse.title = None;
        sparse.workspace = None;
        sparse.workspace_original = None;
        index.upsert_prebuilt_documents_slice(std::slice::from_ref(&sparse))?;
        index.commit()?;
        let exact = PublishedSnapshot::open(&path)?;
        assert!(exact.verify(&sparse)?);
        let mut invented = sparse.clone();
        invented.created_at = Some(0);
        assert!(!exact.verify(&invented)?);
        invented = sparse;
        invented.workspace = Some(String::new());
        assert!(!exact.verify(&invented)?);
        Ok(())
    }

    #[test]
    fn exact_endpoint_snapshot_survives_tombstones_without_reopening() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("index");
        let mut index = TantivyIndex::open_or_create(&path)?;
        let original = document(42, "你好 世界");
        let mut replacement = original.clone();
        replacement.content = "x=1; y=2;".into();
        let sibling = document(43, "1234 5678");
        index.add_prebuilt_documents_slice(&[original.clone(), sibling.clone()])?;
        index.commit()?;
        let before = PublishedSnapshot::open(&path)?;
        for _ in 0..2 {
            index.upsert_prebuilt_documents_slice(std::slice::from_ref(&replacement))?;
            index.commit()?;
            let current = PublishedSnapshot::open(&path)?;
            assert_eq!(current.doc_count(), 2);
            assert!(current.verify(&replacement)?);
            assert!(current.verify(&sibling)?);
            assert!(!current.verify(&original)?);
            assert!(before.verify(&original)?);
            assert!(!before.verify(&replacement)?);
            assert_eq!(before.doc_count(), 2);
        }
        Ok(())
    }

    #[test]
    fn exact_endpoint_admission_refuses_missing_or_corrupt_indexes_without_writes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let missing = temp.path().join("missing");
        assert!(PublishedSnapshot::open(&missing).is_err());
        assert!(!missing.exists());
        let corrupt = temp.path().join("corrupt");
        std::fs::create_dir(&corrupt)?;
        let bytes = b"not a valid Quill manifest";
        std::fs::write(corrupt.join("MANIFEST"), bytes)?;
        assert!(PublishedSnapshot::open(&corrupt).is_err());
        assert_eq!(std::fs::read(corrupt.join("MANIFEST"))?, bytes);
        assert_eq!(std::fs::read_dir(&corrupt)?.count(), 1);
        Ok(())
    }
}
