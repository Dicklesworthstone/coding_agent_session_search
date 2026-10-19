//! Fuel refusal admission for optional counting and explicit lexical fallback.
//!
//! Native query errors retain their concrete type. Text recognition is only a
//! compatibility path for older, already-flattened bridge diagnostics.

use frankensearch::quill::QuillIndexError;

pub(super) fn is_exhausted(error: &anyhow::Error) -> bool {
    // A concrete engine failure is authoritative even when an outer context
    // happens to quote a fuel diagnostic (for example a user-supplied query).
    for cause in error.chain() {
        if let Some(engine) = cause.downcast_ref::<QuillIndexError>() {
            return matches!(engine, QuillIndexError::QueryFuelExhausted { .. });
        }
    }
    error
        .chain()
        .any(|cause| legacy_diagnostic(&cause.to_string()))
}

fn unsigned(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok()
}

/// Recognize the two historical bridge forms, not arbitrary mentions of fuel.
/// The shorter form predates the work-counter suffix. No regex, retry, engine
/// open or configuration change is needed to classify either representation.
fn legacy_diagnostic(message: &str) -> bool {
    let message = message
        .strip_prefix("executing a Quill lexical query: ")
        .unwrap_or(message);
    let Some(rest) = message.strip_prefix("scalar Quill query fuel exhausted after ") else {
        return false;
    };
    let Some((consumed, rest)) = rest.split_once('/') else {
        return false;
    };
    let Some((budget, suffix)) = rest.split_once(" units") else {
        return false;
    };
    if !unsigned(consumed) || !unsigned(budget) {
        return false;
    }
    if suffix.is_empty() {
        return true;
    }
    let Some(counters) = suffix
        .strip_prefix(" (segments=")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    let Some((segments, rest)) = counters.split_once(", dictionary_blocks=") else {
        return false;
    };
    let Some((dictionary, rest)) = rest.split_once(", posting_blocks=") else {
        return false;
    };
    let Some((postings, positions)) = rest.split_once(", position_docs=") else {
        return false;
    };
    [segments, dictionary, postings, positions]
        .into_iter()
        .all(unsigned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::quill_bridge::{
        QuillCassIndex, is_query_fuel_exhausted, open_cass_reader_with_query_fuel_budget,
        search_paginated,
    };
    use frankensearch::quill::argus::ArgusError;
    use frankensearch::quill::cass::CassDocument;
    use frankensearch::quill::query::{CassQueryFilters, CassQueryParser};
    use frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA;

    fn engine_refusal() -> QuillIndexError {
        ArgusError::QueryFuelExhausted {
            budget: 100,
            consumed: 100,
            segments_touched: 4,
            dictionary_blocks: 88,
            posting_blocks: 8,
            position_docs: 0,
        }
        .into()
    }

    #[test]
    fn concrete_fuel_survives_context_without_parsing_the_context() {
        let error = anyhow::Error::new(engine_refusal()).context("an unrelated outer message");
        assert!(is_query_fuel_exhausted(&error));
        let error = error.context("one more layer");
        assert!(is_query_fuel_exhausted(&error));
    }

    #[test]
    fn typed_nonfuel_errors_cannot_be_overridden_by_misleading_context() {
        let misleading = engine_refusal().to_string();
        for engine in [
            QuillIndexError::InvalidState {
                detail: misleading.clone(),
            },
            QuillIndexError::from(ArgusError::QueryCancelled { phase: "collect" }),
            QuillIndexError::from(ArgusError::CursorInvariant("query fuel exhausted")),
        ] {
            let error = anyhow::Error::new(engine).context(misleading.clone());
            assert!(!is_query_fuel_exhausted(&error), "{error:#}");
        }
    }

    #[test]
    fn legacy_engine_diagnostics_remain_compatible() {
        let diagnostic = engine_refusal().to_string();
        for message in [
            diagnostic.clone(),
            format!("executing a Quill lexical query: {diagnostic}"),
            "scalar Quill query fuel exhausted after 10000000/10000000 units".to_string(),
        ] {
            let error = anyhow::anyhow!(message).context("historical wrapper");
            assert!(is_query_fuel_exhausted(&error), "{error:#}");
        }
    }

    #[test]
    fn unrelated_or_malformed_text_does_not_authorize_degradation() {
        let valid = engine_refusal().to_string();
        for message in [
            "query fuel exhausted".to_string(),
            format!("invalid query text: {valid}"),
            format!("{valid}: corrupt index"),
            valid.replace("100/100", "+100/100"),
            valid.replace("100/100", "100/not-a-budget"),
            valid.replace("100/100", "100/18446744073709551616"),
            valid.replace("segments=4", "segments=-4"),
            valid.replace("posting_blocks=8", "posting_blocks=8x"),
            valid.replace("position_docs=0", "position_docs=0, hidden=1"),
        ] {
            assert!(
                !is_query_fuel_exhausted(&anyhow::anyhow!(message.clone())),
                "{message}"
            );
        }
    }

    #[test]
    fn real_fragmented_queries_preserve_typed_refusal_at_the_bridge() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut writer = QuillCassIndex::open_or_create(directory.path())?;
        for ordinal in 0..24_i64 {
            writer.add_cass_documents(&[CassDocument {
                agent: "codex".into(),
                workspace: Some("/gh518".into()),
                workspace_original: None,
                source_path: format!("/gh518/{ordinal}.jsonl"),
                msg_idx: 0,
                created_at: Some(1_700_000_000_000 + ordinal),
                title: Some(format!("query fixture {ordinal}")),
                content: "git fixture test isolation".into(),
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
                conversation_id: Some(ordinal + 1),
            }])?;
            writer.commit()?;
        }
        drop(writer);
        let normal = open_cass_reader_with_query_fuel_budget(directory.path(), 10_000_000)?;
        let constrained = open_cass_reader_with_query_fuel_budget(directory.path(), 1)?;
        assert_eq!(normal.segment_count()?, 24);
        let parser = CassQueryParser::new(CASS_SEMANTIC_SCHEMA)
            .map_err(|error| anyhow::anyhow!("building CASS query parser: {error}"))?;
        for raw in ["git fixture", "test isolation"] {
            let query = parser.parse(raw, &CassQueryFilters::default()).query;
            assert_eq!(search_paginated(&normal, &query, 1, 0, false)?.hits.len(), 1);
            for exact_count in [false, true] {
                let error = search_paginated(&constrained, &query, 1, 0, exact_count)
                    .expect_err("the reader must actually meter its work");
                assert!(
                    matches!(
                        error.downcast_ref::<QuillIndexError>(),
                        Some(QuillIndexError::QueryFuelExhausted { .. })
                    ),
                    "the bridge flattened its engine cause: {error:#}"
                );
                assert!(is_query_fuel_exhausted(&error));
                assert!(
                    error.to_string().starts_with("executing a Quill lexical query: ")
                );
            }
        }
        Ok(())
    }
}
