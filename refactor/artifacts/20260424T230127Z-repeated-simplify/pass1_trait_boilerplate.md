# Pass 1/10 - Trait Boilerplate Derives

## Candidate

- File: `src/daemon/protocol.rs`
- Lever: replace hand-written `Display` + empty `std::error::Error` impls for the daemon protocol string-newtype errors with `thiserror::Error` derives.
- Scope: `EncodeError` and `DecodeError` only. They are symmetric public tuple wrappers in the same protocol module and keep the same constructors, fields, `Debug`, and `Clone` derives.
- Score: `(LOC_saved 2 * Confidence 5) / Risk 1 = 10.0`

## Isomorphism Card

### Equivalence contract

- **Inputs covered:** `EncodeError(String)` and `DecodeError(String)` for representative payload strings; existing encode/decode protocol tests still exercise the same result types.
- **Ordering preserved:** N/A. No iteration or ordering behavior changed.
- **Tie-breaking:** N/A.
- **Error semantics:** Same concrete public error types, same tuple field visibility, same `Debug` and `Clone`, same `Display` strings (`encode error: {msg}` / `decode error: {msg}`), and `std::error::Error::source()` remains `None` because no `#[source]` or `#[from]` field was added.
- **Laziness:** N/A.
- **Short-circuit eval:** N/A.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side-effects:** None. No logging, metrics, I/O, serialization, wire format, or daemon protocol payload changed.
- **Type narrowing:** The Rust public API remains `pub struct EncodeError(pub String)` and `pub struct DecodeError(pub String)`.
- **Rerender behavior:** N/A.

### Verification plan

- `rustfmt --edition 2024 --check src/daemon/protocol.rs`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test daemon::protocol --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`
- `git diff --check -- src/daemon/protocol.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass1_trait_boilerplate.md`
- `cargo fmt --check`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings`

### Verification results

- `rustfmt --edition 2024 --check src/daemon/protocol.rs` passed.
- `git diff --check -- src/daemon/protocol.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass1_trait_boilerplate.md` passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test daemon::protocol --lib` passed: 9 passed, 0 failed, 4111 filtered out.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets` passed.
- `cargo fmt --check` failed on pre-existing unrelated formatting drift in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, `tests/metamorphic_agent_detection.rs`, and `tests/metamorphic_stats.rs`; `src/daemon/protocol.rs` is clean under the focused rustfmt check.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings` failed on unrelated `src/search/query.rs:5859` (`clippy::assertions-on-constants`).
- Diagnostic clippy passed with only that unrelated lint allowed: `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants`.

## LOC Ledger

- `src/daemon/protocol.rs`: 15 insertions, 18 deletions, net -3 LOC including the new preservation test.

## Inspected But Rejected For This Pass

- `src/analytics/types.rs::AnalyticsError`: also candidate-shaped, but it lives on a broader analytics-facing surface with user-facing command guidance text and an em dash; lower risk to leave for a dedicated analytics pass.
- `src/sources/interactive.rs::InteractiveError`: multi-line terminal help string has whitespace and indentation sensitivity; not the lowest-risk first derive target.
- `src/sources/setup.rs::SetupError`: wraps several nested error types; a derive is possible, but source-chain semantics deserve a dedicated pass instead of mixing into this one.
