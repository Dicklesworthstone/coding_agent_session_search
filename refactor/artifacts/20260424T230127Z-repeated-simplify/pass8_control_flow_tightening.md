# Pass 8/10 - Local Control-Flow Tightening

## Candidate

Tighten the `encryption.chunk_size` validation branch in
`src/pages/config_input.rs::ConfigInput::validate`.

The current branch is a nested `if let Some(chunk_size)` plus
`if chunk_size == 0 { ... } else if chunk_size > MAX_CHUNK_SIZE { ... }`.
The candidate collapses that local branch into one `match` over
`self.encryption.chunk_size`.

## Equivalence Contract

- **Inputs covered:** `None`, `Some(0)`, `Some(1..=MAX_CHUNK_SIZE)`, and
  `Some(MAX_CHUNK_SIZE + 1..)`.
- **Ordering preserved:** yes. Validation still reaches the chunk-size branch
  after time-format validation and before warnings.
- **Tie-breaking:** unchanged. `0` still reports only the greater-than-zero
  error, not the oversized error.
- **Error semantics:** unchanged. The same strings are pushed for zero and
  oversized chunk sizes.
- **Laziness:** unchanged. `MAX_CHUNK_SIZE` is only consulted by the guarded
  `Some(chunk_size)` arm after `None` and `Some(0)` fail to match.
- **Short-circuit eval:** unchanged in effect. `None` still performs no local
  error check, and the `Some(0)` arm still prevents the oversized guard from
  applying to zero.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side-effects:** unchanged. The same `errors.push(...)` happens
  at the same validation point; no logs, metrics, DB writes, or CLI/JSON output
  schema are touched.
- **Public API:** unchanged. No types, function signatures, or exported fields
  change.

## Score

| Candidate | LOC | Confidence | Risk | Score | Decision |
|-----------|-----|------------|------|-------|----------|
| `ConfigInput::validate` chunk-size branch | 1 | 5 | 1 | 5.0 | Apply |

## Rejected Candidates

- `src/indexer/semantic.rs::semantic_backfill_scheduler_decision_for_capacity`:
  repeated `&& !signals.force` guards looked tempting, but reducing them would
  either add a broader helper or change the visible evaluation shape of the
  guard ladder.
- `src/indexer/lexical_generation.rs` protected-retention branch: clean nested
  conditional, but the file is a larger lifecycle surface with cleanup
  accounting and tracing nearby.
- `src/tui_asciicast.rs::ensure_parent_dir`: small nested `if`, but replacing
  it with an `Option::filter` chain would be more idiomatic than simpler and
  has weaker targeted coverage.
- `src/search/tantivy.rs` origin helpers: already addressed by pass 4, so
  excluded to keep this pass to a fresh lever.

## Verification Plan

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_simplify_loop cargo test pages::config_input --lib`
- `rustfmt --edition 2024 --check src/pages/config_input.rs`
- `git diff --check -- src/pages/config_input.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass8_control_flow_tightening.md`

## Fresh-Eyes Answer

Fresh-eyes review found one real issue in the first draft: matching only the
inner `chunk_size` value preserved behavior but added lines, which conflicted
with the simplification skill's net-negative intent. I changed the diff to
match directly on `self.encryption.chunk_size`, collapsing the outer `if let`
and inner branch cluster into one match.

I rechecked the preservation contract: `None` still pushes nothing, `Some(0)`
still pushes only the greater-than-zero error, oversized values still push the
same formatted maximum-size error, valid nonzero sizes still push nothing, and
the branch remains in the same validation order before warnings.

## Verification Results

- `rustfmt --edition 2024 --check src/pages/config_input.rs` passed.
- `git diff --check -- src/pages/config_input.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass8_control_flow_tightening.md` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_simplify_loop cargo test pages::config_input --lib` passed: 18 passed.
- `git diff --numstat -- src/pages/config_input.rs` reports 5 insertions and
  7 deletions, net -2 lines in the touched Rust source.
