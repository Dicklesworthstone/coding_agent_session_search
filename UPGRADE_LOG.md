# Dependency Upgrade Log

**Date:** 2026-02-17  
**Project:** coding_agent_session_search (`cass`)  
**Language:** Rust

## Summary
- **Updated:** 3 direct dependency lines in `Cargo.toml` (`reqwest`, `rand`, `rand_chacha`)
- **Migrated code:** rand 0.10 API updates across runtime/test/bench callsites
- **Validated:** `cargo check --all-targets`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
- **Remaining behind latest:** 3 transitive crates (`generic-array`, `hnsw_rs`, `libc`)

## Direct Dependency Updates

### reqwest: 0.12.28 -> 0.13.2
- **Manifest change:** `features = ["json", "rustls-tls", "blocking", "multipart"]` -> `features = ["json", "rustls", "blocking", "multipart"]`
- **Reason:** reqwest 0.13 removed `rustls-tls` feature name
- **Status:** ✅ Compiles and passes strict clippy

### rand: 0.8.5 -> 0.10.0
- **Manifest change:** `rand = "0.8"` -> `rand = "0.10"`
- **Code migration:** replaced old APIs (`thread_rng`, `gen`, `gen_range`) with rand 0.10 APIs (`rng`, `random`, `random_range`) and updated RNG callsites used by export/encryption helpers
- **Status:** ✅ Compiles and passes strict clippy

### rand_chacha: 0.3.1 -> 0.10.0
- **Manifest change:** dev dependency `rand_chacha = "0.3"` -> `rand_chacha = "0.10"`
- **Code migration:** updated deterministic test RNG usage in `tests/util/mod.rs`
- **Status:** ✅ Compiles and passes strict clippy

## Cargo Resolution Notes
- `cargo update --verbose` now reports only these unresolved transitive updates:
  - `generic-array v0.14.7` (available `0.14.9`)
  - `hnsw_rs v0.3.2` (available `0.3.3`)
  - `libc v0.2.180` (available `0.2.182`)

## Validation Run
- `cargo check --all-targets` ✅
- `cargo fmt --check` ✅
- `cargo clippy --all-targets -- -D warnings` ✅

## Files Touched for rand/reqwest Migration
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/pages/encrypt.rs`
- `src/pages/key_management.rs`
- `src/pages/qr.rs`
- `src/pages/wizard.rs`
- `src/html_export/encryption.rs`
- `tests/util/mod.rs`
- `benches/crypto_perf.rs`
- `benches/export_perf.rs`

---

## 2026-02-18 Follow-up Update

### Summary
- Ran `cargo update --verbose` in `coding_agent_session_search`
- Updated lockfile to latest compatible crates available in this environment
- Re-validated code quality gates and targeted regression tests after updates

### Lockfile updates applied
- `aws-lc-rs`: `1.15.4 -> 1.16.0`
- `bumpalo`: `3.19.1 -> 3.20.1`
- `hnsw_rs`: `0.3.2 -> 0.3.3`
- `native-tls`: `0.2.16 -> 0.2.18`
- `toml`: `1.0.2+spec-1.1.0 -> 1.0.3+spec-1.1.0`
- resolver-selected transitive adjustment: `indexmap 2.13.0 -> 2.12.1`

### Remaining behind absolute latest (from cargo update output)
- `generic-array 0.14.7` (latest `0.14.9`)
- `libc 0.2.180` (latest `0.2.182`)

### Post-update validation
- `cargo fmt --check` ✅
- `cargo check --all-targets` ✅
- `cargo clippy --all-targets -- -D warnings` ✅
- Targeted regressions:
  - `cargo test --test connector_aider aider_detect_` ✅
  - `cargo test --test connector_codex codex_detect_` ✅
  - `cargo test --test connector_opencode opencode_computes_started_ended_at` ✅
  - `cargo test --test cross_workstream_integration inline_analytics_badges_match_detail_modal_metrics` ✅

### Full-suite note
- `cargo test` now advances deep into the suite and all newly touched regression areas pass.
- There is still an existing long-running/hanging case in `tests/e2e_error_recovery.rs` (`test_corrupted_index_triggers_rebuild`) that prevented a clean single-command completion in this session.

---

## 2026-02-19 Dependency Update

### Summary
- Ran `cargo update` in `coding_agent_session_search`
- **Updated:** 4 crates | **Unchanged behind latest:** 3 (transitive constraints)
- Build verification via code review (full `cargo check` blocked by pre-existing ftui-widgets errors in sibling repo)

### Lockfile updates applied

| Crate | Old | New | Type | Notes |
|-------|-----|-----|------|-------|
| bumpalo | 3.20.1 | 3.20.2 | Patch | Internal arena allocator (transitive). No API changes. |
| clap | 4.5.59 | 4.5.60 | Patch | Bug fixes only. Includes clap_builder 4.5.59→4.5.60. |
| fastembed | 5.9.0 | 5.11.0 | Minor | New `external_initializers` field on `UserDefinedEmbeddingModel` (v5.10). TLS backend selection (v5.9). Nomic v2 MoE support (v5.11). |
| security-framework | 3.6.0 | 3.7.0 | Minor | macOS-only. Includes security-framework-sys 2.16.0→2.17.0. |

### fastembed 5.9→5.11 compatibility verification
- v5.10 added `external_initializers` field to `UserDefinedEmbeddingModel` — breaks struct-literal construction
- Our code uses `UserDefinedEmbeddingModel::new()` constructor (not struct literals) in both `src/search/fastembed_embedder.rs` and `frankensearch-embed` — **not affected**
- `pooling` field remains `pub` with type `Option<Pooling>` — field assignment pattern unchanged

### Remaining behind absolute latest
| Crate | Current | Available | Reason |
|-------|---------|-----------|--------|
| generic-array | 0.14.7 | 0.14.9 | Transitive constraint |
| indexmap | 2.12.1 | 2.13.0 | Transitive constraint |
| libc | 0.2.180 | 0.2.182 | Transitive constraint |

### Build verification
- Full `cargo check` blocked by **pre-existing** compilation errors in `frankentui` sibling repo (`ftui-widgets`: 27 errors — missing lifetime specifiers, missing variables, unstable features). These errors exist independently of this update.
- Compatibility verified through code review of all 4 updated crates' changelogs and our usage patterns.

---

## 2026-04-22 /library-updater pass (exhaustive, swarm-coordinated)

### Summary
- **Updated git revs:** 2 repositories (5 Cargo.toml pins) — ftui family → `5f78cfa0`, frankensqlite family → `422969cf`
- **Verified:** `asupersync = "0.3.1"` (crates.io, user-specified target — already correct at line 17)
- **Wildcard crates.io deps:** 0 packages behind latest within current constraints (per `cargo update`)
- **Held back / not actionable from cass alone:** `lru 0.16→0.17`, `generic-array 0.14.7→0.14.9`, `rusqlite 0.38→0.39`
- **Coordinated with active swarm:** broadcast reservation on Cargo.toml/Cargo.lock for the ~15 minute upgrade window; resumed swarm afterward.
- **Verification:** `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_libupdate cargo check --all-targets` → `Finished dev profile in 6m 35s`, 2 pre-existing warnings, no errors.

### ftui (+ ftui-runtime, ftui-tty, ftui-extras): 2d25a03d → 5f78cfa0
Commits pulled in:
- `b3e5fc7a chore(deps): bump asupersync 0.2.9 → 0.3.0 (crates.io v0.3.0)`
- `5f78cfa0 chore(deps): bump asupersync 0.3.0 → 0.3.1 (crates.io)`

Breaking: none (internal dep bump only).

### frankensqlite (+ fsqlite-types): 83c0d882 → 422969cf
~30 commits pulled in, highlights:
- perf: `cache autocommit publication binding` (29b062c7), `reuse record header template for upsert` (eb5a74e9), `identity-skip memmove in defrag` (4bb33114)
- `a5813cfc chore(deps): bump asupersync 0.3.0 → 0.3.1 (crates.io)`
- `253959cd chore(deps): bump asupersync 0.2.9 → 0.3.0 (crates.io v0.3.0)`
- test hardening: conformance oracle 74b (e4826610), upsert record traps (92872a44)
- bugfix: `fix(pager): align 3 tests with current bump-allocator + first-committer-wins` (b93c7cbd)

Breaking: none (fsqlite::Connection, compat layer, params! macro unchanged).

### Remaining transitive asupersync 0.2.9
- Flows through `cass → FAD@88756ba9 → fsqlite@e3f57c9a → asupersync 0.2.9` (FAD's own fsqlite pin predates the asupersync 0.3.1 bump).
- **Cannot be collapsed from cass alone** — requires coordinated cross-repo bump in FAD.
- Filed follow-up bead: `coding_agent_session_search-0x5gm` — collapse after bead `3e3qg.14` (FAD rusqlite→frankensqlite migration) completes and FAD pushes a new HEAD.

### Attempted but reverted
- `lru 0.16 → 0.17`: blocked by `fsqlite-core` pinning `lru = "^0.16"`. Requires upstream fsqlite-core bump first. Cargo.toml reverted to `lru = "0.16"`.

### Files modified
- `Cargo.toml` (5 lines: 4 ftui revs + 1 frankensqlite rev + 1 fsqlite-types rev)
- `Cargo.lock` (ftui family, fsqlite family, asupersync 0.3.0→0.3.1 for ftui/fsqlite subgraphs, added `simdutf8 0.1.5`)
- `UPGRADE_LOG.md` (this entry)

