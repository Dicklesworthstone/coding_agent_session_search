# Pass 5/10 - Error Mapping Simplification

## Candidate Chosen

Collapse the repeated analytics query-exec error `Check` construction in
`src/analytics/validate.rs`.

### Score

| Candidate | LOC | Confidence | Risk | Score | Decision |
|-----------|-----|------------|------|-------|----------|
| `validate_track_{a,b}` query-exec `Check` construction | 3 | 5 | 1 | 15.0 | Apply |

## Isomorphism Card

### Change

Extract the shared failed-query `Check` constructor into a private
`query_exec_error_check(...)` helper.

### Equivalence Contract

- Inputs covered: the two `query_executes(...)` failure branches and the two `query_map_collect(...)` failure branches in Track A and Track B validation.
- Ordering preserved: yes. Each branch still pushes one `Check` and immediately returns the same tuple.
- Tie-breaking: N/A.
- Error semantics: unchanged. Each failure still emits `ok=false`, `severity=Error`, the same `id`, the same `details` prefix plus underlying error text, and the same suggested action.
- Laziness: N/A.
- Short-circuit eval: unchanged. The helper is only called in the same already-failed branches.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: unchanged. There were no logs or metrics in these mappings; the `checks.push(...)` happens at the same points.
- Type narrowing: N/A.
- Public API: unchanged. All public structs and function signatures remain the same.

### Verification Planned

- `rustfmt --edition 2024 --check src/analytics/validate.rs`
- `git diff --check -- src/analytics/validate.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass5_error_mapping.md`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test analytics::validate --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`

## Rejected Candidates Inspected

- `src/daemon/worker.rs`: three identical `worker channel closed` mappings exist, but a helper plus behavior-pinning test grows the file and does not satisfy the simplification threshold.
- `src/analytics/query.rs`: repeated `AnalyticsError::Db(format!("... query failed: {e}"))` mappings exist, but the file has many query families with user-facing context strings. A safe extraction would need a broader analytics query taxonomy pass.
- `src/sources/sync.rs`: repeated SFTP path error strings exist, but they interleave remote/local path semantics and filesystem side effects. The savings were not worth touching sync behavior in this focused pass.
- `src/daemon/client.rs`: repeated daemon request error wrapping exists, but it mixes availability, timeout, failed protocol encoding/decoding, connection invalidation, and spawn-lock behavior. Not a single low-risk error-mapping lever.

## Verification Results

- `rustfmt --edition 2024 --check src/analytics/validate.rs` passed.
- `git diff --check -- src/analytics/validate.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass5_error_mapping.md` passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test analytics::validate --lib` passed: 22 passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets` passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings` failed on unrelated `src/search/query.rs:5859` (`clippy::assertions-on-constants`).
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants` passed.
- `cargo fmt --check` still fails on unrelated test formatting drift outside this pass.
- `ubs src/analytics/validate.rs` completed with 0 critical issues; its formatter, clippy, check, and test-build subchecks were clean. It reported pre-existing heuristic warnings in the file's tests.

## LOC Ledger

- `src/analytics/validate.rs`: 30 insertions, 38 deletions, net -8 lines.
