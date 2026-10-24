# Model ownership during daemon inference

Foreground embedding and reranking requests acquire one shared model owner
before reading its ID/dimension or executing any inference chunk. The owner is
acquired in the model slot's lookup/publication handoff. There is no separate
warm-then-lookup window in which an unload can revoke a successful acquisition.

Every chunk and the response metadata use that same owner. Unloading the public
slot, or loading a replacement into it, affects subsequent requests rather than
mixing models within an admitted request. The old model is not republished. It
remains alive until the last borrower releases it; unload does not promise to
immediately return all model memory while inference still owns it.

Attested embedding checks that acquired producer against the connection identity
before inference, after each chunk, and before signing the complete response.
The dispatcher calls the producer's identity-bearing single or batch method,
as named by the challenge, and validates every returned binding. A foreign identity, wrong result count, malformed vector,
producer drift, deadline, shutdown, or later-chunk failure cannot produce a signed
successful prefix. The signing key, challenge, wire schema and protocol revision
are unchanged. This does not add a separate reranker identity protocol.

The existing input limits, chunk sizes, one-foreground-request admission,
shutdown checks and request deadline remain in force. No new worker, lock held
across native inference, retry loop, automatic download, or hash fallback is
introduced. Health/status continue observing short-lived publication snapshots;
they do not need the retained request owner. Running native code remains a
cooperative cancellation boundary, not forcibly preemptible execution.

## Regression coverage and qualification

`daemon::models::slot::acquisition_tests` covers concurrent single-flight owner
acquisition, unload waiting on loading, replacement and final-owner release,
and loader error/panic cleanup. `daemon::core::inference::ownership_tests` calls
the production request dispatcher and chunk planner, replacing or evicting the
published model during the first chunk. It checks complete output, later-request
selection, attestation/HMAC verification, binding refusals, later-chunk errors,
shutdown and owner release. Reranking also exercises the existing document-ID
score-order restoration.

The dispatcher fixtures use real hash-control vectors and synthetic reranker
scores with test-only registered routing labels. They are not native MiniLM
accuracy, loading, performance, or real-socket integration tests. No production
model-injection API is exposed. These new tests have not been compiled or run in
the authoring environment; source and blob checks are not native qualification.

Run the shipping dependency graph and standard repository gates, including:

```sh
cargo test --locked --lib daemon::models::
cargo test --locked --lib daemon::core::inference::
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```
