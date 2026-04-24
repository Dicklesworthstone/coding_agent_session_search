# Pass 3/10 - Rule-of-3 Helper Extraction

## Candidate Accepted

- **File:** `src/ui/data.rs`
- **Lever:** extract the repeated conversation-row projection used by three UI conversation loaders.
- **Spans:** `load_conversation_by_id_uncached`, `load_conversation_uncached`, and `load_conversation_for_hit` all map the same 15-column SQL row into `(conversation_id, Conversation, Option<Workspace>)`.
- **Score:** `(LOC_saved 3 x Confidence 5) / Risk 1 = 15.0`

## Isomorphism Card

### Equivalence contract

- **Inputs covered:** the shared SELECT column order for the three UI conversation loaders: conversation id, agent slug, workspace id/path/name, external id, title, source path, timestamps, token estimate, JSON metadata, source identity, origin host, and binary metadata.
- **Ordering preserved:** yes. Each query still executes at the same callsite, returns rows in the same SQL order, and iterates collected rows in the same sequence.
- **Tie-breaking:** unchanged. SQL `ORDER BY` clauses and `LIMIT` clauses were not changed.
- **Error semantics:** unchanged. `row.get_typed(...)` failures still propagate through `query_map_collect`; `display_name` still uses `row.get_typed(4).ok().flatten()` and therefore still suppresses that one optional display-name read failure.
- **Laziness:** unchanged. `query_map_collect` still materializes the same row vector before message fetches.
- **Short-circuit eval:** unchanged. The helper returns the same first row for `load_conversation_*` single-result paths and the same per-row values for the `load_conversation_for_hit` loop.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side-effects:** unchanged. The row projection has no logs, metrics, I/O, DB writes, cache writes, or message fetches; message fetches still occur after row collection in the same caller order.
- **Type narrowing:** unchanged. Public functions and return types stay the same; the helper is private to `src/ui/data.rs`.
- **Rerender behavior:** unchanged. The resulting `ConversationView` values retain the same fields, cache keys, workspace values, metadata fallback, and normalized source identities.

### Verification plan

- `rustfmt --edition 2024 --check src/ui/data.rs`
- `git diff --check -- src/ui/data.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass3_rule_of_3_helper.md`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test ui::data --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings`

### Verification results

- `rustfmt --edition 2024 --check src/ui/data.rs` - passed after correcting import order.
- `git diff --check -- src/ui/data.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass3_rule_of_3_helper.md` - passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test ui::data --lib` - passed: 47 passed, 0 failed, 4073 filtered out.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets` - passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings` - failed on unrelated `src/search/query.rs:5859` (`clippy::assertions-on-constants`).
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants` - passed.

## LOC Ledger

- `src/ui/data.rs`: 49 insertions, 121 deletions, net -72 LOC.

## Inspected But Rejected For This Pass

- `src/indexer/semantic.rs` scheduler stop branches: already route through `stopped_scheduler_decision`; a further helper would add condition/state/reason plumbing and save little.
- `src/indexer/semantic.rs` packet embedding batch builders: two main spans plus specialized filtering; not a clean rule-of-3 extraction.
- `src/pages/deploy_*` path-copy safety blocks: similar shape across modules, but not local to one bounded module and carries filesystem side-effect risk.
