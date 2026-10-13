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
runs quality inference/retrieval. It recomputes fast retrieval under the current
exclusion set before fusing independent tier rankings. A newly discovered stale
quality-side passage triggers refusion with the expanded exclusion set; old fast
ranks are not reused after exclusions change.

Each selected producer's actual identity-bearing query response is cached only
within this request, including its overrides and producer checks. Refill and
refinement therefore do not rerun successful query inference. Cached embeddings
and exclusions are released on completion, error, unwinding, or iterator drop.
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

The read-only hydration workflow also checks this command and full-library
Clippy. A source commit or an unexecuted workflow is not a passing test receipt.
