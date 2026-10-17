#!/usr/bin/env python3
"""Wire exact selection while preserving concurrent query work and test gates.

Both the caller and the retained-WAL fixture have exact source preconditions.
All transformations are prepared before either file is written. The unchanged
workflow then runs the real Rust differential/exact/ANN suites and lib Clippy.
"""
from pathlib import Path
import hashlib

PATH = Path('src/search/query.rs')
TEST_PATH = Path('src/search/query/message_stream/tests.rs')
SIGNATURE = '''    fn search_exact_semantic_indexes(
        context: &SemanticCandidateContext,
        embedding: &[f32],
        fetch_limit: usize,
        fs_filter: Option<&dyn FsSearchFilter>,
    ) -> Result<(Vec<VectorSearchResult>, SemanticCandidateRetryState)> {
'''
WRAPPER = SIGNATURE + '''        // A resolved raw window cannot certify exhaustion of a retained WAL
        // view. Select current main+delta messages before that shortcut, using
        // the exact cold lane when optimized width/WAL admission declines.
        if context.artifacts.iter().any(|artifact| artifact.index().wal_record_count() > 0) {
            let record_count = context.artifacts.iter().fold(0usize, |total, artifact| {
                total.saturating_add(artifact.index().record_count())
                    .saturating_add(artifact.index().wal_record_count())
            });
            let return_limit = Self::semantic_exact_candidate_limit(fetch_limit, record_count);
            if let Some(hits) = message_stream::try_collect_retained_messages(
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

    // Retain the incumbent driver for unsupported no-WAL row-score widths,
    // and for same-binary, real-engine differential tests.
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
OLD_CALL = 'message_stream::try_collect_exact_messages('
NEW_CALL = 'message_stream::try_collect_retained_messages('


def verify_scope(text: str) -> tuple[int, int]:
    start = text.index('    fn search_exact_semantic_indexes(')
    end = text.index('    fn search_exact_semantic_indexes_with_refinement(', start)
    body = text[start:end]
    if '.wal_record_count()' not in body or body.count(OLD_CALL) + body.count(NEW_CALL) != 1:
        raise ValueError('Retained-WAL wrapper does not have the expected single collector')
    return start, end


def integrate(text: str) -> str:
    counts = [text.count(marker) for marker in [
        'mod message_stream;', 'fn search_exact_semantic_indexes_with_refinement(', OLD_CALL, NEW_CALL]]
    if counts == [1, 1, 1, 1]:
        start, end = verify_scope(text)
        if NEW_CALL not in text[start:end]:
            raise ValueError('Retained collector is outside the WAL wrapper')
        return text
    if counts == [1, 1, 2, 0]:
        # A separately completed first integration may have been rustfmt'd.
        # Upgrade only its scoped callee; never reapply a stale whole function.
        start, end = verify_scope(text)
        return text[:start] + text[start:end].replace(OLD_CALL, NEW_CALL, 1) + text[end:]
    if any(counts):
        raise ValueError('Partial or duplicate exact-message integration; refusing to rewrite')
    result = text
    for old, new in EDITS:
        if result.count(old) != 1:
            raise ValueError(f'Expected exactly one original anchor: {old!r}')
        result = result.replace(old, new, 1)
    verify_scope(result)
    return result


FIXTURE_OLD = '''    drop(write_artifact(&path, 32, Quantization::F16, &rows)?);
    let mut writer = FsVectorIndex::open_writer(&path)?;
    writer.append_batch(&[(doc(1, 0, 3), vector(32, -1.0))])?;
    drop(writer);
    let ctx = context(vec![SemanticIndexArtifact::open(&path, None)?]);
    let index = ctx.artifacts[0].index();
    assert_eq!(index.record_count(), 33);
'''
FIXTURE_NEW = '''    drop(write_artifact(&path, 32, Quantization::F16, &rows)?);
    // Construct the retained-log-before-cleanup image explicitly. A completed
    // append on the pinned backend did NOT reproduce the false-empty baseline
    // (CI 37353320475). Produce a genuine WAL on an identical sibling; keep the
    // original main rows unchanged instead of assuming cleanup left them live.
    let producer_path = tmp.path().join("wal-producer.fsvi");
    std::fs::copy(&path, &producer_path)?;
    let mut writer = FsVectorIndex::open_writer(&producer_path)?;
    writer.append_batch(&[(doc(1, 0, 3), vector(32, -1.0))])?;
    drop(writer);
    let completed = FsVectorIndex::open_read_only(&producer_path)?;
    assert!(!completed.search_top_k(&vector(32, 1.0), 4, None)?.is_empty(),
        "completed append is the control, not the retained-log cut fixture");
    drop(completed);
    std::fs::copy(frankensearch::index::wal_path_for(&producer_path),
        frankensearch::index::wal_path_for(&path))?;
    let ctx = context(vec![SemanticIndexArtifact::open(&path, None)?]);
    let index = ctx.artifacts[0].index();
    assert_eq!(index.record_count(), 33);
    assert_eq!(index.tombstone_count(), 0, "original main cleanup has not occurred");
    assert_eq!(index.wal_record_count(), 1, "the genuine retained delta must be admitted");
'''
FIXTURE_COMMENT_OLD = '''    // These are valid v1 physical duplicate rows, not forged bytes. The
    // writer's best-effort tombstone can leave older duplicates live while
    // the durable WAL already supersedes their entire document identity.
'''
FIXTURE_COMMENT_NEW = '''    // Retained images can contain a replacement WAL beside older main rows.
    // Reuse complete production-serialized artifacts; do not edit flags, CRCs,
    // generations, or source bytes to manufacture the expected verdict.
'''


def integrate_fixture(text: str) -> str:
    marker = 'let producer_path = tmp.path().join("wal-producer.fsvi");'
    if text.count(marker) == 1:
        if 'original main cleanup has not occurred' not in text or 'genuine retained delta must be admitted' not in text:
            raise ValueError('Partial retained-WAL fixture upgrade')
        return text
    if text.count(FIXTURE_OLD) != 1 or text.count(FIXTURE_COMMENT_OLD) != 1:
        raise ValueError('Retained-WAL fixture changed concurrently; refusing to rewrite')
    return text.replace(FIXTURE_OLD, FIXTURE_NEW, 1).replace(FIXTURE_COMMENT_OLD, FIXTURE_COMMENT_NEW, 1)


def main() -> None:
    before = PATH.read_bytes()
    tests_before = TEST_PATH.read_bytes()
    after = integrate(before.decode()).encode()
    tests_after = integrate_fixture(tests_before.decode()).encode()
    assert integrate(after.decode()).encode() == after
    assert integrate_fixture(tests_after.decode()).encode() == tests_after
    for path, old, new in [(PATH, before, after), (TEST_PATH, tests_before, tests_after)]:
        print(f'{path}: input_sha256={hashlib.sha256(old).hexdigest()}')
        print(f'{path}: output_sha256={hashlib.sha256(new).hexdigest()}')
        if old != new:
            path.write_bytes(new)
        print(f'{path}: changed={old != new}')


if __name__ == '__main__':
    main()
