# Dependency Upgrade Log

## September 2026 release update (in progress)

Owner request: update stable dependencies individually, then publish a complete
release through DSR, GitHub, crates.io and Homebrew without GitHub Actions.
Registry inventory on September 12 UTC found 103 declarations covering 98
direct packages; 18 packages have newer stable releases. Crossbeam, Asupersync and Console have passed
focused behavioral validation; the full release gate remains pending.
FrankenSQLite 0.3.18 and agent detection 0.2.4 are current.

First candidate: crossbeam-channel 0.5.16 → 0.5.17. Its published changelog
documents a bounded-channel `SelectedOperation` leak memory-safety fix,
initialization overflow fixes, and timer corrections. CASS uses bounded
channels in indexing and storage. The isolated single-package candidate passed
remote formatting, all-target Clippy, 31 library tests, three interrupted-index
CLI tests and one fleet-setup transport-failure retry test, with no failures or
ignored tests. The first admission ended with exit 143 before tests; a fresh
continuation produced these results. Strict UBS timed out in its Rust module
after 300 seconds, so the aggregate gate exited 1. Source verification passed
after execution; this is a focused behavioral pass with an incomplete scanner,
not full release clearance. The retained receipt SHA-256 is
`6794594468ee8f3f45835a81417faa837c6f2c67b3c6ce494078adfc9af190d9`.
Source: [published crate](https://static.crates.io/crates/crossbeam-channel/crossbeam-channel-0.5.17.crate).

Asupersync 0.4.10 → 0.4.11: published source review found the
used runtime/Cx APIs compatible; current-thread tasks now run on the caller
thread. The resolver changed only the runtime and its four required companion
packages (macros, decision, evidence and kernel). Manifest and build-time pins
are aligned. Remote formatting and all-target Clippy passed. Three exact CLI
tests passed (real HTTP preview, interrupted indexing and proof-debt output).
The library stage first timed out during compilation with zero tests; its
unchanged-source follow-up separated compilation from the test budget and
passed all 22 selected tests, covering every requested filter. These include
runtime nesting, Quill publication/heartbeat, cancellation rollback, backup
collision conservation and redaction privacy. All 25 focused tests passed,
with zero failures or ignored tests. The follow-up receipt SHA-256 is
`a875f612f1a0a2b112da2df1e7745b2cde1fc998c9e21754d50c6219b6fb3013`.
Source: [upstream release](https://github.com/Dicklesworthstone/asupersync/releases/tag/v0.4.11).

Console 0.16.4 → 0.16.6. Live registry verification confirms
0.16.6 is the latest stable, non-yanked release. It corrects OSC/DCS handling,
Unicode truncation and visible tail widths; no CASS API migration is needed.
Only its package version changed; Cargo also reselected 13 existing Windows
dependency edges within their published broad version ranges. No other package
version changed, and locked Windows-target metadata validation passed. Actual
Windows compilation remains part of the release gate. Remote formatting and
all-target Clippy passed. The real pages export/preview/decrypt workflow
passed once with the candidate exporter and once with the published 0.8.0
exporter: one test case executed in two modes, zero failures or ignored tests.
Both check production KDF parameters, wrong-password rejection and all 40
messages. Source verification passed for all 3,752 inputs. Receipt SHA-256:
`32798948f7d22b46bc8887cb788e482d9b9226588bf9c70f05dfced589ddc314`.
Cargo retains an existing
future-compatibility warning from `nix 0.28.0`'s build-script `cfg_aliases!`
expansion (a trailing semicolon in expression position); this is not a new
Console lint or a completed runtime verdict.
Source: [upstream releases](https://github.com/console-rs/console/releases).

Reqwest 0.13.4 → 0.13.5 (validation running): the September 12 registry check confirms
0.13.5 is stable and non-yanked, with the same Rust 1.85 minimum as 0.13.4.
Upstream fixes blocking-client timeout panics, wrapped timeout recognition,
request metadata preservation and proxy credential selection. It also adds
DNS-error classification and TLS-version metadata, and updates its own base64
dependency. The existing four real HTTP model-download tests cover successful,
missing and corrupt artifacts and cancellation/resumption; they do not prove
TLS or proxy behavior. The candidate changes only Reqwest's package entry,
including its required move to the already-resolved base64 0.23.1. Incidental
resolver changes to unrelated edges were manually restored to the preceding
validated candidate; `cargo metadata --locked` accepts the resulting graph.
The remote gate includes all-target Clippy and the four HTTP tests, with
compilation separated from their runtime timeout.
Source: [Reqwest 0.13.5 release](https://github.com/seanmonstar/reqwest/releases/tag/v0.13.5).

Further reviewed candidates, still awaiting individual update and tests:

- Rustls 0.23.44 changes key-log file permissions and ECH rejection name
  verification. Default ML-DSA support applies to AWS-LC; CASS explicitly uses
  Ring. Validate actual HTTPS observations, not merely the best-effort
  `release-verify` command's exit status.
  [Release notes](https://github.com/rustls/rustls/releases/tag/v/0.23.44).
- Lru 0.18.3/0.18.4 add sparse construction and retention. The direct 0.18.2
  already contains the `pop` panic-safety fix; upgrading it does not remove
  the separate transitive 0.16.4 advisory.
  [Published changelog](https://static.crates.io/crates/lru/lru-0.18.4.crate).
- Which 8.0.6 fixes relative PATH entries with an explicitly supplied working
  directory. CASS's only direct consumer locates PowerShell for installer
  tests; an early return when PowerShell is absent is not behavioral coverage.
  [Published changelog](https://static.crates.io/crates/which/which-8.0.6.crate).
- Smallvec 1.16.0 changes debugger visualization packaging; 1.16.1 optimizes
  push and fixes warnings. No CASS call-site migration has been identified.
  [1.16.0 notes](https://github.com/servo/rust-smallvec/releases/tag/v1.16.0),
  [1.16.1 notes](https://github.com/servo/rust-smallvec/releases/tag/v1.16.1).
- Flate2 1.1.10 updates miniz_oxide to 0.9 and rejects incomplete deflate
  streams at EOF. It also fixes gzip write loops and oversized extra fields;
  CASS's pages encryption uses the deflate interfaces. Retain roundtrip,
  bounded decompression and corrupt-payload checks when adopting it.
  [Release notes](https://github.com/rust-lang/flate2-rs/releases/tag/1.1.10).

Pending after console: reqwest, rustls, toml, which, lru, smallvec,
FrankenTUI (four coordinated direct crates), dirs, frankensearch, wide,
argon2 and flate2. Major API changes and the documented frankensearch hold
require source review before adoption. Existing full-test and strict UBS
release requirements remain in force.

Publication baseline: GitHub v0.8.0; Homebrew v0.7.1; crates.io v0.7.0.
The next release must verify every applicable venue rather than infer
publication from a Git tag. No new version bump or release has happened yet.

New release blocker: [CASS #462](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/462)
reports a deterministic open failure on a 35.7 GB archive with the current
FrankenSQLite pin. Two source reviews identified a plausible conflict between
reserved-page freelist repair and the WAL erasure guard. A narrow upstream
candidate and sparse WAL regression are being tested; neither reporter-archive
verification nor CASS adoption is complete. The release bead depends on
`coding_agent_session_search-p3gh0`; it is not being treated as fixed by the
unrelated dependency updates.

Interim `cargo audit` on the crossbeam candidate completed with exit 0 and
zero vulnerability entries. It still reports the existing unmaintained
`paste` and `rustls-pemfile` dependencies, plus RUSTSEC-2026-0253 for the
transitive `lru` 0.16.4. The latter is an unsoundness warning concerning a
panicking key destructor during `pop`; upgrading CASS's separate direct
`lru` dependency does not remove that transitive copy. This is an interim
inventory, not the final dependency or release security gate.

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
- `docs/planning/UPGRADE_LOG.md` (this entry)
