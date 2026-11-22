//! Retained admission for semantic owners. Legacy vectors do not carry a
//! canonical parent proof, so a destructive canonical change revokes their
//! entire generation. Rebuilding another generation never renews this one.

use super::*;
use crate::storage::sqlite::SemanticArchiveState;

#[derive(Debug, thiserror::Error)]
pub enum SemanticIdentityError {
    #[error(
        "semantic-identity-snapshot-invalidated: canonical identities changed; reload semantic artifacts before searching"
    )]
    SnapshotInvalidated,
    #[error(
        "semantic-identity-authority-unavailable: cannot verify retained semantic authority: {detail}"
    )]
    AuthorityUnavailable { detail: String },
}

#[derive(Clone)]
pub(super) struct SemanticAdmissionSnapshot {
    privacy: Vec<PrivacySnapshot>,
    legacy_authorities: Vec<SemanticArchiveState>,
}

impl SemanticAdmissionSnapshot {
    pub(super) fn capture(
        client: &SearchClient,
        artifacts: &[SemanticIndexArtifact],
        quality_artifact: Option<&SemanticIndexArtifact>,
    ) -> Result<Self> {
        let mut privacy = client
            .sqlite_path
            .as_deref()
            .map(PrivacySnapshot::for_database)
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let mut legacy_authorities = Vec::new();
        for artifact in artifacts.iter().chain(quality_artifact) {
            if let Some(admitted) = artifact.privacy_snapshot()
                && !privacy.contains(admitted)
            {
                privacy.push(admitted.clone());
            }
            if let Some(authority) = artifact.legacy_authority()
                && !legacy_authorities.contains(authority)
            {
                legacy_authorities.push(authority.clone());
            }
        }
        let admission = Self {
            privacy,
            legacy_authorities,
        };
        admission.validate(client)?;
        Ok(admission)
    }

    pub(super) fn validate_privacy(&self) -> Result<()> {
        for snapshot in &self.privacy {
            snapshot.validate()?;
        }
        Ok(())
    }

    pub(super) fn validate(&self, client: &SearchClient) -> Result<()> {
        self.validate_privacy()?;
        let result = self.validate_legacy(client);
        // Forget remains the primary typed refusal even when its canonical
        // deletion also revokes legacy identity or makes hydration fail.
        self.validate_privacy()?;
        result
    }

    fn validate_legacy(&self, client: &SearchClient) -> Result<()> {
        if self.legacy_authorities.is_empty() {
            return Ok(());
        }
        let result = (|| -> Result<()> {
            let mut sqlite_guard = client.sqlite_guard()?;
            let result = (|| -> Result<()> {
                let conn = sqlite_guard
                    .as_ref()
                    .ok_or_else(|| anyhow!("semantic search requires database connection"))?;
                conn.verify_canonical_archive()?;
                let mut transaction = SearchReadTransaction::begin(conn)?;
                for authority in &self.legacy_authorities {
                    validate_legacy_authority(&transaction, authority)?;
                }
                transaction.rollback()?;
                conn.verify_canonical_archive()
            })();
            if let Err(error) = &result
                && error.downcast_ref::<SemanticIdentityError>().is_none()
            {
                // Never graft a fresh witness onto an unverified retained
                // worker. A later request may open a new canonical owner.
                if let Some(mut connection) = sqlite_guard.take()
                    && let Err(close_error) = connection.close_without_checkpoint_sync()
                {
                    tracing::warn!(%close_error, "failed to close rejected legacy semantic owner");
                }
            }
            result
        })();
        result.map_err(|error| {
            if error.downcast_ref::<SemanticIdentityError>().is_some() {
                error
            } else {
                SemanticIdentityError::AuthorityUnavailable {
                    detail: format!("{error:#}"),
                }
                .into()
            }
        })
    }
}

/// Read the archive identity and durable invalidation journal together. An
/// append may advance mutation_seq without revoking the retained generation;
/// an identity change stays revoking after its rebuild marker is completed.
pub(super) fn validate_legacy_authority(
    transaction: &SearchReadTransaction<'_>,
    admitted: &SemanticArchiveState,
) -> Result<()> {
    let sequence = i64::try_from(admitted.mutation_seq)?;
    let rows = transaction.query_map_collect(
        "SELECT value, EXISTS(SELECT 1 FROM semantic_conversation_changes
         WHERE invalidation_seq > ?1) FROM meta
         WHERE key = 'semantic_archive_state_v1'",
        &[ParamValue::from(sequence)],
        |row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
    )?;
    let Some((state, invalidated)) = rows.first() else {
        return Err(SemanticIdentityError::SnapshotInvalidated.into());
    };
    let state = SemanticArchiveState::parse(state)?;
    if state.archive_id != admitted.archive_id
        || state.mutation_seq < admitted.mutation_seq
        || *invalidated != 0
    {
        return Err(SemanticIdentityError::SnapshotInvalidated.into());
    }
    Ok(())
}
