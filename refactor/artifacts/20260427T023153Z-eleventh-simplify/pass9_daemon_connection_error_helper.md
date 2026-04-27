# Pass 9 - Daemon Connection Error Helper

- Mission: Local Error Shape.
- Files changed: `src/daemon/client.rs`.
- Change: centralized the repeated `DaemonError::Unavailable("connection not established")` construction in `connection_not_established()`.
- Isomorphism proof: both former call sites now construct the same variant with the same message string, and the new unit test pins the rendered error text.
- Fresh-eyes check: re-read `get_connection_locked()` and `send_request()` after the edit; confirmed reconnect behavior, mutex handling, and stream lookup semantics are unchanged.
- Verification:
  - `rustfmt --edition 2024 --check src/daemon/client.rs`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_eleventh_simplify cargo test --lib daemon::client::tests::`

Verdict: PRODUCTIVE
