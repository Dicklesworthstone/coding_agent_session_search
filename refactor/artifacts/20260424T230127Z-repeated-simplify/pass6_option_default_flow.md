## Pass 6/10 - Option/Default Flow Simplification

### Mission
Find redundant `Option`, `Default`, builder-style branching, or equivalent
fallback flow in one bounded `src/` module and simplify it while preserving
exact defaults, laziness, error semantics, logs, public API, and output shapes.

### Selected Candidate
`src/pages/bundle.rs` had a manual `Default` implementation for
`BundleBuilder`:

```rust
impl Default for BundleBuilder {
    fn default() -> Self {
        Self::new()
    }
}
```

`BundleBuilder` has one field, `config: BundleConfig`, and `BundleConfig`
already implements `Default`. Deriving `Default` on `BundleBuilder` therefore
constructs the same value without routing through the trivial `new()` wrapper.

### Isomorphism Card

#### Change
Replace the hand-written `Default` impl for `BundleBuilder` with
`#[derive(Default)]`.

#### Equivalence contract
- **Inputs covered:** all `BundleBuilder::default()` and
  `BundleBuilder::new()` callers in `src/pages/bundle.rs`,
  `src/pages/key_management.rs`, `src/pages/wizard.rs`, and `src/lib.rs`.
- **Ordering preserved:** yes. `Default::default()` still creates exactly one
  `BundleConfig::default()` value before any builder setters run.
- **Tie-breaking:** unchanged / N/A.
- **Error semantics:** unchanged. Construction is infallible before and after.
- **Default values:** unchanged. The only field uses the same
  `BundleConfig::default()` implementation as `Self::new()`.
- **Laziness:** unchanged. No lazy closure or deferred I/O was involved.
- **Short-circuit eval:** unchanged / N/A.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side effects:** unchanged. No logs, metrics, DB writes, or
  filesystem operations are in this constructor.
- **Public API / output shapes:** unchanged. `BundleBuilder::default()`,
  `BundleBuilder::new()`, and all builder methods remain available with the
  same types.

#### Score
- LOC saved: 4 (small but real, in production code)
- Confidence: 5 (single-field builder, field default unchanged)
- Risk: 1 (single-field derive, constructor is pure and infallible)
- Score: 20.0

### Checked But Left Alone
- `src/indexer/semantic.rs` positive `usize` env resolvers: an extracted helper
  preserves behavior, but rustfmt makes the production diff net-positive
  (`+3` LOC), so it was rejected and reverted.
- `src/indexer/semantic.rs::env_backfill_min_capacity_pct()`: parses with
  `trim()` and clamps to `1..=100`; that contract is not the same as the
  positive `usize` fallback resolvers.
- `src/indexer/semantic.rs::build_hnsw_index(...)`: public `Option<usize>`
  parameters default independently to two frankensearch constants; extracting a
  helper would save no meaningful code and would obscure the public defaults.
- `src/update_check.rs::UpdateState::{load, load_async}`: sync and async read
  paths share parse/default behavior, but a shared helper did not produce a
  meaningful net-negative diff without coupling sync and async I/O.

### Verification Plan
- `rustfmt --edition 2024 --check src/pages/bundle.rs`
- `git diff --check -- src/pages/bundle.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass6_option_default_flow.md`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test pages::bundle --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`
- Exact clippy if feasible; if blocked by known unrelated current-tree lint,
  run diagnostic clippy allowing only that blocker.

### Verification Results
- `rustfmt --edition 2024 --check src/pages/bundle.rs` passed.
- `git diff --check -- src/pages/bundle.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass6_option_default_flow.md` passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test pages::bundle --lib` passed: 13 passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets` passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings` failed on unrelated current-tree `src/search/query.rs:5859` (`clippy::assertions-on-constants`).
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants` passed.
- `cargo fmt --check` still fails on unrelated test formatting drift in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, `tests/metamorphic_agent_detection.rs`, and `tests/metamorphic_stats.rs`.
- `ubs src/pages/bundle.rs` completed with clean fmt/clippy/check/test-build subchecks; it exits nonzero for pre-existing heuristic findings in tests and a false-positive "hardcoded secret" report on the `recovery_secret` setter.

### LOC Ledger
- `src/pages/bundle.rs`: 1 insertion / 6 deletions, net -5 production LOC.
