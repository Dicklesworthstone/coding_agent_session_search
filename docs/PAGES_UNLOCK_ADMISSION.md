# Bounded unlock admission and retryable runtime downloads

The encrypted Pages worker now gives password and recovery unlocks one shared
active slot and retains only the latest waiting request. A superseding request
rejects the prior waiter and clears its credential bytes immediately. It does
not queue another KDF callback behind shared runtime initialization. The active
operation retains admission until its derivation and unwrap settle; cancelling
it never allows a second native operation to start early.

This changes the existing UNLOCK_PASSWORD/UNLOCK_RECOVERY worker handlers, not
an alternate viewer route. CLEAR_KEYS and DECRYPT_DATABASE also invalidate
waiting/active unlocks. Generation checks prevent an obsolete operation from
publishing a DEK, and late unwrapped keys are cleared before failure. Password
and recovery requests retain their own archive/slot context. Slot fallback
still uses the complete original credential, including significant whitespace
and Unicode; parameters, HKDF context, AES-GCM AAD and archive schema are unchanged.

## Runtime acquisition and recovery

The Argon2 WASM binary download uses the existing streaming reader's 1 MiB
limit and 120-second header/body deadline. Cancellation aborts its fetch, disposes
late responses and clears collected buffers without awaiting a stalled cancel
promise. A recovery unlock can therefore supersede a password waiting for a
missing runtime download rather than waiting indefinitely behind it.

The vendored high-level Argon2 API caches its module-loading promise, including
rejections. After a failed/cancelled binary download, the worker retires that API
only after the failed hash call settles. A later admitted password reloads the
API and retries acquisition. Successful runtimes remain reusable; successful keys
are not cached across unlock attempts. The vendored code and binary are unchanged.

The download deadline is not a KDF execution deadline. Once WebCrypto, WASM
initialization or hashing is running, it remains a cooperative boundary. A hung
native/library promise still requires terminating/recreating the worker; no
claim of forcible interruption or hard browser RSS limits is made.

## Credential lifetime and compatibility

Job-owned credentials are mutable bytes, bounded to 64 KiB per decoded secret.
Password limits apply to UTF-8 bytes, not characters. Recovery accepts the
existing URL-safe base64, literal-text fallback and Uint8Array forms, retaining
the 24-byte minimum. Inputs exceeding the new bound are refused before KDF work.
This is an intentional browser limit; archives requiring a larger credential
cannot be unlocked through this viewer.

Superseded waiting credentials and job metadata are released immediately. Active
source credentials are cleared on invalidation; KDF argument copies and temporary
keys are cleared when their awaited operation returns. JavaScript strings, the
caller's message copies, native heaps, CryptoKey storage and garbage-collector
retention cannot be comprehensively zeroized by this code. At most two admitted
unlock jobs are retained, not an asserted whole-process memory ceiling.

## Executed regression boundary

Run the production worker's tests together:

```sh
node --test tests/pages/crypto-worker-admission.test.mjs \
  tests/pages_crypto_prefetch.mjs tests/pages/crypto-worker-unlock.test.mjs
node --check src/pages_assets/crypto_worker.js
```

The unlock tests execute real WebCrypto AES-GCM and HKDF. Argon2 execution and
its lazy/rejected module promise are controlled test doubles so stalls and
interleavings can be forced; these tests do not execute the Argon2-WASM algorithm.
The unchanged admission/prefetch suites also cover actual raw DEFLATE, HTTP
Fetch, worker threads and transferable database buffers. They do not establish
browser/SQLite-WASM end-to-end behavior or production throughput.

The burst regression against the original worker admits 81 KDF callbacks while
the first is held. The updated worker admits one, rejects the 79 superseded
waiters, and then executes only the latest request. This counts admitted work,
not simultaneous native CPU kernels or measured memory savings. A separate
before/after test exercises a failed runtime request followed by a successful
retry. Runtime publication is checked by exact source/blob identity, not by
claiming that source review substitutes for executable tests.
