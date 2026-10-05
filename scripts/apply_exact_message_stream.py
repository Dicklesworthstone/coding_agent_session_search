#!/usr/bin/env python3
"""Wire the real exact-message scan without rewriting concurrent query changes.

The source branch and lockfile stay intact. Exact call-site preconditions must
hold, and CI compiles/tests this integrated tree before a normal main push.
"""
from pathlib import Path
import hashlib

PATH = Path('src/search/query.rs')
SIGNATURE = '''    fn search_exact_semantic_indexes(
        context: &SemanticCandidateContext,
        embedding: &[f32],
        fetch_limit: usize,
        fs_filter: Option<&dyn FsSearchFilter>,
    ) -> Result<(Vec<VectorSearchResult>, SemanticCandidateRetryState)> {
'''
WRAPPER = SIGNATURE + '''        // Post-top-k WAL supersession can erase an entire raw window even
        // when current messages remain. For bounded standard-width WAL views,
        // select the retained main+delta view directly before that shortcut.
        if context.artifacts.iter().any(|artifact| artifact.index().wal_record_count() > 0) {
            let record_count = context.artifacts.iter().fold(0usize, |total, artifact| {
                total.saturating_add(artifact.index().record_count())
                    .saturating_add(artifact.index().wal_record_count())
            });
            let return_limit = Self::semantic_exact_candidate_limit(fetch_limit, record_count);
            if let Some(hits) = message_stream::try_collect_exact_messages(
                &context.artifacts, embedding, return_limit, fs_filter,
            )? {
                let has_more_candidates = hits.len() >= return_limit && return_limit < record_count;
                return Ok((hits, SemanticCandidateRetryState {
                    has_more_candidates,
                    exact_window_may_omit_competitor: false,
                }));
            }
        }
        Self::search_exact_semantic_indexes_with_refinement(
            context, embedding, fetch_limit, fs_filter, true,
        )
    }

    // Retain the incumbent driver for unsupported row-score widths and large
    // WAL overlays, and for same-binary, real-engine differential tests.
    fn search_exact_semantic_indexes_with_refinement(
        context: &SemanticCandidateContext,
        embedding: &[f32],
        fetch_limit: usize,
        fs_filter: Option<&dyn FsSearchFilter>,
        streaming: bool,
    ) -> Result<(Vec<VectorSearchResult>, SemanticCandidateRetryState)> {
'''
BEFORE = '''        let return_limit = Self::semantic_exact_candidate_limit(fetch_limit, record_count);
        let mut refills_left = message_topk::MAX_EXACT_MESSAGE_REFILLS;
'''
AFTER = '''        let return_limit = Self::semantic_exact_candidate_limit(fetch_limit, record_count);
        if streaming
            && let Some(hits) = message_stream::try_collect_exact_messages(
                &context.artifacts, embedding, return_limit, fs_filter,
            )?
        {
            let has_more_candidates = hits.len() >= return_limit && return_limit < record_count;
            return Ok((hits, SemanticCandidateRetryState {
                has_more_candidates,
                exact_window_may_omit_competitor: false,
            }));
        }
        let mut refills_left = message_topk::MAX_EXACT_MESSAGE_REFILLS;
'''
EDITS = [('mod message_topk;\n', 'mod message_stream;\nmod message_topk;\n'),
         (SIGNATURE, WRAPPER), (BEFORE, AFTER)]


def integrate(text: str) -> str:
    # A previously formatted integration cannot be compared to unformatted
    # snippets. Verify the complete set of distinct structural markers instead;
    # subsequent compilation and differential tests still gate publication.
    markers = ['mod message_stream;',
               'fn search_exact_semantic_indexes_with_refinement(',
               'message_stream::try_collect_exact_messages(']
    counts = [text.count(marker) for marker in markers]
    if counts == [1, 1, 2]:
        return text
    if any(counts):
        raise ValueError('Partial or duplicate exact-message integration; refusing to rewrite')
    result = text
    for old, new in EDITS:
        if result.count(old) != 1:
            raise ValueError(f'Expected exactly one original anchor: {old!r}')
        result = result.replace(old, new, 1)
    return result


def main() -> None:
    before = PATH.read_bytes()
    after = integrate(before.decode()).encode()
    assert integrate(after.decode()).encode() == after
    print(f'input_sha256={hashlib.sha256(before).hexdigest()}')
    print(f'output_sha256={hashlib.sha256(after).hexdigest()}')
    if before != after:
        PATH.write_bytes(after)
    print(f'changed={before != after}')


if __name__ == '__main__':
    main()
