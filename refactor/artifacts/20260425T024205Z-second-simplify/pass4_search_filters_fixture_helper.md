# Pass 4/10 - Rule-of-3 Test Fixture Helper

## Isomorphism Card

### Change

Extract the repeated `index.commit().unwrap(); SearchClient::open(dir.path(), None).unwrap().expect("client")` test fixture step in `tests/search_filters.rs` into private `commit_and_open_client`.

### Equivalence Contract

- Inputs covered: the four `SearchClient` fixture openings in `agent_filter_limits_results`, `workspace_filter_limits_results`, `time_filter_respects_since_until`, and `minimal_field_mask_preserves_order`.
- Ordering preserved: yes. Every test still adds conversations before committing, and opens the client immediately after the same commit.
- Tie-breaking: unchanged / N/A.
- Error semantics: unchanged. `commit()` still uses `unwrap()`, `SearchClient::open` still uses `unwrap()`, and the missing-client case still uses `expect("client")`.
- Laziness: unchanged. The helper is called at the exact original point in each test.
- Short-circuit evaluation: unchanged. A commit failure still panics before attempting to open the client.
- Observable side effects: identical Tantivy commit and client-open side effects.
- Public API / schema: unchanged. Test-only private helper.

### Candidate Score

- LOC saved: 7
- Confidence: 5
- Risk: 1
- Score: 35.0
- Decision: accept. This is a repeated local test fixture step with identical panic messages and sequencing.

## Files Changed

- `tests/search_filters.rs`: added private `commit_and_open_client` and replaced four repeated commit/open spans.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass4_search_filters_fixture_helper.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read each replacement against the original sequence: `add_conversation` calls still precede the helper, and the helper commits before opening.
- Confirmed no filter construction moved across the commit/open boundary.
- Confirmed the exact `expect("client")` message is preserved.
- Confirmed the helper accepts `&mut TantivyIndex`, matching the original mutable commit receiver.

## Verification

- `rustfmt --edition 2024 --check tests/search_filters.rs`
  - Result: passed after applying rustfmt's line wrapping in the helper.
- `git diff --check -- tests/search_filters.rs refactor/artifacts/20260425T024205Z-second-simplify/pass4_search_filters_fixture_helper.md`
  - Result: passed with no output.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --test search_filters`
  - Result: passed, `59 passed; 0 failed`.
