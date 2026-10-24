# Bounded encrypted archive prefetch

The Pages crypto worker overlaps up to four encrypted chunk downloads with its
single, ordered AES-GCM/DEFLATE consumer. This is the existing
`DECRYPT_DATABASE` path, not an opt-in alternate viewer or archive format.

A completed response keeps its slot until the consumer finishes authenticating,
decompressing and copying that chunk. A slow first download or native operation
therefore cannot accumulate a whole archive of later responses. Refilling occurs
only after the consumed ciphertext is scrubbed. No plaintext is posted until all
chunks authenticate and both aggregate sizes match.

## Memory admission and cancellation

The window reserves two ciphertext copies per slot, accounting for the existing
stream collector's final concatenation. Its admission target is 32 MiB and four
slots; the per-transfer bound is the smaller of the format's ciphertext bound
and the archive's declared total ciphertext size. Larger bounds reduce the
window. A single allowed chunk that exceeds this target still works serially.
This is a live-ciphertext reservation policy, not a browser/process RSS limit:
the full output database, DEFLATE/WebCrypto scratch, response/network buffering,
object overhead, allocator retention and the subsequent SQLite-WASM load are
outside it. No archive-size or expansion limit has been increased.

A failed future download immediately aborts its siblings, even when an earlier
response has stalled. It also scrubs completed ciphertext and accumulated
plaintext while non-cancellable native work drains. The first failure/cancellation
reason remains authoritative if a late native operation subsequently fails.
The existing single-active/latest-pending job admission still owns that work;
queued requests cannot allocate another database early. Every prefetched task
has an observed outcome and is drained before the job releases its slot.

The original 120-second per-transfer deadline, generation cancellation, bounded
response reader, and per-chunk decompression limit remain in place. No retry,
background job, new provider, request-controlled URL, key/KDF change, or weaker
AAD/authentication rule is introduced. Successful decryption still transfers
one complete buffer using the existing worker protocol.

## Regression commands

```sh
node --test tests/pages/crypto-worker-admission.test.mjs tests/pages_crypto_prefetch.mjs
node --check src/pages_assets/crypto_worker.js
```

The new tests run the production worker with real WebCrypto AES-GCM/HKDF and raw
DEFLATE. They cover bounded overlapping transfers, ordered output, ciphertext
credits held through native work, size-based serialization, sibling failure,
tampering/swapped chunks, immediate scrubbing, supersession, deadlines and total
size validation. The existing suite also runs a real Node worker with HTTP Fetch,
abort signals and transferable output. Its four-chunk cancellation fixtures now
assert all four admitted transfers are cleaned up rather than assuming one.

For a same-invocation comparison against an actual prior worker file:

```sh
CASS_CRYPTO_INCUMBENT=/path/to/prior/crypto_worker.js \
  node --test tests/pages_crypto_prefetch.mjs
```

The comparison holds response bodies, observes one outstanding download in the
serial incumbent versus four in this worker, then checks identical plaintext
and native decryption-call counts. It does not report a wall-clock speedup. Selecting
the original file with `CASS_CRYPTO_WORKER_SOURCE` makes the new overlap
regression fail at its `1 !== 4` assertion. Browser/SQLite-WASM end-to-end,
Argon2/fflate fallback, real-host throughput and Rust builds require separate
qualification; Node tests do not establish them.
