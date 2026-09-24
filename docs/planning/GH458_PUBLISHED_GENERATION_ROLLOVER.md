# GH #458: retain a published generation during passage-contract replacement

## Scope and implementation status

This is an implementation contract and a set of regression **guardrails**, not
an availability fix. `tests/semantic_rollover_guardrails.rs` exercises existing
ledger persistence and write-compatibility boundaries. It does not exercise a
successful CLI query during storage backfill, and does not close #458.

Inspected source: `722c2a6ca23af7dd054d1ae0d7e1a9942ce263d7` on `main`.
The Rust target has not been compiled or executed in the authoring environment:
`cargo`/`rustc` are unavailable there, and the local Git transport cannot resolve
GitHub. Repository reads and the direct-main write use the GitHub connector.
No passing-test or production-qualification claim is attached to this change.

**Status update (checked 2026-09-24).** Section 5 has landed. A proved zero delta
retains the published generation without packet replay or republication.
`gh458_unchanged_backfill_preserves_completed_generation_without_replay` in
`src/indexer/semantic/engine.rs` and
`gh458_robot_unchanged_maintenance_skips_packet_replay_and_publication` in
`tests/e2e_semantic_backfill_robot.rs` cover it. The latter asserts
`status: "unchanged"` with byte-identical vector files and a single `complete`
progress event, and asserts that a same-count content edit still republishes.
Further `gh458_*` library tests cover vector reuse across ingest and same-ID
edit/deletion reconciliation. Serving the retained generation during a
contract replacement (sections 1-4) and the end-to-end table below are not
established here. GitHub closed #458 on 2026-09-19 because a commit message
contained "Does not close #458". The runtime rollover is tracked by bead
`coding_agent_session_search-962e8`.

## Production evidence and exact failure boundary

The [nskidan report](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/458#issuecomment-5735975241)
describes a complete MiniLM index with 433,605 vectors across 4,812 conversations
being refused after the input contract changed. The native model revision stayed
`c9745ed1d9f207416be6d2e6f8de32d1f16199bf`; the expected FSVI revision gained
`:passages-v2`. Its replacement started from zero and was only 23% complete after
roughly 12 CPU hours. This is not evidence that those old vectors acquired a new
embedding geometry. It is also not proof that they can be reused by a writer
with a different passage contract.

The separate [zero-delta report](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/458#issuecomment-5739737960)
found a fallback scan embedding zero documents while rewriting publication and
leaving HNSW unready. The cache miss itself was not explained. A cache miss is
not proof that the selected generation is invalid; conversely, an embedding
count of zero is not proof of no change (deletions are a counterexample).

Relevant source boundaries at the inspected commit:

- `src/search/fastembed_embedder.rs`: the revision suffix deliberately fences
  the old document-input representation from current writes.
- `src/search/model_manager.rs`: `validate_vector_index_contract` is strict and
  is also used by `needs_index_rebuild`. Its query use cannot simply be loosened
  without accidentally loosening rebuild admission. `refuse_stale_semantic_assets`
  protects archive/filter identity, including the actually served tier rather
  than allowing the other tier's readiness to vouch for it.
- `src/indexer/semantic.rs`: both fresh storage backfill and fallback reconciliation
  call `revoke_backfill_serving`; this clears the published tier/HNSW readiness
  and invalidates matching shard readiness. Staging files alone therefore do not
  provide continuous serving. `finish_backfill_batch` replaces the fixed FSVI
  path before it publishes the mutable manifest record; two individually atomic
  writes do not constitute one generation-selection transaction.
- `src/search/semantic_manifest.rs`: checkpoint/backlog updates already need not
  overwrite published artifact records. Immutable generation manifests and a CAS
  selection pointer also exist. However, `selected_generation_owner_requirement`
  in `model_manager.rs` deliberately refuses a `current.json` selection until
  the legacy query path can consume its retained owners. Publishing that pointer
  alone is not a working rollout.
- Query setup builds filter maps from the live archive. An opened vector owner,
  a count/tail fingerprint, and a separately opened archive are not by themselves
  a generation-consistent hydration contract.

## Smallest correct serving behavior

### 1. Separate published state from replacement work

Keep exactly one selected, complete generation and at most one in-progress
replacement for this rollout. Do not introduce a general scheduling system.

Starting, checkpointing, resuming, cancelling, or failing the replacement must
not change the selected generation's paths, input contract, completed timestamp,
coverage, or ANN association. It remains searchable **only while independently
admissible**. With no prior complete generation, partial staging is unavailable.
Build progress may say `building` while `can_search` remains true for the selected
generation; readiness must not be inferred from a 100% progress counter.

Do not preserve a generation merely because an old ledger flag says `ready`.
Retain validated file owners and the complete selection metadata that vouched
for those particular artifacts. Do not silently resurrect a previously revoked
or incomplete legacy artifact during migration.

### 2. Distinguish query compatibility from build/reuse compatibility

For the reported transition, explicitly recognize the known old native MiniLM
query space and its document-input contract. Same dimension, the `minilm-384`
name, or removing arbitrary suffixes is not sufficient admission. Model weights,
engine/producer identity, tokenizer, pooling, normalization, and query preparation
must still match an explicitly supported query contract.

The old prefix-only document representation and `passages-v2` are distinct.
For long messages, old chunk zero is not automatically the same text as new
chunk zero. Interpret and validate retained hits using the **selected** input
contract, not whichever passage function the replacement writer currently uses.

Keep `needs_index_rebuild`, checkpoint compatibility, append admission, and vector
reuse strict. A readable old generation may still require a complete replacement.
Never relabel its FSVI header or append new-contract vectors to it as a shortcut.

### 3. Preserve hydration and filtering correctness

The retained generation must not produce a hit whose old score is attached to a
changed message, reused message ID, different agent/source/workspace/role, or a
deleted message. The existing same-ID-edit and canonical-identity refusal tests
must not simply be deleted or inverted.

The bounded implementation can retain immutable vector/ANN owners and validate
candidate identity, source/provenance, and content against one query-time archive
snapshot, using the selected document-input contract. Changed/deleted candidates
must be excluded and candidate selection/refill must account for exclusions so
that stale candidates cannot consume the entire requested page. Filter-map and
hydration identity must agree with that snapshot. Hashless/ambiguous legacy
records require an additional validated binding or refusal, not optimism.

Alternatively, pinned hydration data requires an explicit live invalidation
check; serving an old database snapshot alone must not resurrect deleted rows.
Until one of these bindings is implemented and tested through real queries,
retain the current fail-closed staleness behavior.

### 4. Publish one complete replacement, once

Use the existing immutable-generation/CAS machinery rather than a second,
unrelated generation format. First wire its validated owners into query setup;
do not remove `OwnerBackedReaderRequired` merely to reopen selected pathnames.

Build under a private generation path. Validate full selected-document coverage,
unique identities, producer/input contracts, all vector values, and any optional
ANN/base-index binding; then flush artifacts and the sealed manifest before the
single selection-pointer CAS. Do not mutate files owned by the old generation.
An ANN is an optional accelerator: an explicitly reported exact-search generation
is preferable to attaching an old graph to a new FSVI.

Readers that started before the swap retain their original owners. New readers
select the replacement as one unit. Clear replacement progress after selection;
a stale checkpoint after a crash is recoverable bookkeeping, not a reason to
withdraw a durable selected generation. Keep the old files while readers own
them. CAS failure must not overwrite a newer publisher's selection.

Audit the CLI maintenance-lock path too. A correct loader is not sufficient if
an unrelated exclusive maintenance gate still rejects every concurrent search.
Separate build work from the short publication critical section without disabling
locks required for archive migration, identity repair, or destructive maintenance.

### 5. Treat a proved zero delta as a no-op

For an unchanged producer/input contract, if canonical reconciliation and
artifact validation prove no additions, deletions, metadata/content changes,
vector/WAL changes, or coverage changes, retain the original generation and its
valid ANN. Report `unchanged: true`; refresh only the advisory completed-build
cache. Do not republish merely because the cache was absent or corrupt.

This predicate must be stronger than `embedded_docs == 0` and stronger than the
coarse `content-v1` count/tail fingerprint. The existing same-ID-edit regression
explicitly exercises changes that leave that fingerprint unchanged. Invalid ANN
state must not be made ready by restoring a saved Boolean.

## Coverage landed here versus coverage still required

The new Rust target contains five guardrail tests:

1. Checkpoint save/reload/progress under the new chunking contract preserves the
   old published quality record, completion time, and ANN record byte-for-value.
2. Abandoning replacement progress does not erase published ledger metadata.
3. Count-based initial-build progress, without cursor exhaustion, does not invent
   a published artifact or ANN.
4. Real FSVI files remain rebuild-incompatible for the legacy passage contract,
   unknown suffixes, different producer/model/embedder identities, and wrong
   dimensions; the rebuild inspection preserves their FSVI bytes.
5. A real prefix-contract hash FSVI rejects current-contract append and retains
   its original bytes and record count.

These tests intentionally **do not** claim successful query fallback. Before the
serving change can be called complete, add and run these end-to-end regressions:

| Scenario | Required observation |
| --- | --- |
| Known MiniLM passage-only transition | Real semantic and hybrid queries use the complete old generation during bounded replacement batches; current writer still rebuilds. |
| Long-message chunk-zero contract change | Retained hits hydrate under the old input contract; new input hashes are never substituted for old ones. |
| Same-ID edit, deletion, and provenance change | No resurrected, misattributed, or wrong-content hits; candidate refill handles invalidated high-ranking rows. |
| Interrupt/restart and failed candidate validation | Selection, old artifact bytes, ANN owner, completion time, and query results remain tied to the old generation. |
| Crash before/after seal and pointer CAS | Selection resolves to one complete generation, never a mixed FSVI/ANN/manifest set; publication conflicts preserve the winner. |
| In-flight query across publication | Old query retains old owners; a new query receives the replacement and correct hydration/filter binding. |
| Cache miss with proved zero delta | No FSVI rewrite, completion-time reset, or valid-HNSW withdrawal; `unchanged` is true. |
| No complete prior generation or incompatible/corrupt prior artifact | Partial work and unsupported producer contracts remain unavailable, with truthful exact/lexical fallback. |
| Search during CLI maintenance/backfill | Concurrent query succeeds when the retained generation is admissible, rather than failing at an earlier lock gate. |

Suggested repository-side validation commands (not executed here):

```sh
cargo test --test semantic_rollover_guardrails
cargo test --lib gh458_
cargo test --lib gh470_prefix_checkpoint_restarts_complete_passage_coverage
cargo test --test e2e_semantic_backfill_robot gh458_
cargo fmt --check
```

The new target does not download MiniLM weights. Full model-backed serving,
concurrency/crash coverage, existing owner-backed artifact tests, and the normal
repository qualification gates remain necessary before claiming the outage fixed.
