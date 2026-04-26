# Pass 8 - Worker Config Fixture

- Mission: Test Fixture Builder.
- Files changed: `src/daemon/worker.rs`.
- Simplification: added `build_pass_config(...)` for the repeated empty-path `EmbeddingJobConfig` setup used by build-pass tests.
- Isomorphism proof: the helper preserves the same `db_path`, `index_path`, `two_tier`, `fast_model`, and `quality_model` values previously written inline in each build-pass test.
- Fresh-eyes review: left `test_job_config` inline because it intentionally exercises non-empty paths and direct struct fields; only the repeated build-pass fixture shape moved.
