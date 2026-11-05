//! A certificate admits historical vectors, not historical message payloads.
//! Every candidate is authenticated and hydrated in one canonical read snapshot.

use super::*;
use crate::search::canonicalize::embedding_passages;
use crate::search::semantic_manifest::SemanticCoverageCertificate;
use crate::search::tantivy::normalized_index_source_id;
use crate::storage::sqlite::{SemanticArchiveState, lookup_semantic_parent_identities_on_async};

type HydratedSemanticHits = Vec<(u64, SearchHit)>;
type HydratedSemanticPage = (HydratedSemanticHits, Option<SemanticCoverageSnapshot>);

#[cfg(all(test, unix))]
#[path = "coverage/tests.rs"]
mod tests;

/// Identity of the canonical database pathname when its hydration owner was
/// opened. Query checks compare only identity so ordinary WAL commits and
/// checkpoints remain valid.
#[derive(Debug)]
pub(super) struct CanonicalDatabaseWitness {
    path: PathBuf,
    handle: same_file::Handle,
}

impl CanonicalDatabaseWitness {
    fn open_handle(path: &Path) -> Result<same_file::Handle> {
        let metadata = std::fs::symlink_metadata(path)
            .context("semantic canonical archive file changed or disappeared")?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("semantic canonical archive must remain a regular non-symlink file");
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .context("semantic canonical archive file changed or disappeared")?;
        if !file.metadata()?.is_file() {
            bail!("semantic canonical archive must remain a regular file");
        }
        Ok(same_file::Handle::from_file(file)?)
    }

    pub(super) fn verify(&self) -> Result<()> {
        if Self::open_handle(&self.path)? != self.handle {
            bail!("semantic canonical archive file changed since the hydration connection opened");
        }
        Ok(())
    }
}

/// Short-lived opening seal, discarded once its readonly connection opens.
/// The pinned dedicated-worker API does not expose its raw descriptor identity.
/// Retained-file and ancestor metadata fence rename-and-restore during opening;
/// checking both path chains also covers stable symlinked parent directories.
pub(super) struct CanonicalDatabaseOpening {
    witness: CanonicalDatabaseWitness,
    canonical_target: PathBuf,
    file_metadata: std::fs::Metadata,
    ancestors: Vec<(PathBuf, std::fs::Metadata)>,
}

impl CanonicalDatabaseOpening {
    pub(super) fn capture(path: &Path) -> Result<Self> {
        let handle = CanonicalDatabaseWitness::open_handle(path)?;
        let file_metadata = handle.as_file().metadata()?;
        let canonical_target = path.canonicalize()?;
        let mut paths = path
            .ancestors()
            .skip(1)
            .chain(canonical_target.ancestors().skip(1))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        paths.sort_unstable();
        paths.dedup();
        let ancestors = paths
            .into_iter()
            .map(|path| std::fs::symlink_metadata(&path).map(|metadata| (path, metadata)))
            .collect::<std::io::Result<Vec<_>>>()?;
        let opening = Self {
            witness: CanonicalDatabaseWitness {
                path: path.to_path_buf(),
                handle,
            },
            canonical_target,
            file_metadata,
            ancestors,
        };
        opening.verify()?;
        Ok(opening)
    }

    fn verify(&self) -> Result<()> {
        if self.witness.path.canonicalize()? != self.canonical_target {
            bail!("semantic canonical archive path changed during hydration connection opening");
        }
        for (path, before) in &self.ancestors {
            crate::search::semantic_coverage::ensure_same_file_metadata(
                before,
                &std::fs::symlink_metadata(path)?,
            )
            .context(
                "semantic canonical archive ancestor changed during hydration connection opening",
            )?;
        }
        crate::search::semantic_coverage::ensure_same_file_metadata(
            &self.file_metadata,
            &self.witness.handle.as_file().metadata()?,
        )?;
        self.witness.verify()
    }

    pub(super) fn finish(self) -> Result<CanonicalDatabaseWitness> {
        self.verify()?;
        Ok(self.witness)
    }
}

struct CurrentPassage {
    message_id: u64,
    content: String,
    created_at: Option<i64>,
    agent_id: Option<i64>,
    workspace_id: Option<i64>,
    source_id: Option<String>,
    origin_host: Option<String>,
    role: String,
    invalidation_seq: i64,
}

impl CurrentPassage {
    fn matches(&self, candidate: &VectorSearchResult) -> bool {
        let Some(identity) = candidate.identity else {
            return false;
        };
        let Some(expected_hash) = identity.content_hash else {
            return false;
        };
        if candidate.message_id != identity.message_id
            || candidate.chunk_idx != identity.chunk_idx
            || identity.message_id != self.message_id
            || identity.agent_id != semantic_doc_component_id_from_db(self.agent_id)
            || identity.workspace_id != semantic_doc_component_id_from_db(self.workspace_id)
            || identity.role
                != role_code_from_str(&crate::model::conversation_packet::normalize_role(
                    &self.role,
                ))
                .unwrap_or(ROLE_USER)
            || identity.created_at_ms != self.created_at.unwrap_or(0)
        {
            return false;
        }
        let source_id = normalized_index_source_id(
            self.source_id.as_deref(),
            None,
            self.origin_host.as_deref(),
        );
        if identity.source_id != crc32fast::hash(source_id.as_bytes()) {
            return false;
        }
        let passages = embedding_passages(&self.content);
        let Some(passage) = passages.get(usize::from(identity.chunk_idx)) else {
            return false;
        };
        let canonical = canonicalize_for_embedding(passage);
        !canonical.is_empty() && content_hash(&canonical) == expected_hash
    }
}

fn coverage_in_transaction(
    transaction: &SearchReadTransaction<'_>,
    certificate: &SemanticCoverageCertificate,
) -> Result<(SemanticCoverageSnapshot, bool)> {
    let rows = transaction.query_map_collect(
        "SELECT (SELECT value FROM meta WHERE key = 'semantic_archive_state_v1'),
                COALESCE((SELECT MAX(id) FROM messages), 0),
                COALESCE((SELECT invalidation_seq FROM semantic_conversation_changes
                          WHERE conversation_id = 0), 0)",
        &[],
        |row| {
            Ok((
                row.get_typed::<Option<String>>(0)?,
                row.get_typed::<i64>(1)?,
                row.get_typed::<i64>(2)?,
            ))
        },
    )?;
    let (state_json, maximum, global_invalidation) = rows
        .first()
        .ok_or_else(|| anyhow!("semantic canonical authority is missing"))?;
    let state = SemanticArchiveState::parse(
        state_json
            .as_deref()
            .ok_or_else(|| anyhow!("semantic canonical authority is missing"))?,
    )?;
    if state.archive_id != certificate.archive_id
        || state.mutation_seq < certificate.mutation_seq
        || *maximum < 0
    {
        bail!("semantic coverage belongs to a different canonical archive");
    }
    let invalidated = *global_invalidation > i64::try_from(certificate.mutation_seq)?;
    Ok((
        SemanticCoverageSnapshot::at_snapshot(certificate, state.mutation_seq, *maximum),
        invalidated,
    ))
}

impl SearchClient {
    /// Admission metadata is useful for owner refresh/status only. Search
    /// responses use the receipt returned by their own hydration transaction.
    pub fn semantic_coverage(&self) -> Option<SemanticCoverageSnapshot> {
        self.semantic
            .lock()
            .ok()?
            .as_ref()?
            .artifacts
            .first()?
            .coverage_snapshot()
            .cloned()
    }

    pub(super) fn hydrate_semantic_hits_at_generation(
        &self,
        results: &[VectorSearchResult],
        field_mask: FieldMask,
        artifact: Option<&SemanticIndexArtifact>,
    ) -> Result<HydratedSemanticPage> {
        let Some(artifact) = artifact.filter(|artifact| artifact.coverage_certificate().is_some())
        else {
            return self
                .hydrate_semantic_hits_with_ids(results, field_mask)
                .map(|hits| (hits, None));
        };
        artifact.verify_coverage_file()?;
        let certificate = artifact
            .coverage_certificate()
            .ok_or_else(|| anyhow!("certified semantic artifact lost its certificate"))?;
        let parents = artifact
            .coverage_parents()
            .ok_or_else(|| anyhow!("certified semantic artifact lost its parent proof"))?;
        let mut sqlite_guard = self.sqlite_guard()?;
        let result = (|| -> Result<HydratedSemanticPage> {
            let conn = sqlite_guard
                .as_ref()
                .ok_or_else(|| anyhow!("semantic search requires database connection"))?;
            conn.verify_canonical_archive()?;
            let mut transaction = SearchReadTransaction::begin(conn)?;
            let (mut coverage, globally_invalidated) =
                coverage_in_transaction(&transaction, certificate)?;
            let mut ids = results
                .iter()
                .filter_map(|candidate| {
                    i64::try_from(candidate.message_id)
                        .ok()
                        .filter(|id| *id > 0 && *id <= certificate.max_message_id)
                })
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids.dedup();
            let retained_parents = parents.identities(&ids)?;
            let current_parents = lookup_semantic_parent_identities_on_async(conn, &ids)?;
            let mut current = HashMap::with_capacity(ids.len());
            if !globally_invalidated {
                for chunk in ids.chunks(256) {
                    let sql = format!(
                        "SELECT m.id, m.content, m.created_at, c.agent_id, c.workspace_id,
                            c.source_id, c.origin_host, m.role,
                            COALESCE(j.invalidation_seq, 0)
                     FROM messages m JOIN conversations c ON c.id = m.conversation_id
                     LEFT JOIN semantic_conversation_changes j ON j.conversation_id = c.id
                     WHERE m.id IN ({})",
                        sql_rowid_literal_list(chunk),
                    );
                    let rows = transaction.query_map_collect(&sql, &[], |row| {
                        Ok(CurrentPassage {
                            message_id: semantic_message_id_from_db(row.get_typed(0)?)?,
                            content: row.get_typed(1)?,
                            created_at: row.get_typed(2)?,
                            agent_id: row.get_typed(3)?,
                            workspace_id: row.get_typed(4)?,
                            source_id: row.get_typed(5)?,
                            origin_host: row.get_typed(6)?,
                            role: row.get_typed(7)?,
                            invalidation_seq: row.get_typed(8)?,
                        })
                    })?;
                    current.extend(rows.into_iter().map(|row| (row.message_id, row)));
                }
            }
            let published_seq = i64::try_from(certificate.mutation_seq)?;
            let candidates = results
                .iter()
                .filter(|candidate| {
                    let Ok(id) = i64::try_from(candidate.message_id) else {
                        return false;
                    };
                    let Some(retained_parent) = retained_parents.get(&id) else {
                        return false;
                    };
                    current_parents.get(&id) == Some(retained_parent)
                        && current.get(&candidate.message_id).is_some_and(|row| {
                            row.invalidation_seq <= published_seq && row.matches(candidate)
                        })
                })
                .map(|candidate| RankedMessageHydrationCandidate {
                    message_id: candidate.message_id,
                    score: candidate.score,
                })
                .collect::<Vec<_>>();
            coverage.excluded_candidates = results.len().saturating_sub(candidates.len());
            if coverage.excluded_candidates > 0 {
                coverage.status = "partial";
                coverage.stale_reason = Some("candidate_identity_changed");
            }
            let hits = self.hydrate_ranked_message_hits_in_transaction(
                &transaction,
                &candidates,
                field_mask,
                MatchType::Exact,
            )?;
            artifact.verify_coverage_file()?;
            transaction.rollback()?;
            conn.verify_canonical_archive()?;
            Ok((hits, Some(coverage)))
        })();
        match result {
            Ok(page) => Ok(page),
            Err(error) => {
                // The closure's transaction guard has completed its rollback
                // path, including error-path Drop, before owner eviction. A
                // later request may open a new owner, but this request never
                // grafts a fresh witness onto the failed connection or retries
                // against a different archive incarnation.
                if let Some(mut connection) = sqlite_guard.take()
                    && let Err(close_error) = connection.close_without_checkpoint_sync()
                {
                    tracing::warn!(%close_error, "failed to close rejected semantic hydration owner");
                }
                let context = format!(
                    "certified semantic hydration failed; retry the search to open a new canonical archive owner: {error:#}"
                );
                Err(error.context(context))
            }
        }
    }
}
