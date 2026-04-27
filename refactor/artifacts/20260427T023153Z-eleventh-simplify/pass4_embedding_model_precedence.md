# Pass 4 - Embedding Model Precedence

- Mission: Option Flow.
- Files changed: `src/daemon/worker.rs`.
- Change: moved the daemon embedding worker's model fallback chains into `EmbeddingJobConfig::{fast_pass_model, quality_pass_model, single_pass_model}`.
- Isomorphism proof: two-tier mode still defaults fast to `hash` and quality to `minilm`; single-pass mode still prefers `quality_model`, then `fast_model`, then `hash`.
- Fresh-eyes check: re-read each helper against the removed inline chains and confirmed clone timing and `unwrap_or_else` defaults are unchanged. The semantic flag still compares the resolved model with `HASH_EMBEDDER_MODEL`.
- Verification:
  - `rustfmt --edition 2024 --check src/daemon/worker.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib daemon::worker::tests::test_build_passes`

Verdict: PRODUCTIVE
