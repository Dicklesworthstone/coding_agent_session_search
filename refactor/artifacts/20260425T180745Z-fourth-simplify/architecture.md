# Architecture Notes for Fourth Loop

- `src/lib.rs` owns CLI dispatch, robot surfaces, and error envelopes.
- `src/indexer/` and `src/storage/` are high-blast-radius and currently dirty, so this loop avoids them.
- `src/analytics/` has isolated query/types tests and several private formatting/projection helpers.
- `src/pages/`, `src/html_export/`, and `src/sources/` contain bounded pure helpers and test modules suitable for low-risk isomorphic simplification.
- `tests/` contains broad integration coverage, but this loop prefers focused module or integration-test gates per pass.
