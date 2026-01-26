# Dependency Upgrade Log

**Date:** 2026-01-26
**Project:** coding-agent-search
**Language:** Rust
**Manifest:** Cargo.toml

---

## Summary

| Metric | Count |
|--------|-------|
| **Total dependencies reviewed** | 11 |
| **Updated** | 0 |
| **Skipped** | 8 |
| **Failed (blocked by constraints)** | 3 |
| **Requires attention** | 0 |

---

## Failed / Blocked Updates

### thiserror: 1.0.69 → 2.0.18

**Changelog:** https://github.com/dtolnay/thiserror/releases/tag/2.0.0

**Breaking changes (2.0.0):**
- `{r#field}` raw identifiers in format strings are no longer accepted
- Trait bounds no longer inferred when explicit named args shadow fields
- Tuple field indices cannot be mixed with extra positional args

**Attempted update:**
```
cargo update -p thiserror@1.0.69 --precise 2.0.18
```

**Blocked by:** `termwiz v0.23.3` (via `ratatui-termwiz v0.1.0` → `ratatui 0.30.0`) requiring `thiserror ^1.0`.

**Action:** Skipped (no lockfile change).

---

### rand: 0.8.5 → 0.9.2

**Changelog:** https://github.com/rust-random/rand/blob/master/CHANGELOG.md

**Breaking changes (0.9.0):**
- `Rng::gen` → `random`, `Rng::gen_range` → `random_range`
- `rand::thread_rng()` → `rand::rng()`
- `rand::distributions` → `rand::distr`

**Attempted update:**
```
cargo update -p rand@0.8.5 --precise 0.9.2
```

**Blocked by:** `rand_distr v0.4.3` (via `tantivy-stacker v0.6.0` → `tantivy 0.25.0`) requiring `rand ^0.8`.

**Action:** Skipped (no lockfile change).

---

### security-framework: 2.11.1 → 3.5.1

**Attempted update:**
```
cargo update -p security-framework@2.11.1 --precise 3.5.1
```

**Blocked by:** `native-tls v0.2.14` (via `ureq 3.1.4` → `ort 2.0.0-rc.11` → `fastembed 5.8.1`) requiring `security-framework ^2.0`.

**Action:** Skipped (no lockfile change).

---

## Skipped (Blocked or Already Latest)

### rand_chacha: 0.3.1 → 0.9.0
**Reason:** Blocked by `rand` staying on 0.8.x; upgrading would cause rand_core trait mismatches.

### rand_core: 0.6.4 → 0.9.5
**Reason:** Blocked by `rand` staying on 0.8.x.

### getrandom: 0.2.17 → 0.3.4
**Reason:** Blocked by `rand_core 0.6.x` in `rand 0.8.x`.

### core-foundation: 0.9.4 → 0.10.1
**Reason:** Blocked by `security-framework 2.x` dependency chain.

### thiserror-impl: 1.0.69 → 2.0.18
**Reason:** Blocked by `thiserror` staying on 1.0.x.

### libc: 0.2.180 → Removed
**Reason:** Reported as Removed by `cargo outdated`; transitive dependency with no update path.

### wasi: 0.11.1+wasi-snapshot-preview1 → Removed
**Reason:** Reported as Removed by `cargo outdated`; transitive dependency with no update path.

### reqwest: 0.13.1 → 0.13.1
**Reason:** Already on latest stable (not listed by `cargo outdated`).

---

## Tests

- Not run yet (no dependency updates applied).

---

## Commands Used

```bash
cargo outdated -w
cargo update -p thiserror@1.0.69 --precise 2.0.18
cargo update -p rand@0.8.5 --precise 0.9.2
cargo update -p security-framework@2.11.1 --precise 3.5.1
```

---

## Notes

- Upgrades are blocked by upstream dependencies that already appear to be at their latest stable versions.
- Revisit when `ratatui`/`termwiz`, `tantivy`, or `native-tls` move to newer major versions that lift these constraints.
