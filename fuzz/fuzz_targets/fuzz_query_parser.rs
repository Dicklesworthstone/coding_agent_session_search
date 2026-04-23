//! Fuzz target for search query parsing and explain-mode analysis.
//!
//! Exercises adversarial query strings and filter values without requiring an
//! index on disk. The target should only return structured explanations or
//! serialization errors; it must never panic on malformed Unicode, deeply
//! quoted input, very long tokens, or odd filter strings.

#![no_main]

use std::collections::HashSet;

use arbitrary::Arbitrary;
use coding_agent_search::search::query::{QueryExplanation, SearchFilters};
use coding_agent_search::sources::provenance::SourceFilter;
use libfuzzer_sys::fuzz_target;

const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_FILTER_BYTES: usize = 4 * 1024;
const MAX_FILTER_VALUES: usize = 16;

#[derive(Arbitrary, Debug)]
struct QueryInput {
    query: String,
    agents: Vec<String>,
    workspaces: Vec<String>,
    session_paths: Vec<String>,
    source_filter: String,
    created_from: Option<i64>,
    created_to: Option<i64>,
}

fn bounded_string(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn bounded_set(values: Vec<String>) -> HashSet<String> {
    values
        .into_iter()
        .take(MAX_FILTER_VALUES)
        .map(|value| bounded_string(value, MAX_FILTER_BYTES))
        .collect()
}

fuzz_target!(|input: QueryInput| {
    let query = bounded_string(input.query, MAX_QUERY_BYTES);
    let filters = SearchFilters {
        agents: bounded_set(input.agents),
        workspaces: bounded_set(input.workspaces),
        created_from: input.created_from,
        created_to: input.created_to,
        source_filter: SourceFilter::parse(&bounded_string(input.source_filter, MAX_FILTER_BYTES)),
        session_paths: bounded_set(input.session_paths),
    };

    let explanation = QueryExplanation::analyze(&query, &filters);
    let _ = serde_json::to_value(&explanation);
});
