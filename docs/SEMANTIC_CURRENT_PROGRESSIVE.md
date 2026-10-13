# Progressive current-source retrieval

Retained and publication-selected readers expose
`progressive_current_text_in_transaction`. This combines lazy fast/quality
delivery with the current-source exclusion/refill operation. It is a library
API; it does not enable the ordinary or progressive CLI, change publication
admission, or bypass `OwnerBackedReaderRequired`.

```rust
use coding_agent_search::search::semantic_reader::publication::text::hydration::{
    PassageProjection,
    query::current::CurrentTextQuery,
};

let request = CurrentTextQuery::new(
    query_text,
    producers, // TextQueryProducers::Progressive { fast, quality }
    10,
    PassageProjection::RawPassagesV2,
);
let mut phases = selected.progressive_current_text_in_transaction(
    request,
    &transaction,
)?;
while let Some(result) = phases.next() {
    let phase = result?;
    consume(phase.verified());
}
drop(phases);
// The caller still owns transaction finalization, including after an error.
```

Both producers and tier availability are checked at construction, before query
inference. Inspecting the identity of a supplied lazy model can initialize it;
this API promises deferred quality **inference and retrieval**, not deferred
identity inspection or model loading.

The first step runs fast retrieval, excludes missing/changed passages and refills
until it has a verified Initial batch or reaches an error. Only the second step
runs quality inference/retrieval. It retains the actual fast-tier candidates and
reuses that ranking when the original filter and exclusion revision are unchanged.
A newly discovered stale quality-side passage invalidates the fast candidates:
both tier rankings are then recomputed under the expanded exclusion set before
refusion. No projected/fused result page is used to reconstruct native candidates.

Each selected producer's actual identity-bearing query response is cached only
within this request, including its overrides and producer checks. Refill and
refinement therefore do not rerun successful query inference. Cached embeddings
and bounded fast candidates/exclusions are released on completion, error,
unwinding, or iterator drop. Retained vector images are shared, not copied.
Dropping after Initial never starts quality work.

The original filter and exact serving/publication owners remain fixed. Exclusions
are complete passage identities, not message IDs. The caller must hold one stable
canonical transaction and keep filter behavior stable throughout the iterator.
The adapter does not begin, commit or roll back that transaction.

## Shared work and terminal outcomes

Admission requires room for `2 * limit` returned passages and at least two search
passes. The configured search count, excluded-passage/ID-byte limits, hydration
passage/source/output limits and deadline apply to the **entire stream**. Refill
passes and repeated source reads are charged again; successful Initial delivery
does not renew the account. Time between iterator polls counts too.

Stats on Initial and Refined are cumulative snapshots. Do not add them together.
Refined keeps the final filtered ranking and its execution receipts; counters
report request work, not whole-generation coverage or a recall guarantee.
`fast_tier_reuses` counts passes using the retained fast ranking. Its fast-shard
execution receipts refer to the original work that produced those candidates,
not another scan performed during refinement. Source verification and its byte
charges are never reused: the entire final phase is checked in the caller's
transaction even when its fast vector candidates were retained.

Missing or changed source passages can be excluded. Malformed rows, corrupt
owners, producer failures and budget exhaustion remain errors. An error or panic
terminates the continuation; later polls cannot restart it. An Initial result
already delivered remains Initial when refinement fails. After successful
Refined delivery, later polls return `None`, even after the deadline.

Native model, vector and database calls remain synchronous cooperative
boundaries. Late results are withheld; these limits do not forcibly interrupt
native code, bound whole-process memory, or prove archive-scale latency.

## Qualification

The new tests exercise the actual continuation plus real canonical FrankenSQLite
rows, FSVI files, and explicitly non-semantic CASS hash producers. They include a
quality-only missing winner, an independently filtered fusion comparison,
cross-phase limits, early drop, producer failure, and retained images after path
renames. The ANN case exercises exact fallback with no selected graph, not graph
acceleration. Run the unchanged shipping dependency graph:

```sh
cargo test --locked --lib search::semantic_reader::publication::text::hydration:: \
  -- --nocapture --test-threads=2
```

The complete-file regression target for fast-tier reuse is:

```sh
cargo test --locked --test semantic_current_fast_reuse -- --nocapture --test-threads=1
```

It executes the prior duplicate-scan control flow and the new continuation
against the same real FSVI images and compares both ranked results and filter
visits. It also checks source invalidation, quality-only omissions, original
scope, cumulative byte limits and retained images/ANN fallback receipts. These
are native regression assertions, not a claim that the author executed them.
A source commit or an unexecuted workflow is not a passing test receipt.

## Verify rank contributors before trimming the page

Current-source synchronous queries and progressive refinement verify the complete
union of each tier's bounded candidate window, not only the final displayed page.
A passage can lose the final reciprocal-rank-fusion competition while still
shifting the ranks of displayed passages. If that hidden contributor is missing
or stale, its complete passage ID is excluded and retrieval/refusion repeats.
Malformed hidden sources and exhausted budgets remain errors, not exclusions.

The public page is trimmed only after the whole candidate union is verified.
Returned-contributor coverage is then recomputed for that prefix; retrieved-tier
counts, scores, tier ranks, native execution receipts and publication owners are
preserved. Public unhydrated retrieval and the older strict hydration entry points
are unchanged. This does not enable ordinary `SearchClient` or CLI routing.

Each tier still retrieves at most `limit` candidates; a two-tier validation pass
can therefore inspect up to `2 * limit` distinct passages. A passage shared by
both tiers is checked once, against both retained source-row references. Actual
union size is charged before source reads under the existing hydration passage
budget. Hidden contributors also count toward source/output-byte checks. The
minimum visible-page admission is not a promise that all additional rank evidence
or refills will fit; limits are never increased to finish a request. No corpus-wide
scan, extra inference, or exhaustive full-corpus RRF guarantee is introduced here.

The regression target `semantic_current_rank_evidence` uses real CASS hash
embeddings and retained one-row FSVI shards to make the documented score-tie law
deterministic. It compares against an independently filtered unhydrated search;
its fixture includes a hidden contributor that changes the actual displayed order.
Execute this target and the existing hydration/fast-reuse suites together under
the shipping lockfile; source review alone is not runtime qualification.
