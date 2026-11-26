//! GH518: preserve a usable filtered page when only a larger fetch exhausts fuel.
//!
//! The first fetch is required. A rejected retry with no retained in-scope hits
//! is still an error, not an authoritative empty page. This helper does not run
//! queries, open another backend, increase fuel, or retain cross-request state.

use anyhow::Result;

use super::{LexicalCandidates, SearchClient};

/// A native lexical page is incomplete because its larger deduplication fetch
/// exhausted fuel. Unlike `query_fuel_exhausted`, the lexical leg was not lost.
pub const LEXICAL_RETRY_QUERY_FUEL_EXHAUSTED: &str = "dedup_retry_query_fuel_exhausted";

fn admit_retry<T>(attempt: Result<T>, retained_hits: usize) -> Result<Option<T>> {
    match attempt {
        Ok(page) => Ok(Some(page)),
        Err(error)
            if retained_hits != 0
                && crate::search::quill_bridge::is_query_fuel_exhausted(&error) =>
        {
            tracing::warn!(
                reason = LEXICAL_RETRY_QUERY_FUEL_EXHAUSTED,
                retained_hits,
                error = %format_args!("{error:#}"),
                "larger lexical fetch exhausted query fuel; retaining the filtered page with unknown total"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

impl SearchClient {
    pub(super) fn settle_lexical_retry(
        &self,
        attempt: Result<LexicalCandidates>,
        retained_hits: usize,
    ) -> Result<Option<LexicalCandidates>> {
        let retry = admit_retry(attempt, retained_hits)?;
        if retry.is_none() {
            self.record_lexical_degrade_reason(Some(LEXICAL_RETRY_QUERY_FUEL_EXHAUSTED));
            if let Ok(mut total) = self.last_tantivy_total_count.lock() {
                *total = None;
            }
        }
        Ok(retry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::{FieldMask, SearchClientOptions, SearchFilters};
    use crate::search::quill_bridge::{
        QuillCassIndex, is_query_fuel_exhausted, live_segment_count,
        open_cass_reader_with_query_fuel_budget, search_paginated,
    };
    use frankensearch::quill::QuillIndexError;
    use frankensearch::quill::argus::ArgusError;
    use frankensearch::quill::cass::CassDocument;
    use frankensearch::quill::query::{CassQueryFilters, CassQueryParser};
    use frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA;

    fn fuel() -> anyhow::Error {
        anyhow::Error::new(QuillIndexError::from(ArgusError::QueryFuelExhausted {
            budget: 100,
            consumed: 100,
            segments_touched: 4,
            dictionary_blocks: 88,
            posting_blocks: 8,
            position_docs: 0,
        }))
        .context("larger native fetch")
    }

    #[test]
    fn completed_retry_is_returned_unchanged_without_requiring_clone() {
        struct Owned(u64);
        for retained in [0, 1, usize::MAX] {
            let actual = admit_retry(Ok(Owned(42)), retained).unwrap().unwrap();
            assert_eq!(actual.0, 42);
        }
    }

    #[test]
    fn fuel_refusal_requires_a_nonempty_postfilter_page() {
        assert!(admit_retry::<()>(Err(fuel()), 1).unwrap().is_none());
        let error = admit_retry::<()>(Err(fuel()), 0).unwrap_err();
        assert!(is_query_fuel_exhausted(&error));
        assert!(error.downcast_ref::<QuillIndexError>().is_some());
    }

    #[test]
    fn cancellation_corruption_and_misleading_context_are_not_recovered() {
        let misleading = fuel().chain().last().unwrap().to_string();
        for engine in [
            QuillIndexError::from(ArgusError::QueryCancelled { phase: "collect" }),
            QuillIndexError::from(ArgusError::CursorInvariant("invalid cursor")),
            QuillIndexError::InvalidState {
                detail: "invalid generation".into(),
            },
        ] {
            let expected = engine.to_string();
            let error = anyhow::Error::new(engine).context(misleading.clone());
            let error = admit_retry::<()>(Err(error), 3).unwrap_err();
            assert_eq!(
                error.downcast_ref::<QuillIndexError>().unwrap().to_string(),
                expected
            );
        }
        let error = admit_retry::<()>(
            Err(anyhow::anyhow!("unrelated query fuel exhausted text")),
            3,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "unrelated query fuel exhausted text");
    }

    fn federated_document(id: i64, content: &str) -> CassDocument {
        CassDocument {
            agent: "claude_code".into(),
            workspace: Some("/retry".into()),
            workspace_original: None,
            source_path: format!("/retry/{id}.jsonl"),
            msg_idx: 0,
            created_at: Some(1_700_000_000_000 + id),
            title: Some("federated retry fixture".into()),
            content: content.into(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(id),
        }
    }

    fn federated_fixture(
        shards: &[Vec<CassDocument>],
    ) -> Result<(tempfile::TempDir, SearchClient)> {
        use crate::search::privacy::PrivacySnapshot;
        use crate::search::quill_bridge::{QuillCassFields, open_cass_reader};
        use crate::search::tantivy::OpenedLexicalIndex;

        let directory = tempfile::tempdir()?;
        let mut readers = Vec::new();
        for (ordinal, documents) in shards.iter().enumerate() {
            let path = directory.path().join(format!("shard-{ordinal}"));
            let mut writer = QuillCassIndex::open_or_create(&path)?;
            writer.add_cass_documents(documents)?;
            writer.commit()?;
            drop(writer);
            readers.push((open_cass_reader(&path)?, QuillCassFields::compiled()));
        }
        let client = SearchClient::from_opened_lexical_index(
            OpenedLexicalIndex {
                path: directory.path().to_path_buf(),
                reader: None,
                federated_readers: Some(readers),
                privacy: PrivacySnapshot::for_index(directory.path()).map(Some),
            },
            None,
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .expect("native federated reader");
        Ok((directory, client))
    }

    #[test]
    fn federated_retry_keeps_full_shard_evidence_with_empty_or_short_peers() -> Result<()> {
        let query = "deploy cache stall";
        let answer = "We checked the deploy and found the cache stall after restarting the worker. The diagnostic details explain the configuration and retained logs.";
        for offset in [0_usize, 1] {
            let initial_limit = (offset + 1) * 2;
            for (peer, peer_matches, peer_is_answer) in [
                ("unrelated message about astronomy", false, false),
                ("[Tool: deploy cache stall", true, false),
                ("The deploy cache stall was resolved.", true, true),
            ] {
                let mut active = (0..initial_limit)
                    .map(|ordinal| {
                        federated_document(ordinal as i64 + 1, "[Tool: deploy cache stall")
                    })
                    .collect::<Vec<_>>();
                active.extend(
                    (0..=offset).map(|ordinal| federated_document(100 + ordinal as i64, answer)),
                );
                let complete_limit = active.len() + 1;
                let (_directory, client) =
                    federated_fixture(&[active, vec![federated_document(200, peer)]])?;
                let readers = client.federated_readers().expect("retained shards");
                for mask in [
                    FieldMask::new(false, false, false, false),
                    FieldMask::new(false, true, true, false),
                    FieldMask::new(true, true, true, false),
                ] {
                    // Prove the adversarial ranking with actual Quill pages:
                    // the first shard's whole initial window is tool noise,
                    // while its peer is empty or exhausted below that limit.
                    let initial = client.search_tantivy_federated(
                        readers.as_ref(),
                        query,
                        query,
                        SearchFilters::default(),
                        initial_limit,
                        mask,
                    )?;
                    assert_eq!(
                        initial.fetched_count,
                        initial_limit + usize::from(peer_matches)
                    );
                    assert!(initial.window_saturated);
                    assert_eq!(initial.hits.len(), usize::from(peer_is_answer));

                    // An independently complete native fetch is the oracle.
                    // It bypasses the retry/cache path, so warming that cache
                    // cannot conceal the original false-empty-page bug.
                    let complete = client.search_tantivy_federated(
                        readers.as_ref(),
                        query,
                        query,
                        SearchFilters::default(),
                        complete_limit,
                        mask,
                    )?;
                    assert!(!complete.window_saturated);
                    assert_eq!(
                        complete.hits.len(),
                        offset + 1 + usize::from(peer_is_answer)
                    );
                    let (_, expected) = client.postprocess_hits_page(
                        complete.hits.into_iter().map(|ranked| ranked.hit).collect(),
                        None,
                        &SearchFilters::default(),
                        1,
                        offset,
                        mask,
                    );
                    assert_eq!(expected.len(), 1);
                    let actual = client.search(query, SearchFilters::default(), 1, offset, mask)?;
                    assert_eq!(
                        serde_json::to_value(&actual)?,
                        serde_json::to_value(&expected)?,
                        "offset={offset}, peer={peer:?}, mask={mask:?}"
                    );
                    let repeated =
                        client.search(query, SearchFilters::default(), 1, offset, mask)?;
                    assert_eq!(
                        serde_json::to_value(&repeated)?,
                        serde_json::to_value(&expected)?
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn federated_exhausted_shards_do_not_look_full_when_their_counts_add_up() -> Result<()> {
        let query = "deploy cache stall";
        let (_directory, client) = federated_fixture(&[
            vec![federated_document(1, "[Tool: deploy cache stall")],
            vec![federated_document(2, "[Tool: deploy cache stall")],
        ])?;
        let readers = client.federated_readers().expect("retained shards");
        let mask = FieldMask::new(false, true, true, false);
        let page = client.search_tantivy_federated(
            readers.as_ref(),
            query,
            query,
            SearchFilters::default(),
            2,
            mask,
        )?;
        assert_eq!(page.fetched_count, 2);
        assert!(!page.window_saturated);
        assert!(page.hits.is_empty());
        assert!(
            client
                .search(query, SearchFilters::default(), 1, 0, mask)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn gh518_fragmented_real_page_survives_injected_retry_refusal_and_next_search_resets_state()
    -> Result<()> {
        // The engine evidence and the policy evidence are deliberately distinct:
        // a normal reader supplies the required page; a one-unit reader on the
        // SAME index supplies a genuine typed retry refusal. This does not claim
        // that this corpus naturally fails only the retry under one shared budget.
        let directory = tempfile::tempdir()?;
        let mut writer = QuillCassIndex::open_or_create(directory.path())?;
        for ordinal in 0..206_i64 {
            writer.add_cass_documents(&[CassDocument {
                agent: "codex".into(),
                workspace: Some("/gh518".into()),
                workspace_original: None,
                source_path: format!("/gh518/{ordinal}.jsonl"),
                msg_idx: 0,
                created_at: Some(1_700_000_000_000 + ordinal),
                title: Some(format!("retry fixture {ordinal}")),
                content: format!("git fixture test isolation distinct{ordinal}"),
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
                conversation_id: Some(ordinal + 1),
            }])?;
            writer.commit()?;
        }
        drop(writer);
        assert_eq!(live_segment_count(directory.path()), Some(206));
        let client = SearchClient::open_with_options(
            directory.path(),
            None,
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .expect("published native reader");
        let constrained = open_cass_reader_with_query_fuel_budget(directory.path(), 1)?;
        let parser = CassQueryParser::new(CASS_SEMANTIC_SCHEMA)
            .map_err(|error| anyhow::anyhow!("CASS parser: {error}"))?;
        let mask = FieldMask::new(false, true, true, false);
        for query in ["git fixture", "test isolation"] {
            let hits = client.search(query, SearchFilters::default(), 1, 0, mask)?;
            assert!(!hits.is_empty(), "normal control must retrieve a real hit");
            let before = serde_json::to_value(&hits)?;
            let parsed = parser.parse(query, &CassQueryFilters::default()).query;
            let error = search_paginated(&constrained, &parsed, 3, 0, false)
                .expect_err("one-unit reader must actually meter the query");
            assert!(error.downcast_ref::<QuillIndexError>().is_some());
            *client.last_tantivy_total_count.lock().unwrap() = Some(206);
            assert!(
                client
                    .settle_lexical_retry(Err(error), hits.len())?
                    .is_none()
            );
            assert_eq!(serde_json::to_value(&hits)?, before);
            assert!(client.last_tantivy_total_count.lock().unwrap().is_none());
            assert_eq!(
                client.lexical_degrade_reason(),
                Some(LEXICAL_RETRY_QUERY_FUEL_EXHAUSTED)
            );
            let after = client.search(query, SearchFilters::default(), 1, 0, mask)?;
            assert_eq!(serde_json::to_value(&after)?, before);
            assert_eq!(client.lexical_degrade_reason(), None);
        }
        Ok(())
    }
}
