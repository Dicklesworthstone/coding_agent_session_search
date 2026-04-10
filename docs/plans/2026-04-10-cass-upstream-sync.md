# cass Upstream Sync + Rebuild Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task.
>
> **Status:** DRAFT — awaiting Lee's approval. Do NOT execute any destructive or state-changing step until Lee signs off.

**Goal:** Sync `cass` with 355 new commits from `upstream/main` (Dicklesworthstone/coding_agent_session_search), rebuild the binary, restart the index-watch daemon, and verify that agent-visible `cass search` noise is gone — without losing the local `feat/cass-monitor` feature or the monitor-patch working-tree edits.

**Architecture:** Isolated git worktree (`~/Projects/leegonzales/cass.sync`) off a fresh branch based on `upstream/main`. Replay only the local monitor feature (11 commits) and then reapply the uncommitted monitor-patch working tree on top. Servitor state (`.servitor/**`) is preserved via file-level copy, not git history. The running daemon stays up on the installed binary until the new build is verified; cut-over is a single `launchctl stop/start` + binary swap.

**Tech Stack:** Rust 1.x, cargo, FrankenSQLite (git dep, now with local path patch override), Tantivy, launchd. Build takes ~5-10 min clean.

---

## 0. Context (what we know)

### Symptoms (confirmed)
- `cass search --mode semantic` exits rc=9 with 484 lines of stderr (2 ERROR + 14 WARN + 467 INFO), ending in `semantic search failed: placeholder index out of range: 32767` (SQLite bind-param limit exceeded).
- `cass search --mode hybrid` — same category of failure.
- `cass search` (lexical) — **clean**, 1 stderr line, returns results.
- `cass stats` — **hangs indefinitely**, consumes up to 3.6 GB RAM, must be killed.
- Short-lived `cass` invocations print a wall of FrankenSQLite telemetry to stderr: `event="region_created"/region_closed"`, `jit_compile`/`jit trigger cache hit`, `ERROR WAL frame salt mismatch — chain terminated frame_index=N` (different N each run), `WARN skipping virtual table from sqlite_master (module not loaded) table=fts_messages`, `WARN parse recovery: skipping malformed statement`, `WARN Connection dropped without explicit close()`, `INFO checkpoint execution complete`.
- `com.cass.index-watch` daemon (PID 8568) is running normally, ~54% CPU, doing Tantivy segment merges. Its log at `~/Library/Logs/cass-index.log` (140 MB) contains zero WAL errors — only short-lived reader invocations trip them.

### Repo state
- Current branch: `main`
- `main` vs `origin/main`: 4 ahead, 0 behind
- `main` vs `upstream/main`: **68 ahead, 355 behind**
- Working tree: 16 modified source files + 20 untracked (mostly `.servitor/**`)
- Installed binary: `~/.local/bin/cass` v0.2.1, built **Mar 12 2026** (~1 month old)

### The 68 local commits (classified)
| Category | Count | Fate in sync |
|---|---|---|
| `chore(servitor)` wake/heartbeat/mail pings | 46 | **DROP** — pure journal state, upstream has no `.servitor/` dir |
| `feat(servitor)` infra additions | 2 | **DROP** — same reason |
| `feat(cass): stage resonance-baseline experiment workspace` (`4fb1d401`) | 1 | **DROP** — misnamed; only touches `.servitor/experiments/` |
| `feat: add base + domain SOPs for Geordi` (`279c8e16`) | 1 | **DROP** — only touches `.servitor/sops/` |
| `feat/cass-monitor` branch (11 commits incl. merge `f4ac9a8a`) | 11 | **KEEP** — real cass code, entirely in `src/monitor/**` |
| Docs commits (`0a5305cd`, `159bcc2b`, `f55c373b`, `965c9b73`) | 4 | **KEEP** — monitor design/plan docs |
| Subtotal | ~65+ | |

All `.servitor/**` content will be preserved via **file copy** from the current checkout into the new worktree, outside of git history. The real cass code work to preserve is **only the 11 monitor-branch commits**.

### Upstream work that fixes our symptoms
| Symptom | Upstream fix commit |
|---|---|
| Wall of stderr from short-lived opens | `b4bde82c fix: suppress frankensqlite internal telemetry in default log filter` |
| Semantic `placeholder index out of range: 32767` | `d36e2270 fix: add missing policy.rs and semantic_manifest.rs source files` + `e5c3bdf3 fix(search,status): pass db_available flag to lexical asset inspection and harden semantic error preservation` |
| WAL frame salt mismatch on reopen | `8a1c0e04 perf(indexer): defer WAL checkpoints and Tantivy updates during bulk imports; add fast schema probe to bypass recovery path` + `50eac478 fix: bump frankensqlite to ff6a114b (1GB page buffer default)` + `99e2ddbe fix: ... stop retrying on DB corruption` |
| `fts_messages` module not loaded / parse recovery warns | `a0aa6f63 refactor: remove rusqlite FTS dual-backend, make frankensqlite sole DB engine` + `76aa4781 refactor(storage): replace rusqlite FTS lifecycle with native frankensqlite rebuild` |
| `cass stats` hang on large DB | `166122e6` / `860acb12` / `c38edcd9` — eliminate multi-table JOINs to avoid frankensqlite materialization |

### Predicted merge-conflict hotspots (uncommitted working tree vs upstream)
| File | Our edit (lines) | Upstream commits touching it | Risk |
|---|---|---|---|
| `src/lib.rs` | 24 | 81 | **HIGH** |
| `src/storage/sqlite.rs` | 10 | 80 | **HIGH** |
| `src/indexer/mod.rs` | 19 | 73 | **HIGH** |
| `src/ui/app.rs` | 5 | 59 | **MEDIUM** |
| `tests/cli_dispatch_coverage.rs` | 88 | 14 | **MEDIUM** |
| `src/monitor/**` (5 files) | ~474 total | **0** (doesn't exist upstream) | **NONE** |
| `src/daemon/resource.rs` | 6 | 3 | **LOW** |
| `src/ui/components/theme.rs` | 26 | 2 | **LOW** |
| `src/ui/style_system.rs` | 4 | 4 | **LOW** |
| `src/indexer/redact_secrets.rs` | 5 | 3 | **LOW** |
| `tests/frankensqlite_concurrent_stress.rs` | 21 | 2 | **LOW** |
| `.beads/.gitignore` | 21 | ? | **LOW** |

### Blocker: upstream Cargo.toml requires a sibling `../frankensqlite` checkout
Upstream `Cargo.toml` contains:
```toml
[patch."https://github.com/Dicklesworthstone/frankensqlite"]
fsqlite = { path = "../frankensqlite/crates/fsqlite" }
fsqlite-types = { path = "../frankensqlite/crates/fsqlite-types" }
```
But `~/Projects/leegonzales/frankensqlite/` **does not exist**. Neighbouring siblings `frankensearch`, `frankentui`, `franken_agent_detection` exist; `frankensqlite` does not. We must clone it before the first `cargo check`.

Upstream also likely expects a sibling `../asupersync` (new dep `asupersync = git "...Dicklesworthstone/asupersync"`), and may have a patch override for it. Needs verification in Task 3.

---

## Strategy decision

**Use an isolated worktree**, not in-place manipulation of `main`. Rationale:
- The running daemon has the installed binary file-locked and the DB write-locked. Building & experimenting in-place risks cross-contamination and makes rollback hard.
- A worktree lets us keep `main` as a rollback checkpoint. If the sync fails we simply `rm -rf` the worktree and nothing in the original checkout moved.
- Per global CLAUDE.md: "Worktrees inside project: Use `.worktrees/` (add to `.gitignore`)."

**Sync method: cherry-pick, not rebase or merge.**
- Rebase of 68 commits on top of 355 new commits across a major storage refactor is the worst-case merge experience and will bury the real work in servitor noise.
- Merge would preserve history but pulls in 355 commits as a giant merge commit — fine for audit but harder to bisect later.
- Cherry-pick of just the 11 monitor-branch commits onto `upstream/main` gives the smallest possible conflict surface and the cleanest history.

**Working-tree strategy: stash first, reapply last.**
- Stash the working tree to a named stash AND to a parallel wip branch before anything destructive.
- Apply the stash on top of the synced branch last, resolving conflicts against upstream refactors there.

---

## FMEA — Failure Mode & Effects Analysis

Severity (S): 1=cosmetic, 10=catastrophic (data loss / unrecoverable).
Occurrence (O): 1=very unlikely, 10=near-certain.
Detection (D): 1=obvious immediately, 10=silent/latent.
RPN = S × O × D. Anything ≥100 is red; ≥60 is amber.

| # | Step | Failure mode | Effect | S | O | D | RPN | Mitigation (before) | Rollback (after) |
|---|---|---|---|---|---|---|---|---|---|
| F1 | 1 Quiesce daemon | `launchctl stop` succeeds but daemon still holds DB lock / segment merge mid-flight corrupts Tantivy | Tantivy index inconsistency, forced reindex | 6 | 2 | 4 | **48** | Wait 10s, verify `ps` shows no `cass` proc before any DB touch. Check `cass-index.log` tail for "shutdown complete" marker | `launchctl start com.cass.index-watch` — daemon will recover from WAL on next open |
| F2 | 2 Snapshot uncommitted work | Stash fails silently on untracked files, or `.servitor/` gets partially staged | Lose in-flight monitor-patch edits | **10** | 2 | 6 | **120** 🔴 | Belt-and-suspenders: (a) `git stash push -m …`, (b) ALSO `git branch wip/pre-sync-snapshot-2026-04-10 HEAD` after an unstaged commit-all, (c) ALSO `rsync -a` the whole checkout to `~/cass-backup-2026-04-10/` as a filesystem-level safety copy | Restore from rsync backup |
| F3 | 2 Snapshot | Servitor journal files are still being written by a running Geordi servitor loop while we snapshot | Snapshot is inconsistent | 4 | 3 | 5 | 60 ⚠ | Check for running servitor processes (`ps auxww \| grep servitor`). Pause any heartbeat loops before snapshot | Reapply servitor state from backup |
| F4 | 3 Create worktree | `.worktrees/sync/` path already exists from prior attempt | `git worktree add` fails; no state change | 2 | 2 | 1 | 4 | Check `git worktree list` first; remove stale entries with `git worktree remove --force` | N/A |
| F5 | 4 Clone `frankensqlite` sibling | Public repo access denied / rev not found / private repo | Build will fail at Task 8 | 4 | 3 | 2 | 24 | Verify with `git ls-remote https://github.com/Dicklesworthstone/frankensqlite` BEFORE any further work. Also verify we can check out rev `ff6a114b` | Delete partial clone; revert to non-patched git dep |
| F6 | 4 Clone siblings | Missing `../asupersync` or other new sibling deps we didn't discover | `cargo check` fails mysteriously | 5 | 5 | 4 | **100** 🔴 | Task 3 (pre-sync Cargo.toml audit) must enumerate ALL path overrides in upstream Cargo.toml. Fail loudly if any expected sibling dir is missing before touching anything else | Clone missing siblings; retry |
| F7 | 5 Reset worktree to `upstream/main` | `upstream/main` is newer than our fetched copy; missing intermediate commits | Inconsistent base | 3 | 1 | 3 | 9 | Re-`git fetch upstream` right before reset. Verify tip SHA matches expected | Re-fetch + reset again |
| F8 | 6 Cherry-pick 11 monitor commits | Conflicts in shared files (`src/lib.rs`, `Cargo.toml`, `src/daemon/mod.rs`) where monitor commits registered the module | Cherry-pick aborts mid-sequence | 5 | **8** | 2 | **80** 🔴 | Expect conflicts. Plan to resolve `src/lib.rs` and `Cargo.toml` by reading upstream's new structure and re-registering the `monitor` module cleanly. Do cherry-picks one-at-a-time, not `-n` batch | `git cherry-pick --abort`; worktree is still valid; retry strategy |
| F9 | 6 Cherry-pick | Dependencies referenced by monitor commits no longer exist upstream (e.g., upstream renamed a struct) | Build fails after cherry-pick | 5 | 5 | 5 | **125** 🔴 | Run `cargo check -p cass` after EACH cherry-pick, not just at the end. If one fails, decide whether to (a) adapt the monitor commit to the new API, (b) squash+rewrite the monitor patch as a single adapted commit | Abort, revert worktree to upstream/main, write monitor patch as one new commit manually |
| F10 | 7 Copy `.servitor/` forward | `.servitor/` contents are stale and point at a previous servitor wake state | Geordi confused on next wake | 2 | 5 | 8 | **80** 🔴 | Document the copy time in `.servitor/state.json` last_wake. Run Geordi's health check after first wake post-sync | Manual state reset |
| F11 | 8 `cargo build --release` | Build fails due to new upstream API changes that break the monitor patch | Dead in the water | 6 | 6 | 2 | **72** 🔴 | This is expected. Budget 30-60 min for fixing API breakages. Common culprits: rusqlite→frankensqlite migration, tracing subscriber reconfiguration, `ui/app.rs` refactors | Not a rollback — an iterative fix loop |
| F12 | 8 Build | Build succeeds but new dep pulls down 1-2 GB of crates; disk pressure | Slow build, no functional impact | 2 | 3 | 2 | 12 | Check free disk in `~/.cargo` before start | Clean `~/.cargo/registry/cache` if needed |
| F13 | 9 Apply stash (monitor-patch working tree) | Conflicts in high-risk files (`src/storage/sqlite.rs`, `src/indexer/mod.rs`, `src/lib.rs`) — upstream rewrote the region our patches touch | Messy merge; high chance of semantic bug during resolution | **7** | **9** | **5** | **315** 🔴🔴 | This is the hardest step. Plan: (a) apply stash with `git stash apply --3way`, (b) for each conflict, open BOTH upstream new version AND our stash version side-by-side, (c) re-derive the monitor-patch intent from scratch on top of upstream. (d) commit each resolved file separately for auditability | Abort stash apply (`git checkout --theirs .` then re-derive), worktree is fine |
| F14 | 9 Stash apply | Stash contains edits that are now OBSOLETE (upstream already did it differently, better) | We reapply redundant/wrong code | 5 | 6 | **8** | **240** 🔴 | For each conflicted file, first read upstream's new version and ASK: "is our edit still needed?" Don't blindly reapply. Document the decision inline | Revert file, keep upstream's version |
| F15 | 10 Test run | Unit tests pass but `cass search --mode semantic` still fails because bug isn't actually fixed upstream | Didn't solve the user's problem | **8** | 4 | 3 | **96** 🔴 | Explicit verification: run the exact failing command BEFORE claiming success. Capture stderr line count — target <10 lines, not 484 | Return to Phase 1 of systematic-debugging; might need upstream bug report |
| F16 | 10 Test | Build + tests pass in worktree but installed binary path clash when we copy it to `~/.local/bin/` — daemon still holds file | `cp` fails with "Text file busy" on macOS | 3 | 6 | 1 | 18 | `launchctl stop com.cass.index-watch` BEFORE `cp` | Retry after daemon stop |
| F17 | 11 DB handling | Old `agent_search.db` schema incompatible with new binary | Daemon crashes on startup | 7 | 4 | 2 | 56 | Upstream migration code should handle this. But plan to move the DB aside first: `mv agent_search.db agent_search.db.pre-sync` before starting daemon. If new daemon happily creates a fresh DB, migration wasn't automatic and we can decide whether to force a reindex or write a migration | Restore the `.pre-sync` DB, revert to old binary, stop daemon, investigate |
| F18 | 11 DB handling | Full reindex of ~5 GB of agent session data takes many hours | Long blackout of `cass` search availability | 4 | 5 | 1 | 20 | Run reindex in background; keep old binary available for fallback search during reindex | Fall back to pre-sync backup DB |
| F19 | 12 Cut-over | Daemon starts but `cass search --mode semantic` now fails with a NEW error (regression) | Different bug, not the one we fixed | 6 | 4 | 5 | **120** 🔴 | Capture stderr line count before AND after. Keep the old binary installed as `cass.pre-sync` for fast comparison | `cp cass.pre-sync cass && launchctl kickstart -k gui/$(id -u)/com.cass.index-watch` |
| F20 | 12 Cut-over | Agents (other Claude Code / Codex / Gemini sessions) are mid-search when we swap the binary | Transient search failures in other sessions | 2 | 3 | 2 | 12 | Accept the risk; downtime is <10s | None needed |
| F21 | 13 `main` branch update | Push to `origin/main` includes the dropped 65 servitor commits, history diverges from fork | Fork history confusion | 3 | 2 | 4 | 24 | Use force-with-lease push, OR keep the synced branch separate as `sync/upstream-2026-04-10` until Lee confirms happy-path | `git push --force-with-lease origin sync/upstream-2026-04-10:main` only AFTER Lee approval |
| F22 | ALL | Claude misreads an FMEA mitigation and executes a destructive step without confirmation | Data loss | **10** | 2 | **10** | **200** 🔴🔴 | Every task in the plan that touches git, files, or launchd must be presented to Lee for approval BEFORE execution. This plan is DRAFT until Lee reviews this FMEA | Per-task rollback |

### Red RPN summary (require explicit mitigation)
- **F13 RPN 315** — Stash reapply conflicts in high-risk files — HARDEST STEP, budget real time.
- **F14 RPN 240** — Reapplying obsolete edits — needs per-file judgment.
- **F22 RPN 200** — Claude executes without approval — procedural mitigation only.
- **F9 RPN 125** — Cherry-picks hit API breakages — iterative fix loop budgeted.
- **F2 RPN 120** — Stash fails silently — triple-backup mitigation.
- **F19 RPN 120** — New regression at cut-over — pre-sync binary kept for fast rollback.
- **F6 RPN 100** — Missing sibling deps — pre-flight audit in Task 3.
- **F15 RPN 96** — Sync "fixes" but symptom persists — explicit verification required.

---

## Tasks (bite-sized, execute only after Lee approves)

### Task 0: Halt state-changing work

**Step 1:** Announce intent to Lee and wait for "go" on the plan + FMEA.
**Step 2:** Check for any running servitor / cron processes that touch `.servitor/`:
```bash
ps auxww | grep -iE "servitor|heartbeat|claude.*cass" | grep -v grep
```
Expected: only Claude Code sessions. If a Geordi loop is running, pause it.

**Step 3:** Confirm daemon state:
```bash
launchctl list | grep com.cass
ps auxww | grep "[c]ass" | grep -v grep
```
Expected: `com.cass.index-watch` running.

**Step 4:** No commit. Lee reads and approves the plan + FMEA.

---

### Task 1: Belt-and-suspenders snapshot of current state

**Step 1:** Filesystem-level backup (survives ANY git mistake):
```bash
rsync -a --exclude='target/' --exclude='.worktrees/' \
  /Users/leegonzales/Projects/leegonzales/cass/ \
  /Users/leegonzales/cass-backup-2026-04-10/
```
Expected: `~/cass-backup-2026-04-10/` populated, ~several hundred MB depending on `.git` size.

**Step 2:** Create preservation branch at current HEAD:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass
git branch wip/pre-sync-snapshot-2026-04-10 HEAD
git rev-parse wip/pre-sync-snapshot-2026-04-10
```
Expected: branch created, SHA printed (will match current HEAD of `main`).

**Step 3:** Stash uncommitted working tree (tracked-only — don't stash `.servitor/` untracked files):
```bash
git stash push -m "pre-sync-2026-04-10: monitor patch + misc" -- \
  src/ tests/ .beads/.gitignore
git stash list
```
Expected: stash@{0} listed with our message.

**Step 4:** Verify stash contents match what we just stashed:
```bash
git stash show -p stash@{0} | head -20
git stash show --stat stash@{0}
```
Expected: 16 files, ~377 insertions / ~327 deletions.

**Step 5:** No commit. Stash + backup are the checkpoints.

---

### Task 2: Quiesce the index-watch daemon

**Step 1:** Stop the daemon and wait for process to exit cleanly:
```bash
launchctl stop com.cass.index-watch
for i in 1 2 3 4 5 6 7 8 9 10; do
  if ! pgrep -f "cass index-watch" >/dev/null; then echo "stopped after ${i}s"; break; fi
  sleep 1
done
pgrep -f "cass index-watch" && echo "STILL RUNNING" || echo "clean exit"
```
Expected: "clean exit" within 10s.

**Step 2:** Verify daemon log tail shows graceful shutdown (not mid-merge abort):
```bash
tail -5 ~/Library/Logs/cass-index.log
```
Expected: normal INFO lines, no ERROR spam.

**Step 3:** Preserve current binary for fast rollback:
```bash
cp ~/.local/bin/cass ~/.local/bin/cass.pre-sync-2026-04-10
ls -la ~/.local/bin/cass*
```
Expected: both files present, same size.

**Step 4:** Preserve current DB for fast rollback (hard link — no disk cost):
```bash
cd "/Users/leegonzales/Library/Application Support/com.coding-agent-search.coding-agent-search"
ln agent_search.db agent_search.db.pre-sync-2026-04-10
ln agent_search.db-wal agent_search.db-wal.pre-sync-2026-04-10
ln agent_search.db-shm agent_search.db-shm.pre-sync-2026-04-10
ls -la agent_search.db*
```
Expected: 3 new hard-linked files. `ls -li` should show matching inode numbers.

**Step 5:** No commit.

---

### Task 3: Pre-flight audit — sibling path dep inventory

**Step 1:** Fetch upstream fresh to be sure:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass
git fetch upstream
git log -1 --oneline upstream/main
```
Expected: upstream/main tip SHA printed.

**Step 2:** Extract ALL `path = "../..."` entries from upstream Cargo.toml:
```bash
git show upstream/main:Cargo.toml | grep -nE 'path *= *"\.\./' | sort -u
```
Expected: list of sibling paths referenced. Each must resolve.

**Step 3:** For each sibling path, verify it exists locally:
```bash
git show upstream/main:Cargo.toml | grep -oE '"\.\./[^"]+"' | sort -u | while read p; do
  stripped=$(echo $p | tr -d '"')
  resolved="/Users/leegonzales/Projects/leegonzales/${stripped#../}"
  if [ -e "$resolved" ]; then echo "OK   $stripped → $resolved"
  else echo "MISSING $stripped → $resolved"; fi
done
```
Expected: all OK, OR a list of MISSING that we need to clone in Task 4.

**Step 4:** Verify `frankensqlite` git remote is reachable and the referenced rev exists:
```bash
git show upstream/main:Cargo.toml | grep -A1 frankensqlite | grep rev
# Expected: rev = "ff6a114b"
git ls-remote https://github.com/Dicklesworthstone/frankensqlite ff6a114b 2>&1 | head
```
Expected: rev SHA returned or empty (empty = branch, check via HEAD).

**Step 5:** No commit. This is a pure read-only audit.

---

### Task 4: Clone missing sibling repos

**Step 1:** Based on Task 3 output, clone each MISSING sibling. Example for `frankensqlite`:
```bash
cd /Users/leegonzales/Projects/leegonzales/
git clone https://github.com/Dicklesworthstone/frankensqlite.git
cd frankensqlite
git checkout ff6a114b
# Expected: detached HEAD at ff6a114b
```

**Step 2:** Verify sub-paths referenced by upstream `[patch]` block exist:
```bash
ls /Users/leegonzales/Projects/leegonzales/frankensqlite/crates/fsqlite/Cargo.toml
ls /Users/leegonzales/Projects/leegonzales/frankensqlite/crates/fsqlite-types/Cargo.toml
```
Expected: both files exist.

**Step 3:** Repeat for any other MISSING siblings from Task 3.

**Step 4:** No commit in cass repo.

---

### Task 5: Create isolated worktree for sync work

**Step 1:** Ensure `.worktrees/` is gitignored:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass
grep -q '^\.worktrees/' .gitignore || echo ".worktrees/" >> .gitignore
```
Expected: no-op OR one-line append.

**Step 2:** Create the worktree on a new branch based on `upstream/main`:
```bash
git worktree add -b sync/upstream-2026-04-10 .worktrees/sync upstream/main
cd .worktrees/sync
git log -1 --oneline
git status
```
Expected: worktree created at `.worktrees/sync/`, HEAD at upstream/main tip, clean tree.

**Step 3:** No commit.

---

### Task 6: Cherry-pick the 11 monitor-branch commits

**Step 1:** Identify the exact 11 commits in topological order from oldest to newest:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass/.worktrees/sync
git log --oneline --reverse --topo-order upstream/main..main \
  --grep='monitor\|Merge branch .feat/cass-monitor' > /tmp/monitor-commits.txt
cat /tmp/monitor-commits.txt
```
Expected: ordered list of ~11 SHAs, oldest first. Filter out `chore(servitor)`.

**Step 2:** For each commit (NOT in batch), cherry-pick and verify build:
```bash
while read sha rest; do
  echo "=== $sha $rest ==="
  git cherry-pick "$sha" || {
    echo "CONFLICT in $sha — STOPPING for manual resolution"
    break
  }
  cargo check --offline 2>&1 | tail -20 || {
    echo "cargo check failed after $sha — STOPPING"
    break
  }
done < /tmp/monitor-commits.txt
```
Expected: all 11 cherry-picks succeed + `cargo check` green.

**Step 3:** If ANY cherry-pick conflicts (high likelihood on `src/lib.rs`, `Cargo.toml`, `src/daemon/mod.rs`), STOP and flag to Lee. Resolution options:
  - (a) Resolve conflict in the cherry-pick — must preserve monitor functionality
  - (b) Abort cherry-picks, squash the feat/cass-monitor branch into one commit, rewrite against upstream API, apply as a single new commit
  - Lee decides.

**Step 4:** After all cherry-picks clean:
```bash
git log --oneline upstream/main..HEAD
cargo check 2>&1 | tail -10
```
Expected: 11 commits on top of upstream/main, cargo check clean.

**Step 5:** Already committed via cherry-pick. No separate commit.

---

### Task 7: Copy `.servitor/` state into the worktree

**Step 1:** Rsync `.servitor/` from main checkout to worktree (file-level, not git):
```bash
cd /Users/leegonzales/Projects/leegonzales/cass
rsync -a .servitor/ .worktrees/sync/.servitor/
```
Expected: all servitor files copied.

**Step 2:** Verify worktree `.gitignore` includes `.servitor/` entries as appropriate (or the upstream version does). If `.servitor/` was previously tracked in our branch but isn't in upstream, we need to decide whether to re-track it:
```bash
cd .worktrees/sync
git status --short .servitor/ | head -10
```
Expected: files appear as untracked (since upstream has no `.servitor/`). That's fine — leave untracked, they live outside git history.

**Step 3:** No commit.

---

### Task 8: Build & install test binary

**Step 1:** Clean build in the worktree:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass/.worktrees/sync
cargo build --release 2>&1 | tee /tmp/cass-sync-build.log | tail -40
```
Expected: `Compiling cass ... Finished release`.

**Step 2:** If build fails, this is F11 — log the error, fix iteratively. Common culprits:
  - Monitor code calls rusqlite APIs that no longer exist (migrate to frankensqlite)
  - Tracing subscriber setup conflicts with upstream's new filter (replace our init with upstream's)
  - `src/lib.rs` module exports — may need to re-add `pub mod monitor;`

**Step 3:** Install test binary as `cass.sync` (NOT `cass`) for isolation:
```bash
cp target/release/cass ~/.local/bin/cass.sync
ls -la ~/.local/bin/cass*
```
Expected: three binaries: `cass`, `cass.pre-sync-*`, `cass.sync`.

**Step 4:** No commit.

---

### Task 9: Smoke-test the sync binary BEFORE cutover

**Step 1:** Lexical search — must return results and have low stderr:
```bash
~/.local/bin/cass.sync search "authentication" 2>/tmp/sync-lex.err >/tmp/sync-lex.out
echo "stdout: $(wc -l </tmp/sync-lex.out)  stderr: $(wc -l </tmp/sync-lex.err)"
```
Expected: stdout > 100 lines, stderr <10 lines. (Compare to old binary: 1005 stdout, 1 stderr.)

**Step 2:** Semantic search — the target of this whole exercise:
```bash
~/.local/bin/cass.sync search "authentication" --mode semantic 2>/tmp/sync-sem.err >/tmp/sync-sem.out
echo "rc=$?  stdout: $(wc -l </tmp/sync-sem.out)  stderr: $(wc -l </tmp/sync-sem.err)"
```
Expected: rc=0, stdout > 10 lines (results), stderr < 10 lines (no wall).
**Success criterion:** stderr line count under 20. Current binary: 484.

**Step 3:** Hybrid search:
```bash
~/.local/bin/cass.sync search "authentication" --mode hybrid 2>/tmp/sync-hyb.err >/tmp/sync-hyb.out
echo "rc=$?  stdout: $(wc -l </tmp/sync-hyb.out)  stderr: $(wc -l </tmp/sync-hyb.err)"
```
Expected: same criteria as semantic.

**Step 4:** Stats (currently hangs):
```bash
( ~/.local/bin/cass.sync stats 2>/tmp/sync-stats.err >/tmp/sync-stats.out ) & pid=$!
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
  if ! kill -0 $pid 2>/dev/null; then echo "returned after ${i}s"; break; fi
  sleep 1
done
if kill -0 $pid 2>/dev/null; then
  kill $pid; echo "STILL HANGS — regression, F15 tripped"
else
  wait $pid; echo "rc=$?"
  echo "stdout: $(wc -l </tmp/sync-stats.out)  stderr: $(wc -l </tmp/sync-stats.err)"
fi
```
Expected: returns in <15s with real stats on stdout.

**Step 5:** If ALL of Step 1-4 pass, proceed to Task 10. If any fails, STOP and report to Lee. Do NOT cut over.

**Step 6:** No commit.

---

### Task 10: Apply the stashed working-tree monitor patch

> ⚠ This is the highest-risk task (FMEA F13 RPN 315). Expect real conflicts.

**Step 1:** Apply the stash with 3-way merge:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass/.worktrees/sync
git stash apply --3way "stash@{0}"
git status
```
Expected: some files clean, some with conflict markers.

**Step 2:** For EACH conflicted file, resolve in this order:
  - `src/monitor/**` — no conflicts possible (upstream has no monitor dir). Should apply clean. If not, something is wrong.
  - `src/ui/components/theme.rs` / `src/ui/style_system.rs` — low risk, resolve naturally.
  - `src/daemon/resource.rs`, `src/indexer/redact_secrets.rs`, `tests/frankensqlite_concurrent_stress.rs` — low risk, resolve naturally.
  - `src/indexer/mod.rs`, `src/storage/sqlite.rs`, `src/lib.rs`, `src/ui/app.rs`, `tests/cli_dispatch_coverage.rs` — **HIGH RISK**. Read upstream's version carefully. ASK per conflict: "is our edit still needed, or did upstream solve it differently?" Don't blindly reapply.

**Step 3:** After each file is resolved, run `cargo check` to catch semantic breakage early:
```bash
cargo check 2>&1 | tail -20
```

**Step 4:** Once all conflicts resolved and `cargo check` clean:
```bash
cargo build --release 2>&1 | tail -20
cp target/release/cass ~/.local/bin/cass.sync
```

**Step 5:** Re-run Task 9 smoke tests with the patched binary.
**Success criterion:** all four tests from Task 9 pass AGAIN.

**Step 6:** Commit the resolved patches directly on the sync branch:
```bash
git add src/ tests/ .beads/.gitignore
git commit -m "feat(monitor): reapply monitor patch on top of upstream sync 2026-04-10"
```

---

### Task 11: DB handling decision

**Step 1:** Let the new binary attempt to open the existing DB. DRY RUN in worktree (not cutover yet):
```bash
~/.local/bin/cass.sync search "test" 2>/tmp/sync-dbcheck.err >/tmp/sync-dbcheck.out
echo "stderr of DB open:"
head -20 /tmp/sync-dbcheck.err
```

**Step 2:** Decide based on output:
  - **Clean** — no WAL errors, no parse recovery warnings → proceed to cutover with existing DB.
  - **WAL / recovery warnings persist** → the schema is too old; move DB aside and let new daemon rebuild:
    ```bash
    cd "/Users/leegonzales/Library/Application Support/com.coding-agent-search.coding-agent-search"
    mv agent_search.db agent_search.db.old-2026-04-10
    mv agent_search.db-wal agent_search.db-wal.old-2026-04-10
    mv agent_search.db-shm agent_search.db-shm.old-2026-04-10
    ```
    Tantivy index is kept (`index/`) — only SQLite metadata is rebuilt.

**Step 3:** Ask Lee before moving the DB aside. This is F17.

**Step 4:** No commit.

---

### Task 12: Cutover

**Step 1:** Install the sync binary as the primary:
```bash
cp ~/.local/bin/cass.sync ~/.local/bin/cass
ls -la ~/.local/bin/cass*
```

**Step 2:** Restart the daemon:
```bash
launchctl kickstart -k gui/$(id -u)/com.cass.index-watch
sleep 2
launchctl list | grep com.cass
```
Expected: daemon running with new PID.

**Step 3:** Tail the daemon log for 30 seconds, watch for ERROR:
```bash
tail -f ~/Library/Logs/cass-index.log &
TAIL_PID=$!
sleep 30
kill $TAIL_PID
```
Expected: normal INFO lines, no WAL salt mismatch or parse recovery ERRORs.

**Step 4:** Run the three verification commands that motivated this work:
```bash
cass search "authentication" 2>&1 | wc -l
cass search "authentication" --mode semantic 2>&1 | wc -l
cass stats 2>&1 | head -20
```
Expected: lexical returns >100 results, semantic returns results with minimal stderr, stats completes in <15s.

**Step 5:** No commit.

---

### Task 13: Promote `sync/upstream-2026-04-10` to `main`

**Step 1:** Lee reviews the new `main` candidate. DO NOT push to `origin` without approval.

**Step 2:** Update local `main` to point at the synced branch:
```bash
cd /Users/leegonzales/Projects/leegonzales/cass     # NOTE: main checkout, not worktree
git fetch .worktrees/sync sync/upstream-2026-04-10:sync/upstream-2026-04-10
git checkout main
git reset --hard sync/upstream-2026-04-10
git log --oneline -10
```
Expected: `main` now at the tip of our synced branch.

**Step 3:** Verify daemon still green after main swap:
```bash
cass search "test" 2>&1 | head -5
launchctl list | grep com.cass
```

**Step 4:** Push to `origin` with Lee's approval:
```bash
git push --force-with-lease origin main
```
Note: `--force-with-lease` because we rewrote history. If this fails, someone else pushed in the interim → investigate.

**Step 5:** Cleanup:
```bash
git worktree remove .worktrees/sync
# DO NOT delete these until Lee is satisfied for 24h+:
# - ~/cass-backup-2026-04-10/
# - wip/pre-sync-snapshot-2026-04-10 branch
# - ~/.local/bin/cass.pre-sync-2026-04-10
# - agent_search.db*.pre-sync-2026-04-10
```

**Step 6:** Final commit: none required (already committed during cherry-pick + Task 10).

---

## Success criteria

1. `cass search --mode semantic "anything"` returns results with **<20 lines of stderr** (baseline: 484).
2. `cass search --mode hybrid "anything"` — same.
3. `cass stats` completes in **<15s** (baseline: indefinite hang).
4. `cass-index.log` (daemon) shows **zero** `ERROR WAL frame salt mismatch` over a 10-minute window.
5. The `feat/cass-monitor` functionality is preserved and usable.
6. `main` branch history shows clear upstream-sync point + monitor commits on top.

## Rollback (if anything in Tasks 2-13 goes wrong)

Per-task rollbacks are in the FMEA table. The nuclear rollback:
```bash
launchctl stop com.cass.index-watch
cp ~/.local/bin/cass.pre-sync-2026-04-10 ~/.local/bin/cass
cd "/Users/leegonzales/Library/Application Support/com.coding-agent-search.coding-agent-search"
mv agent_search.db.pre-sync-2026-04-10 agent_search.db
mv agent_search.db-wal.pre-sync-2026-04-10 agent_search.db-wal
mv agent_search.db-shm.pre-sync-2026-04-10 agent_search.db-shm
cd /Users/leegonzales/Projects/leegonzales/cass
git worktree remove --force .worktrees/sync 2>/dev/null
git checkout main
git reset --hard wip/pre-sync-snapshot-2026-04-10
git stash pop  # reapply the stashed monitor-patch working tree
launchctl start com.cass.index-watch
```

If that fails too: restore from `~/cass-backup-2026-04-10/`.

---

## Known follow-ups (NOT in this plan)

- The `frankensqlite` sibling needs a pinning strategy so it doesn't drift out of sync with upstream's pinned rev.
- `.servitor/` needs a canonical policy: tracked in git, or not? Currently hybrid (some tracked, some not) which causes confusion.
- The 68 dropped servitor commits are still in `wip/pre-sync-snapshot-2026-04-10`; decide whether to rewrite them as a single "servitor archive" commit on a separate branch for long-term memory, or drop them forever.
