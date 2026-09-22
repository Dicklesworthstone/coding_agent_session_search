# Text queries over a selected semantic generation

`SelectedSemanticGeneration::activate_text` joins supplied model owners to the
already admitted publication before exposing inference or vector retrieval.
It returns a `SelectedTextSemanticSearch`, whose result batches keep the exact
pointer, immutable manifest, canonical corpus identity, and serving witnesses.

This is a Rust library capability, not a change to the default CLI or MCP search
mode. It does not enable an incomplete publication, discover model files, or
derive canonical database identity from the manifest it is checking. Supply the
expected corpus identity from the caller's independently pinned archive snapshot.

## Quality-only retrieval

```rust,ignore
use coding_agent_search::search::fastembed_embedder::FastEmbedder;
use coding_agent_search::search::semantic_reader::publication::{
    SelectedSemanticGeneration, SemanticSelectionBudget,
};
use coding_agent_search::search::semantic_reader::text::TextQueryProducers;

// expected_corpus comes from the caller's pinned canonical archive snapshot.
let reader = SelectedSemanticGeneration::open_current(
    &data_dir, &expected_corpus, SemanticSelectionBudget::default(),
)?;
// Explicit local bundle only; this factory does not download models.
let quality = FastEmbedder::load_from_dir(&installed_model_directory)?;
let request = reader.activate_text(
    "How was request cancellation implemented?",
    TextQueryProducers::Quality(&quality),
)?;
let results = request.search(10, None)?;
let source = results.selection().corpus();
let hits = results.batch().hits();
```

The supplied producer must match the selected tier's full embedding space,
input contract, producer attestation, and dimension. An equal model name or
dimension is insufficient. Query-side in-memory storage may legitimately differ
from the selected artifact's persisted quantization. Missing or mismatched
identities fail closed; no expected identity is attached to an unbound response.

## Progressive execution

Use `TextQueryProducers::Progressive { fast: &fast, quality: &quality }`, then
`request.progressive(k, filter)`. Creating the iterator performs no inference.
The first `next()` infers and retrieves only the fast tier. The second infers
and independently retrieves quality candidates, then rank-fuses both lists.
Quality can introduce passages absent from the fast candidate set. Dropping
the iterator after Initial avoids all quality inference and vector search.

Both requested producer identities are inspected during activation, before the
first inference. An identity-aware lazy producer may load its model during that
inspection: deferred **inference** is not a promise of deferred model loading.
Each phase rechecks the producer owners, and every returned embedding must carry
the exact pre-admitted producer identity. A quality failure terminates iteration
with an error; the earlier fast batch stays Initial, never a successful refinement.

`search_with_ann` and `progressive_with_ann` use only graphs explicitly admitted
through `SelectedSemanticGeneration::with_ann`. The ordinary search methods stay
exact. Inspect `batch().execution()` for per-shard native/exact fallback behavior.

## Ownership and limits

Queries borrow the selected reader and producer owners. Publishing a successor
cannot alter an in-flight request. Returned batches retain the old selected
identity and vector owners after the reader is dropped or explicitly refreshed.
The adapter does not reopen artifact paths during inference or retrieval.

Text must be nonempty and at most 4,096 UTF-8 bytes. It is passed unchanged to
the producer. A zero-result request or witnessed empty tier still validates
identity, but requires no inference. These synchronous calls cannot interrupt a
running model or vector scan; hosts remain responsible for deadlines, admission,
and process memory supervision. Retaining complete vector images is not a total
RSS bound, and the producer may apply its documented token truncation policy.

The lower-level `SemanticGenerationReader` exposes the same text API for callers
that separately own artifact-selection authority. Its explicit hash controls
remain `HashControl`, not learned semantic readiness. Prefer the selected wrapper
when consuming published CASS generations so selection provenance cannot be lost.

## Validation scope

Default text-engine tests use real deterministic hash inference and real FSVI
files. Publication adapter tests reuse explicitly declared axis-vector fixtures
only for selection and lifetime mechanics; neither suite measures learned
relevance. The opt-in `selected_text_native_minilm_uses_actual_producer_vectors_and_published_owners`
test requires `CASS_TEST_EMBEDDER_MODEL` and runs actual local native MiniLM
inference, publishes those vectors, and retrieves through retained owners. It
does not download a model or constitute a corpus-scale relevance benchmark.
