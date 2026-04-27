# Pass 4 - Semantic Fixture Surface

## Change

Extract `test_conversation_fixture(...)` so the semantic tests share one `Conversation` fixture body while the local and remote provenance wrappers keep their distinct call-site intent.

## Score

| LOC saved | Confidence | Risk | Score |
|---:|---:|---:|---:|
| 3 | 5 | 1 | 15.0 |

## Equivalence Contract

- Inputs covered: single-message local fixtures and multi-message remote fixtures.
- Ordering preserved: message vectors are passed through unchanged.
- Tie-breaking: N/A.
- Error semantics: unchanged; fixture construction is infallible.
- Laziness: unchanged; callers still build the same `Vec<Message>` before fixture construction.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG/hash order: N/A.
- Observable side effects: unchanged; tests get the same source path, timestamps, metadata, source id, and origin host.
- Type narrowing: N/A.

## Fresh-Eyes Review

I re-read both removed struct literals against the shared constructor. The common fields are byte-for-byte equivalent in value: `agent_slug`, `workspace`, `external_id`, `title`, `source_path`, `started_at`, `ended_at`, `approx_tokens`, and `metadata_json`. The local wrapper still supplies `source_id = "local"` and no origin host; the remote wrapper still supplies `source_id = "remote-laptop"` and `origin_host = "builder-host"`.

## Verification

- Passed: `rustfmt --edition 2024 --check src/indexer/semantic.rs`
- Passed: `git diff --check -- src/indexer/semantic.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass4_semantic_conversation_fixture.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib indexer::semantic::tests::semantic_inputs_from_packets_matches_storage_replay -- --exact`
