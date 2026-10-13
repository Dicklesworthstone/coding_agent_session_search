# Source-bound hydration for retained semantic batches

`SemanticSearchBatch` and `SelectedSemanticBatch` now expose
`hydrate_in_transaction` and `hydrate_from_connection`. These are explicit
library entry points; they do not replace ordinary `SearchClient` hydration or
enable the progressive CLI route.

A semantic vector can outlive the source row that originally produced it. A
lookup by message ID alone must not attach that vector's score to rewritten
text. This adapter verifies each selected passage's SHA-256 and its encoded
message, agent, workspace, source, role, and timestamp fields against source
rows read through one FrankenSQLite transaction. It also checks every result's
retained vector owner and physical row before reading source bodies.

The returned text is the **verified canonical passage**, not an unchecked full
message. Verification deliberately does not certify every raw byte of the full
message, a physical pathname, the CRC32 source field as a cryptographic source
identity, or the identity/freshness of the entire archive. The original ranking,
coverage, execution reports, vector/graph owners, and (for selected batches)
publication identity remain available through `source()`.

## Usage

```rust
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::search::semantic_reader::publication::SelectedSemanticBatch;
use coding_agent_search::search::semantic_reader::publication::text::hydration::{
    HydratedSemanticBatch, HydrationBudget, HydrationError, PassageProjection,
};

fn hydrate(
    batch: &SelectedSemanticBatch,
    connection: &mut Connection,
    selected_projection: PassageProjection,
) -> Result<HydratedSemanticBatch<SelectedSemanticBatch>, HydrationError> {
    batch.hydrate_from_connection(
        connection,
        selected_projection,
        HydrationBudget::default(),
    )
}
```

Use the already-open canonical archive connection. The adapter opens no files
and performs no migrations, model acquisition, index repair, or row writes.
`hydrate_from_connection` owns and rolls back its read transaction on both
success and failure. It does not adopt an existing caller transaction. With an
already-pinned snapshot, pass its `franken_sync::compat::Transaction` to
`hydrate_in_transaction`; that entry point leaves transaction ownership with
the caller.

The projection is mandatory. `CanonicalPrefixV1` accepts only the historical
chunk-zero canonical prefix. `RawPassagesV2` replays the current raw-first
bounded passage selection, then canonicalizes the selected passage. Select the
profile that built the retained artifact; there is no automatic fallback or
upgrade. A digest mismatch is an error in either mode. Short inputs can have
identical verified bytes under both profiles.

## Failure and resource behavior

No partial hydrated list is returned if a later source is missing, changed,
oversized, malformed, or otherwise unreadable. Failures do not modify the
original batch; callers can retain it for a deliberate retry. Errors and the
hydrated types' `Debug` implementations omit message bodies and source paths.

Default limits are 256 passages, 4 MiB per full source message, 16 MiB aggregate
source text/metadata, 2 MiB verified output text/locator strings, and five
seconds. Hard admission ceilings are 1,024 passages, 8 MiB per message, 32 MiB
for each aggregate byte budget, and 60 seconds. Per-message source metadata has
a 16 KiB ceiling. Chunks from the same message share one full-body read and one
locator; full source bodies are released after that message is verified.

A byte-length probe precedes body transfer, in the same transaction. Deadlines
are checked around SQL and canonicalization, and connection-owned hydration
counts transaction admission and rollback in its deadline. **These are not hard
limits on a synchronous engine call, its scratch allocations, or process RSS.**
Keep the existing command/worker supervision for end-to-end containment.

## Qualification

The added tests cover pure fingerprint/metadata checks plus real FrankenSQLite
and retained FSVI/hash-control fixtures. No learned model files are required.
The focused full-crate test command is:

```bash
RUST_MIN_STACK=134217728 cargo test --locked --lib \
  search::semantic_reader::publication::text::hydration:: -- --nocapture
```

Run full compilation, formatting, Clippy, and repository gates separately.
Availability of this API is not a release-qualification receipt.

## Composed text queries and progressive delivery

An activated `TextSemanticSearch` or `SelectedTextSemanticSearch` also exposes
`search_hydrated_in_transaction` and
`search_hydrated_with_ann_in_transaction`. These compose inference, retrieval
and source verification rather than returning unchecked text between them.
The returned batch retains the same ranking and owners as the unhydrated
search; the selected variant retains its publication identity too.

`progressive_hydrated_in_transaction` and
`progressive_hydrated_with_ann_in_transaction` return lazy fused iterators.
The first step runs fast inference/retrieval and verifies the Initial passages.
Only a subsequent step runs independent quality inference/retrieval and
verifies the Refined passages. Dropping after Initial does not start quality
work. A retrieval or source-verification error terminates the iterator; an
already-returned Initial batch is not relabelled as a completed refinement.
A caught panic cannot restart the abandoned continuation either.

All composed entry points borrow an existing `franken_sync::compat::Transaction`.
Keep that transaction's snapshot and the filter stable throughout the operation.
The iterator does not begin, commit or roll back it. Drop the iterator before
finalizing the transaction. The results retain verified text and vector owners,
not a borrow of the transaction.

```rust
let request = selected.activate_text(query_text, producers)?;
let mut phases = request.progressive_hydrated_in_transaction(
    10,
    None,
    &transaction,
    PassageProjection::RawPassagesV2,
    HydrationBudget::default(),
)?;
// Each successful item contains verified canonical passages and its original
// phase/publication evidence. On error, later next() calls return None.
for phase in phases.by_ref() {
    let verified = phase?;
    consume(verified);
}
drop(phases);
// The caller still owns transaction finalization, including error paths.
```

Composed-query deadlines start at the query method or iterator construction,
not after retrieval. They include time between iterator polls. Earlier reader
admission and producer activation are outside this deadline. An in-progress
native call is still not forcibly interrupted; a late result is withheld.

Progressive admission reserves `2 * k` passage slots before native work, and
source/output byte counters are shared across both phases. Repeated reads in
Refined are charged again, not given a fresh allowance. Full source messages
are not cached across phases. After Refined, the iterator is terminal even if
the caller waits past the deadline before polling it again. These limits do
not cover model allocations, all vector owners, or process RSS.

These are retained-reader library APIs. They do not switch ordinary
`SearchClient` hydration or the normal/progressive CLI onto this path.


## Current-source refill during replacement

`SemanticGenerationReader::search_current_text_in_transaction` and
`SelectedSemanticGeneration::search_current_text_in_transaction` accept a
`query::current::CurrentTextQuery`. Unlike strict whole-batch hydration, this
opt-in operation excludes a missing source or a passage whose content/provenance
has changed, then repeats top-k retrieval over the same retained owners. The
original filter is preserved and exclusions use complete passage IDs, not message
IDs: a stale chunk cannot discard a different, still-valid chunk of that message.

Each requested producer's actual identity-bearing query response is reused only
within this request. Attested response overrides and current-producer identity
checks are preserved; no raw vector receives invented identity metadata.
Missing tiers, foreign producers, corrupt owners/source rows, database errors and
resource limits remain failures. A refill limit never produces a false-empty or
partially verified page. A genuinely empty page requires a subsequent filtered
retrieval returning no candidates.

```rust
use coding_agent_search::search::semantic_reader::publication::text::hydration::query::current::CurrentTextQuery;

let mut query = CurrentTextQuery::new(
    query_text, producers, 10, PassageProjection::RawPassagesV2,
);
query.filter = Some(&snapshot_filter);
let current = selected.search_current_text_in_transaction(query, &transaction)?;
consume(current.verified());
// current.stats() reports searches, exclusions and cumulative byte work.
```

Keep the caller-owned transaction and filter stable throughout the call. Select
the projection appropriate to the admitted document contract, not the replacement
writer's projection. This API verifies selected passage content and encoded
metadata, not arbitrary text outside the selected passage or whole-archive
coverage. It does not loosen publication admission, maintenance locks, or the
normal CLI's `OwnerBackedReaderRequired` refusal.

The deadline starts before producer activation. Native calls are cooperative,
not preemptible. All passes share one hydration passage/source/output account;
repeated reads and provisional verified text discarded during refill are charged
again. `RefillBudget` additionally bounds retrieval passes, excluded passages and
excluded-ID bytes. Memory held by model/vector owners is outside these limits.
The result keeps the final filtered ranking, execution receipts and exact
publication identity; exclusion counts do not certify current corpus coverage.
