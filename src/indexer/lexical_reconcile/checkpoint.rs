//! Bind a recovery attempt to its complete lexical projection, not row counts.

use super::LexicalReconcileCheckpoint;
use anyhow::{Result, ensure};
use frankensearch::quill::cass::CassDocument;

pub(super) const VERSION: u32 = 2;

/// Hash every projected field in order without copying large message bodies.
/// Length prefixes and option tags make the encoding unambiguous; fixed-width
/// little-endian lengths keep it independent of the host's pointer width.
pub(super) fn projection_fingerprint(docs: &[CassDocument]) -> String {
    fn text(hasher: &mut blake3::Hasher, value: &str) {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    fn optional_i64(hasher: &mut blake3::Hasher, value: Option<i64>) {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(&value.to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    fn optional_text(hasher: &mut blake3::Hasher, value: Option<&str>) {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                text(hasher, value);
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cass-lexical-reconcile-projection-v2\0");
    hasher.update(&(docs.len() as u64).to_le_bytes());
    for doc in docs {
        for value in [
            &doc.content,
            &doc.agent,
            &doc.source_path,
            &doc.source_id,
            &doc.origin_kind,
        ] {
            text(&mut hasher, value);
        }
        for value in [
            doc.title.as_deref(),
            doc.workspace.as_deref(),
            doc.workspace_original.as_deref(),
            doc.origin_host.as_deref(),
        ] {
            optional_text(&mut hasher, value);
        }
        optional_i64(&mut hasher, doc.created_at);
        optional_i64(&mut hasher, doc.conversation_id);
        hasher.update(&doc.msg_idx.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn valid_digest(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Return the next durable checkpoint BEFORE any index mutation. Version one
/// had no content witness: upgrade it only by rebinding the current projection
/// and replaying the entire source. Never infer prior completion from it.
pub(super) fn resume(
    mut current: LexicalReconcileCheckpoint,
    previous: Option<LexicalReconcileCheckpoint>,
) -> Result<LexicalReconcileCheckpoint> {
    ensure!(
        current.version == VERSION
            && current.conversation_id > 0
            && valid_digest(current.projection_blake3.as_deref()),
        "invalid current lexical reconcile binding"
    );
    let Some(previous) = previous else {
        return Ok(current);
    };
    ensure!(
        matches!(previous.version, 1 | VERSION),
        "unsupported lexical reconcile checkpoint version {}; checkpoint retained",
        previous.version
    );
    ensure!(
        previous.conversation_id == current.conversation_id
            && previous.source_id == current.source_id
            && previous.source_path == current.source_path,
        "lexical reconcile checkpoint belongs to a different source identity; checkpoint retained"
    );
    ensure!(
        previous.message_count == current.message_count
            && previous.max_message_idx == current.max_message_idx
            && previous.content_bytes == current.content_bytes
            && previous.expected_docs == current.expected_docs,
        "canonical lexical projection changed shape since checkpoint; checkpoint retained"
    );
    if previous.version == VERSION {
        ensure!(
            valid_digest(previous.projection_blake3.as_deref()),
            "lexical reconcile checkpoint has no valid projection digest; checkpoint retained"
        );
        ensure!(
            previous.projection_blake3 == current.projection_blake3,
            "canonical lexical content or metadata changed since checkpoint; checkpoint retained"
        );
    } else {
        ensure!(
            previous.projection_blake3.is_none(),
            "legacy lexical reconcile checkpoint has an unexpected digest; checkpoint retained"
        );
        tracing::warn!("rebinding legacy lexical checkpoint before a complete source replay");
    }
    current.attempt = previous
        .attempt
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("lexical reconcile attempt counter exhausted"))?;
    current.started_at_ms = previous.started_at_ms;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> CassDocument {
        CassDocument {
            title: Some("title".into()),
            content: "alpha".into(),
            agent: "codex".into(),
            workspace: Some("/work".into()),
            workspace_original: Some("/Work".into()),
            source_path: "/source".into(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            created_at: Some(42),
            conversation_id: Some(7),
            msg_idx: 3,
        }
    }
    fn checkpoint(docs: &[CassDocument]) -> LexicalReconcileCheckpoint {
        LexicalReconcileCheckpoint {
            version: VERSION,
            conversation_id: 7,
            source_id: "local".into(),
            source_path: "/source".into(),
            message_count: docs.len(),
            max_message_idx: 3,
            content_bytes: docs.iter().map(|doc| doc.content.len()).sum(),
            expected_docs: docs.len(),
            projection_blake3: Some(projection_fingerprint(docs)),
            started_at_ms: 100,
            attempt: 1,
        }
    }

    #[test]
    fn same_length_rewrites_and_every_projected_field_change_the_binding() {
        let original = document();
        let expected = checkpoint(std::slice::from_ref(&original));
        for field in 0..12 {
            let mut changed = original.clone();
            match field {
                0 => changed.title = Some("other".into()),
                1 => changed.content = "bravo".into(),
                2 => changed.agent = "other".into(),
                3 => changed.workspace = Some("/else".into()),
                4 => changed.source_path = "/other".into(),
                5 => changed.source_id = "other".into(),
                6 => changed.origin_kind = "remote".into(),
                7 => changed.origin_host = Some("host".into()),
                8 => changed.created_at = None,
                9 => changed.conversation_id = Some(8),
                10 => changed.msg_idx = 4,
                _ => changed.workspace_original = Some("/Other".into()),
            }
            let current = checkpoint(&[changed]);
            assert_ne!(
                current.projection_blake3, expected.projection_blake3,
                "field {field}"
            );
            assert!(
                resume(current, Some(expected.clone())).is_err(),
                "field {field}"
            );
        }
    }

    #[test]
    fn fingerprint_framing_preserves_boundaries_options_and_order() {
        let mut a = document();
        a.title = Some("ab".into());
        a.content = "c".into();
        a.created_at = None;
        let mut b = a.clone();
        b.title = Some("a".into());
        b.content = "bc".into();
        assert_ne!(
            projection_fingerprint(&[a.clone()]),
            projection_fingerprint(&[b.clone()])
        );
        b = a.clone();
        b.created_at = Some(0);
        assert_ne!(
            projection_fingerprint(&[a.clone()]),
            projection_fingerprint(&[b.clone()])
        );
        assert_ne!(
            projection_fingerprint(&[a.clone(), b.clone()]),
            projection_fingerprint(&[b, a])
        );
        assert_ne!(
            projection_fingerprint(&[]),
            projection_fingerprint(&[document()])
        );
    }

    #[test]
    fn retries_keep_start_time_and_advance_attempt_without_rebinding() -> Result<()> {
        let mut previous = checkpoint(&[document()]);
        previous.attempt = 9;
        let mut current = previous.clone();
        current.started_at_ms = 999;
        let next = resume(current, Some(previous.clone()))?;
        assert_eq!(next.attempt, 10);
        assert_eq!(next.started_at_ms, 100);
        assert_eq!(next.projection_blake3, previous.projection_blake3);
        Ok(())
    }

    #[test]
    fn unsupported_misbound_and_malformed_checkpoints_cannot_resume() {
        let current = checkpoint(&[document()]);
        for case in 0..11 {
            let mut previous = current.clone();
            match case {
                0 => previous.version = 3,
                1 => previous.conversation_id = 8,
                2 => previous.source_id = "foreign".into(),
                3 => previous.source_path = "/foreign".into(),
                4 => previous.message_count += 1,
                5 => previous.max_message_idx += 1,
                6 => previous.content_bytes += 1,
                7 => previous.expected_docs += 1,
                8 => previous.projection_blake3 = None,
                9 => previous.projection_blake3 = Some("G".repeat(64)),
                _ => previous.attempt = u32::MAX,
            }
            assert!(
                resume(current.clone(), Some(previous)).is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn legacy_checkpoint_is_explicitly_upgraded_before_full_replay() -> Result<()> {
        let current = checkpoint(&[document()]);
        let mut previous = current.clone();
        previous.version = 1;
        previous.projection_blake3 = None;
        let value = serde_json::to_value(&previous)?;
        let mut without_digest = value.as_object().unwrap().clone();
        without_digest.remove("projection_blake3");
        let old: LexicalReconcileCheckpoint = serde_json::from_value(without_digest.into())?;
        let next = resume(current.clone(), Some(old))?;
        assert_eq!(next.version, VERSION);
        assert_eq!(next.projection_blake3, current.projection_blake3);
        assert_eq!(next.attempt, 2);
        Ok(())
    }

    #[test]
    fn oversized_and_malformed_checkpoints_are_retained() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("checkpoint.json");
        for bytes in [
            vec![b' '; super::super::CHECKPOINT_MAX_BYTES as usize + 1],
            b"{unfinished".to_vec(),
        ] {
            std::fs::write(&path, &bytes)?;
            assert!(super::super::load_checkpoint(&path).is_err());
            assert_eq!(std::fs::read(&path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn invalid_conversation_identity_is_rejected_before_archive_or_lock_access() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("not-created");
        for id in [0, -1, i64::MIN] {
            let error = super::super::run_lexical_conversation_reconcile(
                &data,
                &data.join("archive.db"),
                id,
            )
            .unwrap_err();
            assert!(error.to_string().contains("must be positive"));
            assert!(!data.exists());
        }
        Ok(())
    }
}
