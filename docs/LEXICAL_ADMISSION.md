# Lexical admission qualification (GH#501)

## Status

The production fast-open path is **not implemented by this change**. Strict
reader/writer admission is unchanged. There is no new verification-policy flag
or persistent open-proof format. The regression suite and measurement tools
below establish a baseline; they do not close #501 or establish a speedup.

The relevant CASS boundaries are `src/search/quill_bridge.rs`:
`open_cass_reader` and `QuillCassIndex::open_or_create`. Full-file witness
verification occurs inside Quill admission, before CASS receives a reader.
A CASS-side metadata cache alone cannot remove that internal verification.
The integration must use a supported published dependency API, not bypass
`build.rs` or the FrankenSuite source-provenance rules in `AGENTS.md`.

## Design assessment

| Option | What it establishes | Decision |
| --- | --- | --- |
| Parallel full verification | Existing whole-segment witness is checked on every fresh admission; parallelism changes latency, not bytes hashed | Preserve as strict fallback; not the remaining CPU optimization |
| Retain an admitted reader | Admission cost amortized within one live process and its pinned snapshot | Useful, but does not solve independent fresh processes |
| Path + length + mtime receipt | Only weak metadata equality | Reject: same-length rewrites with restored mtime evade it |
| Descriptor identity + length + mtime + ctime receipt | Detects ordinary replacement, truncation and writes on qualified filesystems; prior successful full verification can be reused | Potential opt-in optimization, not equivalent to present full verification |
| Deferred checks of accessed sections | Verifies sections before use, provided the dependency actually enforces this on every path | Does not establish integrity of unvisited bytes; cannot silently replace the current contract |
| Kernel-authenticated immutable segments | Potentially supplies a stronger reusable admission basis, if the kernel digest is bound to the exact manifest and descriptor | Preferred direction when preserving strong integrity; filesystem/platform support and publication changes must be qualified |

A checksummed receipt is not an authenticity boundary. An actor who can rewrite
both the proof and the data can recompute an unkeyed checksum. Likewise, identity
metadata does not necessarily expose latent storage corruption, inode reuse,
or every mmap modification pattern. A quiet period reduces timestamp-resolution
races but is not a proof of immutability. These distinctions must be explicit
before enabling identity receipts by default.

## Required integration contract

The dependency should own the admission policy and retained descriptor guards.
Maintenance, writer-open, repair, merge, import and integrity inspection must
remain full-verify by default; a search-only admission policy must not leak into
those call chains. An explicit full-verify override must ignore any cached
receipt and be covered through the real CLI dispatch.

An identity-receipt experiment would need all of the following, not just a stat
cache: manifest-generation and witness binding; proof format/version limits;
opened-descriptor rather than path-only identity; before/after checks around
successful full verification; stable publication; bounded atomic private proof
storage; rejection of symlinks and unsafe permissions; no proof issuance after
an error; cache misses on unsupported filesystems or malformed/corrupt/stale
proofs; a defined race boundary; and continued section validation before use.
Its weaker integrity policy must be named and documented as such.

Adversarial qualification must exercise both policies using fresh subprocesses:
replacement, truncation, same-length mutation, restored mtime, root/manifest
replacement, concurrent mutation/publication, symlink substitution, malformed
proofs, oversized proofs, stale or wrong-generation proofs, and writable or
forged proof state. Each invalid proof must cause strict re-verification or a
closed failure, never unverified success. The full-verify override must still
reject changed bytes even when metadata and proof state appear reusable.

## Executable baseline

`tests/lexical_admission_contract.rs` creates a real disposable index and checks
both the strict reader and maintenance writer after replacement, truncation,
same-length rewrite, and restored-mtime rewrite. These are strict-path tests,
not tests of a nonexistent receipt cache. Corrupted-cache-proof regressions
remain part of the unimplemented fast-open work.

```sh
cargo test --locked --release --test lexical_admission_contract -- --nocapture
cargo build --locked --release --bin cass --example lexical_admission_fixture
target/release/examples/lexical_admission_fixture /tmp/cass-admission-fixture 4096
python3 scripts/benchmark_lexical_admission.py \
  --cass target/release/cass --index /tmp/cass-admission-fixture \
  --rounds 7 --output /tmp/cass-admission-baseline.json
```

Use a new, nonexistent fixture destination. On a real archive, use a quiescent
published generation and representative matching queries. The harness does not
mutate or repair the index; it checks manifest stability, but that alone is not
an archive immutability proof. `all_segment_files_bytes` includes every matching
segment file in the directory, not necessarily only live manifest entries.

One binary produces `mode: baseline_only`. To compare a separately implemented
candidate, add `--candidate-cass /path/to/other/cass`. Identical binary hashes are
refused. Different hashes do not prove different verification policies; record
the source revisions, build flags and actual admission policy with the report.
No unsupported environment variable is interpreted as a fast/full toggle.

Fresh observations use a new `cass serve` process with exactly one measured
index-only lexical query. They are **not** end-to-end ordinary `cass search`
CLI timings. Process wall and CPU include startup, the initial status exchange,
search and clean EOF teardown; request wall and CPU are separate. The retained
workload reports first admission, repeated queries, unload, re-admission, reload
and post-reload reuse separately for each binary. Full hit/score/order equality
and expected `reader_reused` values are required. No timing threshold determines
correctness. A failed request, bad exit, deadline or changed manifest invalidates
the run rather than producing apparently successful evidence.

`rchar` is read-system-call traffic, **not bytes hashed**; mmap verification is
excluded. `read_bytes` reports storage I/O, not verification CPU. Per-request CPU
is quantized to kernel clock ticks, and maximum RSS is a process-lifetime high
water mark. Warm-cache priming, alternating comparison order, binary SHA-256s,
manifest SHA-256 and raw observations are recorded. A 4096-document CI fixture
checks operability, not multi-gigabyte performance. Repeat on representative
large indexes before making a performance claim.

```sh
python3 -W error::ResourceWarning -m unittest discover -s scripts \
  -p test_benchmark_lexical_admission.py -v
```

Those ten tests use fake protocol workers to validate the Python harness and
its failure handling. They are not native Quill correctness or performance
results. Native qualification runs in `.github/workflows/lexical-admission.yml`;
its status and artifacts must be checked separately before claiming success.
