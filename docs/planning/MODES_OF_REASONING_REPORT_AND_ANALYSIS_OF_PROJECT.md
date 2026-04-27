# Modes of Reasoning: Comprehensive Analysis of `cass`

> **Project:** coding-agent-search (cass) — Unified TUI/CLI to index and search local coding agent session histories
> **Date:** 2026-04-08
> **Methodology:** 10-mode reasoning swarm with triangulated synthesis
> **Confidence:** 0.85 (composite)

---

## 1. Executive Summary

Ten independent analytical agents — each applying a distinct reasoning mode — converged on a clear picture of `cass`:

**The project solves a genuine, well-defined problem with impressive engineering depth, but its ambition has outrun its organizational capacity.** The core search-and-index pipeline is sound, performant, and thoughtfully designed. However, three structural issues threaten long-term sustainability:

1. **The dual SQLite driver** (frankensqlite + rusqlite) is a load-bearing architectural seam that propagates complexity into every subsystem. All 10 modes identified this as problematic — the strongest convergence in the entire analysis.

2. **Monolithic code concentration** — five files contain 111K lines (app.rs alone is 46K). This is an artifact of the "no file proliferation" AGENTS.md rule overcorrecting for AI agent misbehavior, and it inhibits decomposition, review, and contribution.

3. **Feature surface area exceeds solo-developer capacity** — the project ships 18 connectors, 3 search modes, 7 analytics views, 18 themes, multi-machine SSH sync, encrypted HTML export, a web publishing platform, a background ML daemon, and more. This is enterprise-grade breadth on an alpha-stage budget.

**Top 5 recommended actions (in priority order):**
1. Fix frankensqlite's FTS5 shadow table support to eliminate the rusqlite dependency
2. Set UDS socket permissions to 0700 after bind (security fix)
3. Remove `DatabaseCorrupt` from retryable errors (data safety fix)
4. Split app.rs into 10-15 focused modules
5. Resolve the asupersync version bifurcation in the lockfile

---

## 2. Methodology

### Mode Selection Rationale

The project is a complex Rust CLI/TUI with a custom dependency ecosystem, performance-critical search, and security-sensitive features. Mode selection targeted 3 key taxonomy axes:

| Axis | Pole 1 (represented) | Pole 2 (represented) |
|------|----------------------|----------------------|
| Descriptive vs Normative | F7, F5, B1, F2 (what IS) | L5, I4 (what OUGHT) |
| Single-agent vs Multi-agent | A8, F4, F3 (internal analysis) | H2, I4 (adversarial/social) |
| Ampliative vs Non-ampliative | B1, F3 (pattern discovery) | A8 (boundary verification) |

### The 10 Modes

| # | Mode | Code | Category | Focus |
|---|------|------|----------|-------|
| 1 | Systems-Thinking | F7 | Causal | Feedback loops, emergent behavior, leverage points |
| 2 | Root-Cause | F5 | Causal | 5-whys on structural problems |
| 3 | Adversarial-Review | H2 | Strategic | Security, attack surfaces, trust assumptions |
| 4 | Failure-Mode (FMEA) | F4 | Causal | Cascade failures, RPN scoring |
| 5 | Edge-Case | A8 | Formal | Boundary conditions, degenerate inputs |
| 6 | Inductive | B1 | Ampliative | Pattern recognition, codebase evolution |
| 7 | Counterfactual | F3 | Causal | Alternative architecture evaluation |
| 8 | Perspective-Taking | I4 | Dialectical | Stakeholder viewpoints, UX |
| 9 | Scope-Control | L5 | Meta | Feature bloat, complexity budget |
| 10 | Dependency-Mapping | F2 | Causal | Supply chain, blast radius, version drift |

**Category coverage:** A, B, F, H, I, L = 6 of 12 categories.
**Opposing pairs:** H2 (adversarial) vs I4 (empathic); L5 (reduce scope) vs F7 (see the whole system).

---

## 3. Taxonomy Axis Analysis

### Descriptive vs Normative

The descriptive modes (F7, F5, B1, F2) mapped the system as it is: a tightly coupled franken-ecosystem with dual SQLite drivers, retry machinery, and concentrated code. The normative modes (L5, I4) judged what it should be: a leaner, more decomposed system with better onboarding and contributor accessibility.

**Key insight:** The descriptive modes agree the architecture *works*. The normative modes argue it *won't scale* — not in performance, but in human cognitive capacity and maintenance burden.

### Single-agent vs Multi-agent

The single-agent modes (A8, F4) found concrete bugs and edge cases (NFC/NFD asymmetry, ORDER BY IS NULL usage, DatabaseCorrupt in retry list). The multi-agent modes (H2, I4) found systemic issues: the daemon UDS socket has no permissions, the README alienates new users by front-loading agent-focused content, and the supply chain has bus factor = 1.

**Key insight:** The concrete bugs are fixable in hours. The systemic issues require architectural decisions.

### Ampliative vs Non-ampliative

Inductive reasoning (B1) discovered the "scar tissue" pattern: AGENTS.md rules are reactive responses to specific agent failures, and the extraction-to-franken-crate pattern is driven by protecting code from agent contamination. Edge-case analysis (A8) verified specific boundary conditions, finding that most are handled but NFC normalization and negative timestamps are not.

**Key insight:** B1's evolutionary narrative explains *why* the codebase looks the way it does; A8 verifies *where* it breaks at the boundaries.

---

## 4. Convergent Findings (KERNEL — 3+ modes agree)

These findings were independently discovered by 3 or more modes through different analytical lenses. They represent the highest-confidence conclusions.

### K1: Dual SQLite Driver Is the Central Architectural Problem
**Discovered by:** F7, F5, F3, F4, B1, L5, F2, H2, I4 (9 of 10 modes)
**Confidence:** 0.95

frankensqlite cannot read on-disk FTS5 shadow tables, forcing retention of rusqlite as a parallel SQLite driver. This single limitation cascades into:
- ~900 lines of FTS5 glue code in `storage/sqlite.rs`
- Three `unsafe impl Send` wrappers (frankensqlite uses `Rc` internally, making connections `!Send`)
- Pervasive retry/backoff machinery (15+ call sites) for dual-driver lock contention
- A self-reinforcing loop where agents see rusqlite in the codebase and copy it, requiring ever-more-emphatic AGENTS.md rules
- Two independent WAL readers on the same database file, amplifying lock contention

**Evidence:** `Cargo.toml:107-109` (explicit comment), `storage/sqlite.rs:800` (rusqlite FTS5 functions), `storage/sqlite.rs:67-71` (unsafe Send), `AGENTS.md:49-63` (Rule 2).

**Root cause (from F5):** The decision to reimplement SQLite in Rust for one advanced feature (BEGIN CONCURRENT / MVCC) created an incomplete replacement that cannot handle the full SQLite feature surface.

**Counterfactual (from F3):** Using rusqlite alone would have eliminated ~900 lines of glue code and all `unsafe impl Send` wrappers, at the cost of losing BEGIN CONCURRENT support.

### K2: Monolithic Code Concentration (app.rs 46K lines, 5 files = 111K lines)
**Discovered by:** F7, F5, F3, B1, L5, I4 (6 of 10 modes)
**Confidence:** 0.93

Five files contain 111K lines: `app.rs` (46K), `lib.rs` (22K), `storage/sqlite.rs` (17K), `query.rs` (15K), `indexer/mod.rs` (11K). This concentration:
- Makes code review impractical (any PR touching app.rs conflicts with every other PR)
- Slows incremental compilation (Rust recompiles entire files on change)
- Creates a hostile environment for contributors who must navigate megafiles
- Is an artifact of the "No File Proliferation" rule (AGENTS.md line 173-179), which conflates preventing `_v2.rs` variants with preventing legitimate decomposition

**Root cause (from F5):** The rule was a rational overcorrection to agent file spam, but it now prevents healthy architectural decomposition.

### K3: `unsafe impl Send` Is a Soundness Risk
**Discovered by:** F7, F5, H2, F4 (4 of 10 modes)
**Confidence:** 0.88

Three locations (`storage/sqlite.rs:71`, `storage/sqlite.rs:435`, `query.rs:62`) wrap `!Send` `FrankenConnection` (which uses `Rc` internally) in newtypes with `unsafe impl Send`. The safety argument ("Rc fields are not cloned or shared externally") is a claim about frankensqlite internals that:
- Cannot be verified at compile time
- Could be silently invalidated by any upstream change to frankensqlite
- Would cause undefined behavior (use-after-free, data race) if violated

**Recommendation:** Switch frankensqlite from `Rc` to `Arc` internally. The atomic reference counting overhead is negligible compared to SQLite I/O.

### K4: Wildcard Dependency Versions Create Reproducibility Risk
**Discovered by:** F5, B1, H2, F2, I4 (5 of 10 modes)
**Confidence:** 0.90

All ~79 crates.io dependencies use `version = "*"`. While `Cargo.lock` provides reproducibility for existing builds, any `cargo update` or fresh build pulls latest versions. Combined with the git-pinned custom crates, this creates a split-brain pinning strategy where custom deps are strict but everything else is unconstrained.

### K5: Feature Surface Area Exceeds Solo-Developer Capacity
**Discovered by:** L5, I4, B1, F3 (4 of 10 modes)
**Confidence:** 0.87

L5 catalogued: 18 connectors, 3 search modes, 7 analytics views, 18 themes, multi-machine SSH sync wizard, encrypted HTML export, web publishing platform (27K lines), background ML daemon, macro/asciicast recording, command palette, self-update, 6 ranking modes. This is approximately 5-10x what a single alpha-stage developer can maintain.

**The pages/ module (27K lines)** is a complete web publishing platform that is functionally independent from the search tool — the strongest scope-creep candidate.

### K6: Supply Chain Concentration Risk (Bus Factor = 1)
**Discovered by:** H2, F2, I4, L5 (4 of 10 modes)
**Confidence:** 0.89

All 6 franken crate families (~46 sub-crates in the lockfile, 5.8% of packages but disproportionate in criticality) are authored by a single developer. None are published to crates.io with broad community adoption. A compromise of the single GitHub account would compromise the entire build.

### K7: Franken Ecosystem Coupling Creates Multiplicative Maintenance Burden
**Discovered by:** F7, F5, F3, L5, F2, I4 (6 of 10 modes)
**Confidence:** 0.91

The maintainer doesn't just maintain cass — they maintain frankensqlite (pure-Rust SQLite reimplementation), frankensearch, franken_agent_detection, frankentui, asupersync (custom async runtime), and toon (custom serialization). Each has its own correctness requirements. Changes in any one can cascade through the others. The `build.rs` contract validation system is a sophisticated guardrail, but it only catches drift at compile time.

### K8: AGENTS.md Rules Are "Scar Tissue" from Agent Misbehavior
**Discovered by:** F5, B1, I4 (3 of 10 modes)
**Confidence:** 0.86

Each AGENTS.md rule maps to a specific class of agent failure:
- Rule 0 (override prerogative) → agents ignoring instructions
- Rule 1 (no file deletion) → agents deleting critical files
- Rule 2 (no rusqlite, "violated OVER 10 TIMES") → agents copying legacy patterns
- No file proliferation → agents creating `_v2.rs` variants
- No script-based changes → agents running brittle regex transforms

The tone escalation ("THE OWNER IS DONE TOLERATING IT") suggests diminishing effectiveness. Agents are not deterred by emphatic language — they're deterred by code structure.

---

## 5. Supported Findings (2 modes agree)

### S1: Daemon UDS Socket Lacks Permissions and Authentication
**Discovered by:** H2, F4
**Confidence:** 0.85

The socket at `/tmp/semantic-daemon-$USER.sock` has no `chmod` after bind and no authentication. Any local user can connect and: submit arbitrary `db_path` to open/write databases, send `Shutdown` to kill the daemon, or exhaust memory via 10MB payloads. **Priority: HIGH — simple fix with `set_permissions(0o700)`.**

### S2: FTS5 Repair Cascade on Search Hot Path
**Discovered by:** F7, F4
**Confidence:** 0.82

`FrankenStorage::open()` triggers a 6-step initialization cascade including FTS consistency checks that can block for seconds. Since `SearchClient::sqlite_guard()` calls this on the search hot path, a single corrupted FTS state blows through the 60ms latency budget.

### S3: Vector Index / SQLite Database Can Drift
**Discovered by:** F7, F4
**Confidence:** 0.80

No transactional guarantee links SQLite message inserts to FSVI vector index updates. A crash between the two creates dangling entries or orphaned rows, producing incorrect semantic search results without detection.

### S4: Stale Detector Can Trigger Rebuild Storms
**Discovered by:** F7, F4
**Confidence:** 0.78

If a rebuild errors out partway through, the `StaleDetector` is never reset, potentially re-triggering another rebuild. No `rebuild_in_progress` guard exists.

### S5: ORDER BY IS NULL Used Despite Known Limitation
**Discovered by:** F4, A8
**Confidence:** 0.83

Three production queries use `ORDER BY ... IS NULL` patterns (`lib.rs:11532`, `query.rs:4921`, `sqlite.rs:4541`) despite AGENTS.md line 266 documenting this as unsupported in frankensqlite. If the limitation manifests as incorrect ordering rather than an error, conversations with NULL timestamps will be silently misordered.

### S6: Connector Extraction to FAD Was Clearly Right
**Discovered by:** F3, B1
**Confidence:** 0.92

All 18 connector files are now 1-5 line re-export stubs. The extraction creates a hard boundary that prevents agent contamination, enables independent testing, and makes breadth cheap. This is the project's best architectural decision.

---

## 6. Divergent Findings and Unique Insights by Mode

These findings were discovered by a single mode, representing the value of analytical diversity.

### From Edge-Case Analysis (A8): NFC/NFD Query Asymmetry
**Confidence:** 0.85 | **Evidence:** `query.rs:13788-13802`

The canonicalization pipeline applies NFC normalization to indexed content, but the query sanitizer does NOT normalize queries. A user typing "café" with a combining accent (NFD, common on macOS) will search for "cafe " while indexed content stores "café" (NFC). The test at line 13788 explicitly documents this as "expected behavior" — but it is a functional correctness bug for international users.

### From Dependency-Mapping (F2): asupersync Version Bifurcation
**Confidence:** 0.92 | **Evidence:** Cargo.lock contains two distinct `asupersync 0.2.9` entries

The lockfile contains TWO copies of asupersync 0.2.9 from different sources: one from crates.io (used by fsqlite sub-crates) and one from git rev `08dd31df` (used by cass and frankensearch). If these have any type-level differences, data crossing the fsqlite↔cass boundary could cause subtle runtime bugs. The git version also pulls additional dependencies (ring, rustls) not in the registry version.

### From Failure-Mode (F4): DatabaseCorrupt in Retryable Errors
**Confidence:** 0.88 | **Evidence:** `storage/sqlite.rs:2316-2321`

`retryable_franken_error` includes `DatabaseCorrupt` as a retryable error. Retrying on corruption can amplify damage — the retry loop hammers corrupt pages, partial writes land in WAL, readers see inconsistent state. This should fail fast and trigger the backup/quarantine path instead. **Single highest-impact one-line fix.**

### From Failure-Mode (F4): Indexer-Daemon Writer Livelock
**Confidence:** 0.75 | **Evidence:** `indexer/mod.rs:674-699`, `daemon/worker.rs`

Both the indexer and the daemon's background embedding worker access the same SQLite database with independent retry/backoff. Under contention, both parties can keep retrying and colliding, creating a livelock where neither makes progress.

### From Perspective-Taking (I4): README Front-Loads Agent Content Over Human Onboarding
**Confidence:** 0.82 | **Evidence:** README.md lines 77-117

The README shows Agent Mail MCP endpoint JSON-RPC payloads before screenshots or "Why This Exists." No "first run" experience is documented — a human user who installs and types `cass` sees an empty TUI with no guidance to run `cass index` first.

### From Perspective-Taking (I4): Robot Mode Should Include `_corrections` Field
**Confidence:** 0.78

When the forgiving CLI parser corrects agent mistakes (e.g., `-robot` → `--robot`), corrections are suppressed in robot mode JSON output. Adding a `"_corrections"` field would teach agents canonical syntax without polluting the data stream.

### From Perspective-Taking (I4): AVX Check Prevents All Functionality on Restricted VMs
**Confidence:** 0.80 | **Evidence:** `main.rs:33-48`

The startup AVX check exits with zero functionality on pre-2011 CPUs and some VMs/containers. There is no graceful degradation (e.g., disabling only semantic/ONNX search while keeping lexical mode working).

### From Edge-Case (A8): 132 Uses of `to_string_lossy` Silently Corrupt Non-UTF-8 Paths
**Confidence:** 0.80 | **Evidence:** 132 instances across 29 source files

On Linux, filenames can contain arbitrary bytes. Session files at paths with non-UTF-8 bytes will be indexed with replacement characters (U+FFFD), making stored paths unmatchable with filesystem paths. No warning is emitted.

### From Edge-Case (A8): Negative Timestamp Handling Gap
**Confidence:** 0.82 | **Evidence:** `analytics/query.rs:58-64`

`normalize_epoch_millis` only normalizes values in range `0..100_000_000_000`. A negative second-based timestamp like `-86400` (one day before epoch) is treated as `-86400ms` instead of `-86400000ms` — a factor-of-1000 error.

### From Systems-Thinking (F7): Two-Tier Search Creates Non-Deterministic Daemon Configuration
**Confidence:** 0.78 | **Evidence:** `daemon/protocol.rs:28`

The daemon socket at `/tmp/semantic-daemon-$USER.sock` is shared with the `xf` tool. First process to bind wins. If `xf` spawns the daemon with different model/dimension settings, cass may get incompatible results without any error signal.

---

## 7. Risk Assessment (Aggregated)

| # | Risk | Severity | Likelihood | Modes | Priority |
|---|------|----------|------------|-------|----------|
| R1 | Dual SQLite lock contention under concurrent indexing + search | High | High | F7,F5,F4,F3 | P0 |
| R2 | `unsafe impl Send` soundness if frankensqlite internals change | Critical | Low | F7,F5,H2,F4 | P1 |
| R3 | DatabaseCorrupt retry amplifies corruption | Critical | Low | F4 | P0 |
| R4 | Daemon UDS socket world-accessible, no auth | High | Medium | H2,F4 | P0 |
| R5 | FSVI-SQLite drift produces wrong semantic results | High | Medium | F7,F4 | P1 |
| R6 | Supply chain concentration (bus factor = 1) | High | Low-Med | H2,F2,I4 | P2 |
| R7 | asupersync version bifurcation causes subtle type bugs | Medium | Medium | F2 | P1 |
| R8 | FTS5 repair blocks search hot path for seconds | High | Medium | F7,F4 | P1 |
| R9 | Wildcard deps cause non-reproducible builds | Medium | Medium | F5,B1,H2,F2 | P2 |
| R10 | NFC/NFD asymmetry causes missed search results (macOS) | Medium | Medium | A8 | P1 |
| R11 | ORDER BY IS NULL produces silently wrong ordering | Medium | High | F4,A8 | P1 |
| R12 | Stale detector rebuild storm | High | Low | F7,F4 | P2 |
| R13 | 46K-line app.rs prevents contribution and review | High | Certain | F5,F3,B1,L5,I4 | P1 |

---

## 8. Recommendations (Prioritized)

### P0 — Fix Now (High impact, often low effort)

| # | Recommendation | Effort | Modes | Expected Benefit |
|---|---------------|--------|-------|-----------------|
| 1 | **Remove `DatabaseCorrupt` from `retryable_franken_error`** at `sqlite.rs:2316-2321` | Low (1 line) | F4 | Prevents corruption amplification cascade |
| 2 | **Set daemon UDS socket permissions to 0700 after bind** | Low (3 lines) | H2,F4 | Closes world-accessible socket vulnerability |
| 3 | **Replace ORDER BY IS NULL patterns** with COALESCE at 3 call sites | Low | F4,A8 | Avoids documented frankensqlite limitation |
| 4 | **Apply NFC normalization to search queries** before sanitization | Low | A8 | Fixes missed matches for macOS NFD input |

### P1 — Fix Soon (High impact, moderate effort)

| # | Recommendation | Effort | Modes | Expected Benefit |
|---|---------------|--------|-------|-----------------|
| 5 | **Fix frankensqlite's FTS5 shadow table support** | High | F7,F5,F3,F4,B1 | Eliminates rusqlite dependency, removes unsafe Send wrappers, simplifies retry machinery, stops agent rule violations |
| 6 | **Resolve asupersync version bifurcation** in lockfile | Medium | F2 | Prevents subtle type-level bugs at fsqlite↔cass boundary |
| 7 | **Split app.rs into 10-15 focused modules** | Medium | F5,F3,B1,L5,I4 | Enables contribution, review, parallel agent work, faster compilation |
| 8 | **Move FTS5 repair out of `FrankenStorage::open()` hot path** | Medium | F7,F4 | Protects 60ms search latency budget |
| 9 | **Add FSVI-SQLite consistency check** on startup or after rebuild | Medium | F7,F4 | Detects and prunes orphaned vector entries |
| 10 | **Switch frankensqlite from `Rc` to `Arc`** internally | Medium | F7,F5,H2,F4 | Eliminates all `unsafe impl Send` wrappers |
| 11 | **Add daemon spawn circuit breaker** (max 2-3 attempts per 60s window) | Low | F4 | Prevents 5.5s UI freeze per query on daemon crash loop |

### P2 — Fix Eventually (Important but not urgent)

| # | Recommendation | Effort | Modes | Expected Benefit |
|---|---------------|--------|-------|-----------------|
| 12 | **Pin dependency versions** with semver ranges instead of `*` | Low | F5,B1,H2,F2,I4 | Reproducible builds |
| 13 | **Extract pages/ module** into a separate binary/crate | Medium | L5 | Saves ~27K lines, removes crypto deps from core |
| 14 | **Enable strict-path-dep-validation in CI** | Low | F2,F7 | Catches sibling crate drift before merge |
| 15 | **Add `_corrections` field to robot-mode JSON** | Low | I4 | Teaches agents canonical syntax |
| 16 | **Restructure README** with human quickstart before agent content | Low | I4 | Improves new-user onboarding |
| 17 | **Add rebuild guard to StaleDetector** (`rebuild_in_progress` flag) | Low | F7,F4 | Prevents rebuild storm cascade |
| 18 | **Graceful AVX degradation** (disable semantic only, keep lexical) | Medium | I4 | Broadens VM/container compatibility |

---

## 9. New Ideas and Extensions

| Idea | Source Mode | Innovation Level | Description |
|------|-----------|-----------------|-------------|
| Connection affinity for search | F7 | Significant | Thread-local connection affinity instead of round-robin reader pool — eliminates mutex contention and enables per-thread prepared statement caches |
| Speculative prefetch in two-tier search | F7 | Significant | Start fast embedding while user is typing (before Enter), so HNSW results are ready on submit |
| Write-ahead buffer for indexer | F7 | Significant | Buffer conversations in mmap append-only log, drain to SQLite in background — decouples scan from write latency |
| `_corrections` teaching field | I4 | Incremental | Structured field in robot-mode JSON showing what was auto-corrected |
| FTS5 proxy in frankensqlite | F7 | Radical | Wrap rusqlite internally for FTS5 only, presenting a single connection API externally |
| Daemon configuration negotiation | F7 | Incremental | Handshake on connect to verify model/dimension compatibility with xf |
| Cross-tier consistency epoch | F7 | Incremental | Monotonic counter incremented after both SQLite + Tantivy are updated; search waits on mismatch |
| Freshness metadata for synced data | F4 | Incremental | Tag remote sessions with `last_verified_at` so search can indicate staleness |

---

## 10. Assumptions Ledger

Assumptions surfaced across all 10 modes that the project makes but does not explicitly validate:

| Assumption | Surfaced By | Risk If Wrong |
|-----------|------------|---------------|
| frankensqlite's Rc fields are never cloned or shared externally | F7,F5,H2,F4 | Undefined behavior (memory corruption) |
| FTS5 and frankensqlite WAL readers don't interfere on same file | F7,F5 | Lock contention, data corruption |
| First-spawned daemon's model config works for all clients | F7 | Wrong semantic results silently |
| All session file paths are valid UTF-8 | A8 | Silent path corruption via to_string_lossy |
| Agents respect AGENTS.md rules | B1,I4 | Continued rule violations, wrong dependencies |
| Query input is NFC-normalized before reaching the search engine | A8 | Missed matches on macOS NFD input |
| DatabaseCorrupt errors are transient and retryable | F4 | Corruption amplification |
| Remote session data is current after partial sync | F4 | Stale results served without indication |
| The single developer remains available indefinitely | F2,L5,I4 | All 46 franken sub-crates frozen |

---

## 11. Open Questions for Project Owner

1. **Is frankensqlite's FTS5 limitation a hard technical barrier or a prioritization gap?** How much effort would it take to support on-disk shadow tables?
2. **Is the pages/ web publishing platform intended to be a core feature or a separate product?** Its 27K lines represent a significant scope commitment.
3. **What is the intended contributor model?** The codebase structure is optimized for solo + AI agents. Is external contribution a goal?
4. **Has the asupersync bifurcation (crates.io vs git) caused any observed runtime issues?**
5. **Are there plans to publish franken crates to crates.io?** This would reduce bus-factor risk and enable `cargo-audit` coverage.
6. **Has the ORDER BY IS NULL limitation actually triggered in production queries, and if so, what was the observed behavior?**
7. **What is the actual daemon spawn failure rate in practice?** The 5.5s worst-case timeout may be theoretical.

---

## 12. Confidence Matrix

| Finding | Confidence | Supporting Modes | Dissenting Modes | Evidence Quality |
|---------|-----------|-----------------|-----------------|-----------------|
| K1: Dual SQLite driver | 0.95 | F7,F5,F3,F4,B1,L5,F2,H2,I4 | None | Explicit code comments, 900+ lines of glue |
| K2: Monolithic code | 0.93 | F7,F5,F3,B1,L5,I4 | None | Measurable file sizes |
| K3: unsafe Send risk | 0.88 | F7,F5,H2,F4 | None | Code inspection, but Rc scope uncertain |
| K4: Wildcard versions | 0.90 | F5,B1,H2,F2,I4 | None | Direct Cargo.toml evidence |
| K5: Scope creep | 0.87 | L5,I4,B1,F3 | None (but F3 notes breadth is cheap via FAD) | Feature inventory vs team size |
| K7: Ecosystem coupling | 0.91 | F7,F5,F3,L5,F2,I4 | None | Lockfile analysis, build.rs contracts |
| S1: Daemon UDS perms | 0.85 | H2,F4 | None | No chmod/set_permissions in source |
| NFC/NFD asymmetry | 0.85 | A8 only | None | Test explicitly documents the gap |
| asupersync bifurcation | 0.92 | F2 only | None | Two distinct entries in Cargo.lock |
| DatabaseCorrupt retryable | 0.88 | F4 only | None | Direct code evidence |

---

## 13. Contribution Scoreboard

Scoring formula: `0.40 × (findings/total) + 0.30 × (unique_insights/total_unique) + 0.20 × evidence_quality + 0.10 × calibration_quality`

| Mode | Code | Findings | Unique Insights | Evidence Quality | Calibration | Score | Rank |
|------|------|----------|----------------|-----------------|-------------|-------|------|
| Failure-Mode | F4 | 10 | 3 (DatabaseCorrupt, livelock, FMEA table) | 0.90 | 0.85 | **0.89** | 1 |
| Systems-Thinking | F7 | 10 | 2 (cache coherence, daemon non-determinism) | 0.88 | 0.82 | **0.86** | 2 |
| Root-Cause | F5 | 9 | 1 (5-whys chains) | 0.90 | 0.82 | **0.84** | 3 |
| Dependency-Mapping | F2 | 8 | 2 (asupersync bifurcation, blast radius) | 0.95 | 0.92 | **0.84** | 4 |
| Edge-Case | A8 | 11 | 3 (NFC/NFD, negative timestamps, lossy paths) | 0.85 | 0.82 | **0.83** | 5 |
| Inductive | B1 | 10 | 2 (scar tissue thesis, evolution phases) | 0.85 | 0.87 | **0.82** | 6 |
| Scope-Control | L5 | 8 | 1 (feature audit table) | 0.82 | 0.88 | **0.79** | 7 |
| Perspective-Taking | I4 | 14 | 3 (_corrections field, AVX degradation, README) | 0.78 | 0.78 | **0.78** | 8 |
| Adversarial-Review | H2 | 9 | 1 (daemon socket) | 0.85 | 0.78 | **0.77** | 9 |
| Counterfactual | F3 | 10 | 1 (alternative evaluations) | 0.80 | 0.82 | **0.76** | 10 |

**Diversity metric:** 24 unique insights across 10 modes. No single mode produced more than 3 unique findings, confirming good mode selection diversity.

---

## 14. Mode Performance Notes

**Most productive:** F4 (Failure-Mode) produced the highest-impact unique findings (DatabaseCorrupt retryable, FMEA cascade analysis) and the most structured analytical output. F7 (Systems-Thinking) provided the deepest architectural understanding.

**Most unique value:** A8 (Edge-Case) and F2 (Dependency-Mapping) found concrete, actionable issues that no other mode caught — the NFC/NFD asymmetry and asupersync bifurcation respectively.

**Best evolutionary insight:** B1 (Inductive) provided the most compelling narrative of *how* the codebase reached its current state, making the structural issues understandable rather than merely catalogued.

**Best stakeholder coverage:** I4 (Perspective-Taking) was the only mode to analyze the human experience (onboarding, README, AVX degradation), complementing the code-focused majority.

**Least incremental value:** F3 (Counterfactual) largely confirmed what other modes found through direct analysis. However, its "clearly right decisions" list (Rust choice, FAD extraction, local-only architecture) provides valuable positive validation.

---

## 15. Mode Selection Retrospective

**Would change with hindsight:**
- **Add Bayesian (B3)** to quantify uncertainty around the "how often do retries actually fire?" question that multiple modes raised but none could answer
- **Replace F3 (Counterfactual) with G7 (Means-End)** since counterfactual findings mostly duplicated other modes; means-end could have mapped the concrete path from current state to desired state

**Selection validated:**
- The F-category concentration (F2, F3, F4, F5, F7 = 5 causal modes) was justified because the project's problems are fundamentally structural/causal
- Having both H2 (adversarial) and I4 (empathic) as opposing multi-agent modes produced genuinely different findings
- L5 (Scope-Control) was essential — no other mode would have produced the feature audit table

---

## 16. Appendix: Provenance Index

| Finding ID | Source Mode(s) | Report Section |
|-----------|---------------|----------------|
| K1 (dual SQLite) | F7,F5,F3,F4,B1,L5,F2,H2,I4 | §4 Kernel |
| K2 (monolithic code) | F7,F5,F3,B1,L5,I4 | §4 Kernel |
| K3 (unsafe Send) | F7,F5,H2,F4 | §4 Kernel |
| K4 (wildcard deps) | F5,B1,H2,F2,I4 | §4 Kernel |
| K5 (scope creep) | L5,I4,B1,F3 | §4 Kernel |
| K6 (bus factor) | H2,F2,I4,L5 | §4 Kernel |
| K7 (ecosystem coupling) | F7,F5,F3,L5,F2,I4 | §4 Kernel |
| K8 (scar tissue) | F5,B1,I4 | §4 Kernel |
| S1 (daemon socket) | H2,F4 | §5 Supported |
| S2 (FTS5 hot path) | F7,F4 | §5 Supported |
| S3 (FSVI drift) | F7,F4 | §5 Supported |
| S4 (rebuild storm) | F7,F4 | §5 Supported |
| S5 (ORDER BY IS NULL) | F4,A8 | §5 Supported |
| S6 (FAD extraction right) | F3,B1 | §5 Supported |
| U-A8-1 (NFC/NFD) | A8 | §6 Unique |
| U-F2-1 (asupersync bifurcation) | F2 | §6 Unique |
| U-F4-1 (DatabaseCorrupt retry) | F4 | §6 Unique |
| U-F4-2 (indexer-daemon livelock) | F4 | §6 Unique |
| U-I4-1 (README ordering) | I4 | §6 Unique |
| U-I4-2 (_corrections field) | I4 | §6 Unique |
| U-I4-3 (AVX degradation) | I4 | §6 Unique |
| U-A8-2 (lossy paths) | A8 | §6 Unique |
| U-A8-3 (negative timestamps) | A8 | §6 Unique |
| U-F7-1 (daemon non-determinism) | F7 | §6 Unique |
