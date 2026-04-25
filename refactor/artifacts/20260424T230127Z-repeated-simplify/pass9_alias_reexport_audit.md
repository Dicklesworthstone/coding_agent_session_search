# Pass 9: Alias/Re-Export Audit

## Candidate

Inline the private `LexicalRebuildMessageBatch` alias in `src/indexer/mod.rs`.

The alias is:

```rust
type LexicalRebuildMessageBatch = Vec<LexicalRebuildConversationPacket>;
```

It is used only in `src/indexer/mod.rs` for two function parameters and one local variable type. It does not cross a public module boundary.

## Equivalence Contract

- Inputs covered: streamed lexical rebuild batch flushing and the local pending batch allocation in `rebuild_lexical_index_from_storage_with_recovery`.
- Ordering preserved: yes. The underlying container remains `Vec<LexicalRebuildConversationPacket>`.
- Tie-breaking: unchanged; no ranking or comparison logic changes.
- Error semantics: unchanged; function bodies and return paths stay identical.
- Laziness: unchanged; the same eager `Vec` allocation and mutation paths remain.
- Short-circuit eval: unchanged; conditionals are not modified.
- Floating-point: N/A.
- RNG / hash order: unchanged; no hash/RNG logic changes.
- Observable side-effects: unchanged; logging, index writes, commits, and metrics stay in the same code paths.
- Type narrowing: Rust-only private alias expansion; no public API type name is removed.
- Public module paths: unchanged; no `pub use`, connector stub, or exported storage/search alias is removed.

## Score

- LOC saved: 1
- Confidence: 5
- Risk: 1
- Score: 5.0
- Decision: accept. This is a pure private alias expansion with no runtime representation change.

## Rejected Candidates

- `src/storage/sqlite.rs`: `pub type SqliteStorage = FrankenStorage`. Rejected because it is explicitly retained for compatibility and used broadly by downstream tests and benches.
- `src/search/tantivy.rs`: `pub type Fields` / `pub type MergeStatus`. Rejected because they are public aliases on the search module surface and preserve downstream import compatibility.
- Connector re-export/stub modules. Rejected by mission constraint to preserve public module paths and not remove connector stubs.
- `std::fmt::Write as _` and similar trait imports. Rejected because they intentionally bring extension traits into scope without a local binding.

## Verification Plan

- `rg -n "LexicalRebuildMessageBatch" src/indexer/mod.rs` should return no matches after the edit.
- `rustfmt --edition 2024 --check src/indexer/mod.rs`
- Targeted compile check for the touched module surface: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_pass9_alias cargo check --lib`
- `git diff --check -- src/indexer/mod.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass9_alias_reexport_audit.md`

## Fresh-Eyes Answer

Fresh-eyes review completed after the edit. `rg -n "LexicalRebuildMessageBatch" src/indexer/mod.rs` returned no matches, and the diff only removes the private alias plus expands the same `Vec<LexicalRebuildConversationPacket>` type at the two function parameters and one local binding. I did not find a real bug to fix. The equivalence contract is preserved because there is no runtime representation, control-flow, side-effect, public path, or public API change.

## Coordination Note

Agent Mail granted `src/indexer/mod.rs` to `BlueBluff`. The artifact path conflicted with `CrimsonCastle`'s broad `refactor/artifacts/20260424T230127Z-repeated-simplify/*` reservation; direct messaging was contact-gated, so a contact request was sent before creating this pass-specific artifact.
