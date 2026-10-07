# Explicit external embeddings (GH #481 / gl29d)

CASS remains local-only by default. A URL or API key never selects an external
provider. External indexing/search requires both explicit provider selection
and `CASS_EXTERNAL_EMBEDDINGS=1`. Session text and search queries are then sent
to the configured service, including when it runs on another machine. Do not
enable this for an endpoint that is not permitted to receive your transcripts.
This adds no in-process CUDA/ONNX stack.

## Configuration

Use an OpenAI-compatible endpoint that returns indexed float embeddings, the
requested model name, and finite unit-normalized vectors. Configure the full
`/v1/embeddings` URL, not a base URL. HTTPS is required except for loopback HTTP;
redirects and implicit environment proxies are refused. URL credentials, query
parameters and fragments are rejected. Credentials are sent only in the Bearer
header and are not included in embedding identities or diagnostics.

```sh
export CASS_EXTERNAL_EMBEDDINGS=1
export CASS_EXTERNAL_EMBEDDING_URL=http://127.0.0.1:8080/v1/embeddings
export CASS_EXTERNAL_EMBEDDING_MODEL=my-served-model
export CASS_EXTERNAL_EMBEDDING_DIMENSION=384
export CASS_EXTERNAL_EMBEDDING_REVISION=my-frozen-model-and-preprocessing-revision
# Optional, for an authenticated endpoint:
# export CASS_EXTERNAL_EMBEDDING_API_KEY=...

cass models backfill --tier quality --embedder external \
  --batch-conversations 16 --max-batches 200 --json
cass search 'transaction checkpoint' --mode semantic --model external --json
```

For a custom archive, pass the same `--data-dir` and `--db` to backfill/search.
The endpoint is a **quality** producer; `--tier fast --embedder external` is
rejected. The fast hash tier and installed native MiniLM vectors stay separate.
An external service running MiniLM is still not the local MiniLM vector space.

Before indexing, two rounds of fixed public strings must produce the declared
dimension, finite unit-normalized vectors (norm tolerance 0.001), and repeatable
coordinates (absolute tolerance 0.00001). Every subsequent response is checked
for model, row count, unique input indexes, dimension, finite values and norm.
Preflight checks the output contract, not semantic quality or an authenticated
model-weight digest. Freeze the server's model/preprocessing configuration and
change `CASS_EXTERNAL_EMBEDDING_REVISION` whenever that configuration changes.

The filesystem-safe identity hashes endpoint, model and revision and includes
the dimension. A change in any of those starts a different vector namespace,
not a continuation of old vectors. API-key rotation does not change the space.
The persisted header revision records CASS's external transport/input contract;
the quality manifest's model revision records the retained provider's identity.

## Bounded work, interruption, and resume

HTTP requests are serial per provider. A concurrent caller waiting for the
provider checks cancellation while queued and uses the configured timeout as a
separate admission deadline. `external_queue_timeout` means none of that call's
text was sent; it does not cancel or detach the active caller's request. A caller
that gains admission still has the full configured deadline for each HTTP
request. Cancellation remains cooperative, not a hard real-time guarantee.

`CASS_EXTERNAL_EMBEDDING_BATCH_SIZE`
defaults to 64 rows and is limited to 1–128. The serialized request byte budget
`CASS_EXTERNAL_EMBEDDING_MAX_REQUEST_BYTES` defaults to 262144 and is limited to
1024–4194304; JSON escaping counts against it. Responses are limited to 64 MiB.
`CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS` defaults to 30000 and is limited to 1–120000.

For unattended runs, `CASS_EXTERNAL_EMBEDDING_MAX_RETRIES=2` opts into retrying
HTTP 429, 500, 502, 503 and 504. The default is **0**; the maximum is 5 retries
after the initial attempt. Only the failed HTTP sub-batch is resent, with the
same endpoint, model, authorization and inputs. A retry can cause additional
server execution or billing. Tuning retries does not change vector identity.

All attempts, backoff and response reading for one HTTP sub-batch share its
original timeout; retries do not multiply the time budget. Backoff starts at
100 ms and doubles. A valid `Retry-After` (seconds or IMF-fixdate) is a minimum
wait, never shortened to fit the budget. Invalid, ambiguous or unsupported
headers, or waits that cannot fit, fail explicitly without another attempt.
Cancellation is polled during backoff. Transport errors, timeouts, redirects,
authentication errors and malformed vectors are not retried. Exhaustion
rejects the complete embedding call, preserving the last durable checkpoint
and preventing later sub-batches from being sent. Fixed-probe preflight uses
the same bounded policy and must still pass all output-contract checks.

These HTTP limits do not replace canonical backfill limits. Use
`--batch-conversations`, `--max-batches`, `CASS_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT`
and `CASS_SEMANTIC_MAX_BYTES_PER_CHECKPOINT` to bound admitted work. The existing
whole-conversation exception still applies to a single oversized conversation;
these are not a hard process-RSS ceiling. One provider/preflight is retained
across successful batches within a command.

SIGINT/SIGTERM are polled before and after HTTP requests. An in-flight blocking
request is not detached: cancellation latency is bounded by that request's
configured deadline. A cancelled or failed HTTP batch never advances its
durable checkpoint, even if an earlier HTTP sub-batch succeeded. Previously
checkpointed vectors remain available for canonical reconciliation on restart.
Rerun the same command/configuration to resume. The CLI reports cancellation as
exit 130/143 and preserves structured batch accounting. It does not persist
credentials or transcript bodies in the checkpoint.

Network, HTTP, malformed-response and preflight errors are reported without
response bodies, URLs, or credentials. Failures do not switch to MiniLM or
hash. A corrupt external checkpoint manifest is refused, not reset to an empty
ledger. Partial generations remain unavailable to external search.

## Nightly operation

To explicitly select external quality backfill and search through policy:

```sh
export CASS_SEMANTIC_EMBEDDER=external
# The consent and endpoint settings above must also be present in this process.
cass models backfill --tier quality --scheduled --max-batches 200 --json
```

`cass schedule run nightly` inherits these settings for its quality worker.
It no longer requires local MiniLM files when external is explicitly selected.
Planning does not probe the endpoint; the admitted quality worker checks
consent and preflight. Idle/load, foreground-work, maintenance-lock, and total
batch limits still apply. A paused/disabled worker sends no probes or text.
Installed OS timers need these variables in their own service environment;
exporting them in an unrelated interactive shell does not configure a timer.
CASS does not write API keys into generated unit files.

## Search scope

The supported external query surface is exact semantic search over the matching
completed monolithic artifact. Both explicit `--model external` and the opt-in
policy above are supported, including the bounded JSON/robot search worker.
External vectors do not enter the local inference daemon or local query cache
namespace. Missing/stale/mismatched artifacts and unfinished checkpoints are
refused before endpoint preflight. Availability probes perform no HTTP.
The newer immutable-generation and progressive/two-tier serving paths still
require their existing ownership/identity admission and remain fail-closed;
this feature does not bypass those gates. No GPU throughput or quality gain
is guaranteed; those depend on the chosen server and model.
