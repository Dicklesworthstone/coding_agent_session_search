# CASS frankensearch CJK fast-reject perf slice - 2026-05-02

## Workload

Copied the live CASS canonical database into a fresh data directory with no
copied lexical index, then ran:

```bash
cass index --watch-once /tmp/cass-cjk-fast-reject-nonexistent \
  --data-dir "$DATA" \
  --json \
  --progress-interval-ms 10000
```

The command enters the authoritative canonical DB lexical repair path and then
skips the broad follow-up scan because explicit `--watch-once` paths were
provided.

## Baseline

Binary: `/tmp/cass_perf_opt_target/profiling/cass` at CASS `8c705ac2`, with
frankensearch pinned to `3dbab624`.

Artifact:
`tests/artifacts/perf/cass-tantivy-writer-threads-20260502T024321Z/after-skip-broad-scan-2.stderr.txt`

- Exit: `0`
- CASS elapsed: `103579 ms`
- Wall clock: `1:44.79`
- Full corpus reached: `current=51214` at `44636 ms`
- Max RSS: `60683480 KB`

## Change

Upstream frankensearch commit `a982f33a` changes
`frankensearch-lexical/src/cass_compat.rs` so
`CjkBigramDecomposeStream::decompose_cjk` rejects ordinary non-CJK tokens before
allocating `Vec<char>`.

CASS now pins frankensearch to `a982f33a`.

## After

Measured using a command-line local frankensearch patch before the CASS rev bump;
the touched `cass_compat.rs` file was identical between frankensearch `3dbab624`
and local `HEAD` except for this patch.

Artifact:
`tests/artifacts/perf/cass-tokenizer-cjk-fast-reject-20260502T0333Z/localfs-after.stderr.txt`

- Exit: `0`
- CASS elapsed: `95165 ms`
- Wall clock: `1:36.07`
- Full corpus reached: `current=51214` at `34824 ms`
- Max RSS: `60238784 KB`

## Result

- Total wall time improved from `104.79 s` to `96.07 s`: `1.09x`.
- Ingestion-to-full-corpus improved from `44.636 s` to `34.824 s`: `1.28x`.
- RSS moved from `60,683,480 KB` to `60,238,784 KB`.

## Verification

frankensearch:

- `cargo fmt --check`
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p frankensearch-lexical cjk_bigram -- --nocapture`
- `cargo test -p frankensearch-lexical cass_normalize_and_limit_matches_legacy_pipeline -- --nocapture`
- `cargo test -p frankensearch-lexical cass_tokenizer_matches_legacy_regex_boundaries -- --nocapture`

CASS verification is recorded in the closeout for the dependency bump commit.
