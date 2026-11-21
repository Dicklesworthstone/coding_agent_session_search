//! Opt-in title retrieval over retained native vectors and current canonical
//! witnesses. Title evidence is scoped before top-k and cites a real message;
//! neither a query nor its admission creates indexes, downloads, or repairs.

use super::*;
use crate::search::privacy::{PrivacyError, PrivacySnapshot, state_path_for_database};
use crate::search::title_fusion::{
    ConversationScore, TitleCoverage, TitleSearchConfig, TitleSearchMetadata, fuse_title_body,
};
use crate::search::title_index::{
    TitleDocId, TitleIndex, load_title_manifest, normalized_title, title_identity_hash,
    title_input_hash, title_model_revision,
};
use crate::storage::sqlite::SemanticArchiveState;

pub(super) struct TitleSearchContext {
    data_dir: PathBuf,
    config: TitleSearchConfig,
    retained: Mutex<Option<Arc<TitleIndex>>>,
}

impl TitleSearchContext {
    fn open(&self, embedder_id: &str) -> Result<Arc<TitleIndex>> {
        let selected = load_title_manifest(&self.data_dir, embedder_id)?
            .context("title semantic search unavailable; enable CASS_SEMANTIC_TITLES=1 and run explicit semantic indexing first")?;
        let mut retained = self
            .retained
            .lock()
            .map_err(|_| anyhow!("title reader lock poisoned"))?;
        if let Some(index) = retained.as_ref()
            && index.manifest() == &selected
        {
            index.verify_unchanged()?;
            return Ok(Arc::clone(index));
        }
        let index = Arc::new(
            TitleIndex::open_bounded(
                &self.data_dir,
                embedder_id,
                self.config.max_admission_titles,
            )?
            .context("title semantic search unavailable; its manifest disappeared")?,
        );
        *retained = Some(Arc::clone(&index));
        Ok(index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TitleSnapshot {
    archive: SemanticArchiveState,
    global_invalidation: i64,
}

fn snapshot(transaction: &SearchReadTransaction<'_>, database: &Path) -> Result<TitleSnapshot> {
    let rows = transaction.query_map_collect(
        "SELECT (SELECT value FROM meta WHERE key = 'semantic_archive_state_v1'),
                COALESCE((SELECT invalidation_seq FROM semantic_conversation_changes
                          WHERE conversation_id = 0), 0),
                (SELECT value FROM meta WHERE key = 'forget_derived_rebuild_pending')",
        &[],
        |row| {
            Ok((
                row.get_typed::<Option<String>>(0)?,
                row.get_typed::<i64>(1)?,
                row.get_typed::<Option<String>>(2)?,
            ))
        },
    )?;
    let (archive, global_invalidation, pending_forget) = rows
        .first()
        .context("title search archive authority is missing")?;
    if pending_forget.is_some() {
        return Err(PrivacyError::RepairPending {
            path: state_path_for_database(database),
        }
        .into());
    }
    let archive = SemanticArchiveState::parse(
        archive
            .as_deref()
            .context("title search requires canonical mutation authority")?,
    )?;
    anyhow::ensure!(
        *global_invalidation >= 0,
        "invalid title invalidation authority"
    );
    Ok(TitleSnapshot {
        archive,
        global_invalidation: *global_invalidation,
    })
}

struct CanonicalTitle {
    conversation_id: i64,
    agent_id: i64,
    workspace_id: Option<i64>,
    source_id: String,
    external_id: Option<String>,
    source_path: String,
    started_at: Option<i64>,
    title: Option<String>,
    invalidation_seq: i64,
    agent: String,
    workspace: String,
    source_kind: Option<String>,
    origin_host: Option<String>,
}

impl CanonicalTitle {
    fn matches_identity(&self, identity: &TitleDocId) -> bool {
        self.conversation_id == identity.conversation_id
            && u64::try_from(self.invalidation_seq).ok() == Some(identity.invalidation_seq)
            && title_input_hash(self.title.as_deref()) == Some(identity.title_hash)
            && title_identity_hash(
                self.conversation_id,
                self.agent_id,
                self.workspace_id,
                &self.source_id,
                self.external_id.as_deref(),
                &self.source_path,
                self.started_at,
            ) == identity.identity_hash
    }

    fn matches_filters(&self, filters: &SearchFilters) -> bool {
        if (!filters.agents.is_empty() && !filters.agents.contains(&self.agent))
            || (!filters.workspaces.is_empty() && !filters.workspaces.contains(&self.workspace))
            || (!filters.session_paths.is_empty()
                && !filters.session_paths.contains(&self.source_path))
        {
            return false;
        }
        let source = normalized_search_hit_source_id_parts(
            &self.source_id,
            self.source_kind.as_deref().unwrap_or_default(),
            self.origin_host.as_deref(),
        );
        let kind = normalized_search_hit_origin_kind(&source, self.source_kind.as_deref());
        match &filters.source_filter {
            SourceFilter::All => true,
            SourceFilter::Local => kind == crate::sources::provenance::LOCAL_SOURCE_ID,
            SourceFilter::Remote => kind != crate::sources::provenance::LOCAL_SOURCE_ID,
            SourceFilter::SourceId(wanted) => {
                source == normalize_search_source_filter_value(wanted)
            }
        }
    }
}

struct TitleAdmission {
    identity: TitleDocId,
    message_id: u64,
    title: String,
}

struct AdmittedTitles {
    by_conversation: HashMap<i64, TitleAdmission>,
    examined: usize,
    stale: usize,
}

impl FsSearchFilter for AdmittedTitles {
    fn matches(&self, doc_id: &str, _metadata: Option<&serde_json::Value>) -> bool {
        TitleDocId::parse(doc_id).is_some_and(|identity| {
            self.by_conversation
                .get(&identity.conversation_id)
                .is_some_and(|admitted| admitted.identity == identity)
        })
    }

    fn matches_doc_id_hash(
        &self,
        _hash: u64,
        _metadata: Option<&serde_json::Value>,
    ) -> Option<bool> {
        // A hash cannot establish canonical title identity or message scope.
        None
    }

    fn name(&self) -> &str {
        "cass_canonical_title_scope"
    }
}

fn role_matches(role: &str, roles: Option<&HashSet<u8>>) -> bool {
    let normalized = crate::model::conversation_packet::normalize_role(role);
    let code = role_code_from_str(&normalized).unwrap_or(ROLE_USER);
    roles.is_none_or(|roles| roles.contains(&code))
}

fn time_matches(created_at: Option<i64>, filters: &SearchFilters) -> bool {
    if filters.created_from.is_none() && filters.created_to.is_none() {
        return true;
    }
    created_at.is_some_and(|created_at| {
        filters.created_from.is_none_or(|from| created_at >= from)
            && filters.created_to.is_none_or(|to| created_at <= to)
    })
}

/// A title can match a conversation whose original representative is outside
/// the requested time/role window. Select a real in-scope message with bounded
/// indexed seeks instead of discarding it after global top-k or scanning bodies.
fn scoped_message(
    transaction: &SearchReadTransaction<'_>,
    conversation: &CanonicalTitle,
    filters: &SearchFilters,
    roles: Option<&HashSet<u8>>,
) -> Result<Option<u64>> {
    for role in ["user", "assistant", "agent", "system", "tool"] {
        if !role_matches(role, roles) {
            continue;
        }
        let mut sql = String::from(
            "SELECT id FROM messages INDEXED BY idx_messages_title_scope \
             WHERE conversation_id = ? AND role = ?",
        );
        let mut parameters = vec![
            ParamValue::from(conversation.conversation_id),
            ParamValue::from(role),
        ];
        if let Some(from) = filters.created_from {
            sql.push_str(" AND created_at >= ?");
            parameters.push(ParamValue::from(from));
        }
        if let Some(to) = filters.created_to {
            sql.push_str(" AND created_at <= ?");
            parameters.push(ParamValue::from(to));
        }
        sql.push_str(" ORDER BY created_at, id LIMIT 1");
        let rows: Vec<i64> =
            transaction.query_map_collect(&sql, &parameters, |row| row.get_typed(0))?;
        if let Some(id) = rows.first() {
            return Ok(Some(semantic_message_id_from_db(*id)?));
        }
        // NULL timestamps use the conversation's starting instant everywhere
        // else in CASS. Keep that behavior without a non-indexable COALESCE scan.
        if (filters.created_from.is_some() || filters.created_to.is_some())
            && time_matches(conversation.started_at, filters)
        {
            let rows: Vec<i64> = transaction.query_map_collect(
                "SELECT id FROM messages INDEXED BY idx_messages_title_scope \
                 WHERE conversation_id = ? AND role = ? AND created_at IS NULL \
                 ORDER BY id LIMIT 1",
                &parameters[..2],
                |row| row.get_typed(0),
            )?;
            if let Some(id) = rows.first() {
                return Ok(Some(semantic_message_id_from_db(*id)?));
            }
        }
    }
    Ok(None)
}

fn admit_titles(
    transaction: &SearchReadTransaction<'_>,
    index: &TitleIndex,
    current: &TitleSnapshot,
    filters: &SearchFilters,
    roles: Option<&HashSet<u8>>,
) -> Result<AdmittedTitles> {
    let manifest = index.manifest();
    anyhow::ensure!(
        current.archive.archive_id == manifest.archive_state.archive_id
            && current.archive.mutation_seq >= manifest.archive_state.mutation_seq,
        "title semantic search unavailable: artifact belongs to a different canonical archive"
    );
    let mut admitted = AdmittedTitles {
        by_conversation: HashMap::new(),
        examined: 0,
        stale: 0,
    };
    if u64::try_from(current.global_invalidation)? > manifest.archive_state.mutation_seq {
        admitted.examined = manifest.indexed_titles;
        admitted.stale = manifest.indexed_titles;
        return Ok(admitted);
    }

    // Verify the explicit maintenance index before any per-conversation seek.
    // Searches never create or repair it.
    let columns: Vec<String> = transaction.query_map_collect(
        "PRAGMA index_info(idx_messages_title_scope)",
        &[],
        |row| row.get_typed(2),
    )?;
    anyhow::ensure!(
        columns == ["conversation_id", "role", "created_at", "id"],
        "title semantic search unavailable: run explicit semantic indexing to prepare the title scope index"
    );

    for artifact in index.artifacts() {
        for start in (0..artifact.record_count()).step_by(256) {
            let end = start.saturating_add(256).min(artifact.record_count());
            let identities = (start..end)
                .map(|record| {
                    TitleDocId::parse(artifact.doc_id_at(record)?)
                        .context("retained title record has an invalid identity")
                })
                .collect::<Result<Vec<_>>>()?;
            admitted.examined += identities.len();
            let ids = identities
                .iter()
                .map(|identity| identity.conversation_id)
                .collect::<Vec<_>>();
            let sql = format!(
                "SELECT c.id, COALESCE(c.agent_id, 0), c.workspace_id, \
                        COALESCE(c.source_id, 'local'), c.external_id, c.source_path, \
                        c.started_at, substr(c.title, 1, 512), COALESCE(j.invalidation_seq, 0), \
                        COALESCE(a.slug, 'unknown'), COALESCE(w.path, ''), s.kind, c.origin_host \
                 FROM conversations c \
                 LEFT JOIN semantic_conversation_changes j ON j.conversation_id = c.id \
                 LEFT JOIN agents a ON a.id = c.agent_id \
                 LEFT JOIN workspaces w ON w.id = c.workspace_id \
                 LEFT JOIN sources s ON s.id = c.source_id \
                 WHERE c.id IN ({})",
                sql_rowid_literal_list(&ids),
            );
            let rows = transaction.query_map_collect(&sql, &[], |row| {
                Ok(CanonicalTitle {
                    conversation_id: row.get_typed(0)?,
                    agent_id: row.get_typed(1)?,
                    workspace_id: row.get_typed(2)?,
                    source_id: row.get_typed(3)?,
                    external_id: row.get_typed(4)?,
                    source_path: row.get_typed(5)?,
                    started_at: row.get_typed(6)?,
                    title: row.get_typed(7)?,
                    invalidation_seq: row.get_typed(8)?,
                    agent: row.get_typed(9)?,
                    workspace: row.get_typed(10)?,
                    source_kind: row.get_typed(11)?,
                    origin_host: row.get_typed(12)?,
                })
            })?;
            let current_rows = rows
                .into_iter()
                .map(|row| (row.conversation_id, row))
                .collect::<HashMap<_, _>>();
            let representative_ids = identities
                .iter()
                .map(|identity| identity.representative_message_id)
                .collect::<Vec<_>>();
            let sql = format!(
                "SELECT id, conversation_id, role, created_at FROM messages WHERE id IN ({})",
                sql_rowid_literal_list(&representative_ids),
            );
            let representatives = transaction
                .query_map_collect(&sql, &[], |row| {
                    Ok((
                        row.get_typed::<i64>(0)?,
                        (
                            row.get_typed::<i64>(1)?,
                            row.get_typed::<String>(2)?,
                            row.get_typed::<Option<i64>>(3)?,
                        ),
                    ))
                })?
                .into_iter()
                .collect::<HashMap<_, _>>();
            for identity in identities {
                let Some(row) = current_rows
                    .get(&identity.conversation_id)
                    .filter(|row| row.matches_identity(&identity))
                else {
                    admitted.stale += 1;
                    continue;
                };
                // The original canonical citation must still belong to this
                // conversation, even when a different message meets the filter.
                let Some((parent, role, created_at)) = representatives
                    .get(&identity.representative_message_id)
                    .filter(|(parent, _, _)| *parent == row.conversation_id)
                else {
                    admitted.stale += 1;
                    continue;
                };
                let _ = parent;
                if !row.matches_filters(filters) {
                    continue;
                }
                let message_id = if role_matches(role, roles)
                    && time_matches(created_at.or(row.started_at), filters)
                {
                    Some(semantic_message_id_from_db(
                        identity.representative_message_id,
                    )?)
                } else {
                    scoped_message(transaction, row, filters, roles)?
                };
                if let Some(message_id) = message_id {
                    admitted.by_conversation.insert(
                        identity.conversation_id,
                        TitleAdmission {
                            identity,
                            message_id,
                            title: normalized_title(row.title.as_deref()).unwrap_or_default(),
                        },
                    );
                }
            }
        }
    }
    Ok(admitted)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl SearchClient {
    /// Attach an optional title policy to the currently admitted semantic
    /// producer. Context replacement clears it, preventing cross-model reuse.
    /// Disabled configuration does no file/database/network work.
    pub fn set_title_search_config(
        &self,
        data_dir: &Path,
        config: TitleSearchConfig,
    ) -> Result<()> {
        if config.enabled {
            config.validate()?;
        }
        let mut semantic = self
            .semantic
            .lock()
            .map_err(|_| anyhow!("semantic lock poisoned"))?;
        let Some(state) = semantic.as_mut() else {
            anyhow::ensure!(
                !config.enabled,
                "title semantic search requires an admitted body context"
            );
            return Ok(());
        };
        state.title_search = config.enabled.then(|| {
            Arc::new(TitleSearchContext {
                data_dir: data_dir.to_path_buf(),
                config,
                retained: Mutex::new(None),
            })
        });
        if state.progressive_context_unavailable
            == Some(SemanticProgressiveUnavailableReason::TitleFusionRequiresSinglePass)
        {
            state.progressive_context_unavailable = None;
        }
        for mode in [
            SemanticTierMode::Progressive,
            SemanticTierMode::FastOnly,
            SemanticTierMode::QualityOnly,
        ] {
            if state.in_memory_two_tier_unavailable.reason(mode)
                == Some(SemanticProgressiveUnavailableReason::TitleFusionRequiresSinglePass)
            {
                state.in_memory_two_tier_unavailable.clear_available(mode);
            }
        }
        Ok(())
    }

    fn title_snapshot(&self) -> Result<TitleSnapshot> {
        let sqlite = self.sqlite_guard()?;
        let conn = sqlite
            .as_ref()
            .context("title search requires a canonical database")?;
        conn.verify_canonical_archive()?;
        let mut transaction = SearchReadTransaction::begin(conn)?;
        let current = snapshot(
            &transaction,
            self.sqlite_path
                .as_deref()
                .context("title search requires a canonical database path")?,
        )?;
        transaction.rollback()?;
        conn.verify_canonical_archive()?;
        Ok(current)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn search_with_titles(
        &self,
        context: &TitleSearchContext,
        query: &str,
        filters: SearchFilters,
        limit: usize,
        offset: usize,
        field_mask: FieldMask,
        approximate: bool,
        tier_mode: SemanticTierMode,
    ) -> Result<SemanticSearchOutcome> {
        let config = context.config;
        let field_mask = effective_field_mask(field_mask);
        let canonical = canonicalize_for_embedding(query);
        let database = self
            .sqlite_path
            .as_deref()
            .context("title search requires a canonical database path")?;
        // The durable fence is published before SQLite deletion begins. Bind
        // it before opening retained title assets and admit the final result
        // only if the same privacy epoch still holds.
        let privacy = PrivacySnapshot::for_database(database)?;
        let outcome = (|| -> Result<SemanticSearchOutcome> {
            // At most three attempts at one canonical snapshot. Busy archives
            // receive an explicit retry, never a mixed title/body generation.
            for _ in 0..3 {
                let before = self.title_snapshot()?;
                let (embedder_id, roles, token) = {
                    let semantic = self
                        .semantic
                        .lock()
                        .map_err(|_| anyhow!("semantic lock poisoned"))?;
                    let state = semantic.as_ref().context("semantic search unavailable")?;
                    (
                        state.embedder.id().to_owned(),
                        state.roles.clone(),
                        Arc::clone(&state.context_token),
                    )
                };
                let index = if config.title_weight > 0.0 {
                    anyhow::ensure!(
                        title_model_revision(&embedder_id).is_some(),
                        "title semantic search unavailable for this producer; use a supported native or hash model"
                    );
                    Some(context.open(&embedder_id)?)
                } else {
                    None
                };
                privacy.validate()?;
                let (body, ann_stats, coverage) = if config.title_weight < 1.0 {
                    self.search_semantic_body_with_coverage(
                        query,
                        filters.clone(),
                        config.body_candidates,
                        0,
                        field_mask,
                        approximate,
                        tier_mode,
                    )?
                } else {
                    (Vec::new(), None, None)
                };
                let embedding = if index.is_some() {
                    Some(self.semantic_query_embedding(&canonical)?)
                } else {
                    None
                };
                if !self.semantic_context_matches(&token)?
                    || embedding
                        .as_ref()
                        .is_some_and(|embedding| !Arc::ptr_eq(&embedding.context_token, &token))
                {
                    continue;
                }
                let sqlite = self.sqlite_guard()?;
                let conn = sqlite
                    .as_ref()
                    .context("title search requires a canonical database")?;
                conn.verify_canonical_archive()?;
                let mut transaction = SearchReadTransaction::begin(conn)?;
                let current = snapshot(&transaction, database)?;
                if current != before {
                    transaction.rollback()?;
                    continue;
                }
                let mut metadata = TitleSearchMetadata {
                    coverage: TitleCoverage::ValidatedCandidates,
                    policy: config,
                    title_generation: index
                        .as_ref()
                        .map(|index| index.manifest().generation.clone()),
                    archive_mutation_seq: current.archive.mutation_seq,
                    title_mutation_seq: index
                        .as_ref()
                        .map(|index| index.manifest().archive_state.mutation_seq),
                    indexed_titles: index
                        .as_ref()
                        .map_or(0, |index| index.manifest().indexed_titles),
                    eligible_titles: 0,
                    stale_titles_excluded: 0,
                    body_conversations: 0,
                    title_conversations: 0,
                    candidate_window_limited: body.len() == config.body_candidates,
                    admission_cache_hit: false,
                    admission_titles_examined: 0,
                    admission_bitmap_bytes: 0,
                    admission_ms: 0,
                    title_search_ms: 0,
                };
                let mut title_hits = HashMap::new();
                let mut title_scores = Vec::new();
                if let (Some(index), Some(embedding)) = (&index, &embedding) {
                    let started = Instant::now();
                    let admission =
                        admit_titles(&transaction, index, &current, &filters, roles.as_ref())?;
                    metadata.admission_ms = elapsed_ms(started);
                    metadata.eligible_titles = admission.by_conversation.len();
                    metadata.stale_titles_excluded = admission.stale;
                    metadata.admission_titles_examined = admission.examined;
                    metadata.candidate_window_limited |=
                        admission.by_conversation.len() > config.title_candidates;
                    let started = Instant::now();
                    for shard in index.artifacts() {
                        for hit in shard.search_top_k(
                            &embedding.vector,
                            config.title_candidates,
                            Some(&admission),
                        )? {
                            let identity = TitleDocId::parse(&hit.doc_id)
                                .context("invalid admitted title result")?;
                            anyhow::ensure!(
                                hit.score.is_finite(),
                                "title search returned a non-finite score"
                            );
                            title_scores.push(ConversationScore {
                                conversation_id: identity.conversation_id,
                                score: hit.score,
                            });
                        }
                        // Bound retained candidates across all 256 partitions,
                        // rather than multiplying the query's cap by shard count.
                        title_scores.sort_unstable_by(|a, b| {
                            b.score
                                .total_cmp(&a.score)
                                .then(a.conversation_id.cmp(&b.conversation_id))
                        });
                        title_scores.truncate(config.title_candidates);
                    }
                    metadata.title_search_ms = elapsed_ms(started);
                    let candidates = title_scores
                        .iter()
                        .map(|score| {
                            let admitted = admission
                                .by_conversation
                                .get(&score.conversation_id)
                                .context("title result escaped canonical admission")?;
                            Ok(RankedMessageHydrationCandidate {
                                message_id: admitted.message_id,
                                score: score.score,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    // The title is already bounded and redacted by the versioned
                    // representation. Do not hydrate an unbounded raw title here.
                    let title_mask = FieldMask::new(
                        field_mask.needs_content(),
                        field_mask.wants_snippet(),
                        false,
                        false,
                    )
                    .with_preview_content_limit(field_mask.preview_content_limit());
                    for (_, mut hit) in self.hydrate_ranked_message_hits_in_transaction(
                        &transaction,
                        &candidates,
                        title_mask,
                        MatchType::SemanticTitle,
                    )? {
                        let id = hit
                            .conversation_id
                            .context("title citation lost its conversation")?;
                        let admitted = admission
                            .by_conversation
                            .get(&id)
                            .context("title citation escaped admission")?;
                        anyhow::ensure!(
                            Self::sqlite_fts5_hit_matches_filters(&hit, &filters),
                            "title citation escaped query scope"
                        );
                        if field_mask.wants_title() {
                            hit.title.clone_from(&admitted.title);
                        }
                        title_hits.insert(id, project_hit_content(hit, field_mask));
                    }
                    title_scores.retain(|score| title_hits.contains_key(&score.conversation_id));
                    index.verify_unchanged()?;
                }
                let mut body_hits: HashMap<i64, SearchHit> = HashMap::new();
                for hit in body {
                    let Some(id) = hit.conversation_id.filter(|id| *id > 0) else {
                        continue;
                    };
                    match body_hits.entry(id) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(hit);
                        }
                        std::collections::hash_map::Entry::Occupied(mut entry)
                            if hit.score > entry.get().score =>
                        {
                            entry.insert(hit);
                        }
                        _ => {}
                    }
                }
                let body_scores = body_hits
                    .iter()
                    .map(|(&conversation_id, hit)| ConversationScore {
                        conversation_id,
                        score: hit.score,
                    })
                    .collect::<Vec<_>>();
                metadata.body_conversations = body_scores.len();
                metadata.title_conversations = title_scores.len();
                let fused = fuse_title_body(&body_scores, &title_scores, config)?;
                let union_count = body_hits
                    .keys()
                    .chain(title_hits.keys())
                    .copied()
                    .collect::<HashSet<_>>()
                    .len();
                metadata.candidate_window_limited |= union_count > config.total_candidates;
                let requested = if limit == 0 {
                    config.total_candidates
                } else {
                    limit
                };
                metadata.candidate_window_limited |=
                    offset.saturating_add(requested) > config.total_candidates;
                let hits = fused
                    .into_iter()
                    .skip(offset)
                    .take(requested)
                    .filter_map(|score| {
                        let mut hit = body_hits
                            .remove(&score.conversation_id)
                            .or_else(|| title_hits.remove(&score.conversation_id))?;
                        hit.score = score.score;
                        if score.title_score.is_some() {
                            hit.match_type = if score.body_score.is_some() {
                                MatchType::SemanticTitleBody
                            } else {
                                MatchType::SemanticTitle
                            };
                        }
                        Some(hit)
                    })
                    .collect();
                transaction.rollback()?;
                conn.verify_canonical_archive()?;
                if !self.semantic_context_matches(&token)? {
                    continue;
                }
                return Ok(SemanticSearchOutcome {
                    hits,
                    ann_stats,
                    coverage,
                    titles: Some(metadata),
                });
            }
            bail!(
                "title semantic search unavailable: canonical history or its semantic producer changed during the query; retry"
            )
        })();
        // Privacy takes precedence over a missing/replaced artifact error;
        // otherwise a caller could mistake concurrent forget for a normal
        // semantic-unavailable condition and fall back to retained text.
        privacy.validate()?;
        outcome
    }
}
