# `cass monitor` — Team Execution Plan

> **For Claude:** This is the team orchestration plan. Read this file,
> then execute the waves below using TeamCreate + Task tool dispatch.

**Prerequisites:**
- Read `docs/plans/2026-03-11-monitor-design.md` (design decisions)
- Read `docs/plans/2026-03-11-monitor-implementation.md` (full code for each task)
- Branch: `feat/cass-monitor` (already created, 2 commits ahead of main)
- All code for each task is in the implementation plan — agents copy from there

**Critical:** The `ftui` framework is a local path dependency at `../frankentui/`.
If that directory doesn't exist, `cargo build` will fail. Check before spawning
TUI agents. If missing, the TUI task must be deferred until ftui is available.

---

## Team Structure

**Team name:** `cass-monitor`

| Agent Name | Role | Wave | Tasks | Worktree | Files Touched |
|-----------|------|------|-------|----------|---------------|
| **lead** (you) | Orchestrator | 1, 3, 5 | 1, 2, 5, 9 | main branch | `mod.rs`, `lib.rs` |
| **discovery** | Process discovery | 2 | 3 | isolated worktree | `discovery.rs` only |
| **session** | JSONL reader | 2 | 4 | isolated worktree | `session.rs` only |
| **json-output** | JSON streaming | 4 | 6 | isolated worktree | `mod.rs` additions |
| **tui-builder** | Dashboard TUI | 4 | 7, 8 | isolated worktree | `tui.rs` only |

---

## Wave Execution

### Wave 1: Scaffolding (Lead does this directly, no team needed)

**Duration:** ~2 min. Do this before creating the team.

1. Create `src/monitor/state.rs` — copy from implementation plan Task 1
2. Create `src/monitor/mod.rs` — copy from implementation plan Task 2
3. Create stub files: `discovery.rs`, `session.rs`, `tui.rs`
4. Add `pub mod monitor;` to `src/lib.rs` after line 18
5. Run `cargo check` to verify compilation
6. Commit: `feat(monitor): scaffold monitor module with state types`

**After Wave 1:** Push the branch so worktree agents can see scaffolding.

### Wave 2: Parallel — Discovery + Session Reader

**Create team:** `cass-monitor`

**Create tasks in team task list:**

```
Task A: "Implement process discovery (src/monitor/discovery.rs)"
  - Owner: discovery
  - Description: See implementation plan Task 3. Copy code from plan.
    Write tests, make them pass, verify with cargo test.
    Only touch src/monitor/discovery.rs. Do NOT modify mod.rs or lib.rs.
  - Blocked by: none

Task B: "Implement JSONL session reader (src/monitor/session.rs)"
  - Owner: session
  - Description: See implementation plan Task 4. Copy code from plan.
    Write tests, make them pass. May need tempfile dev-dependency.
    Only touch src/monitor/session.rs. Do NOT modify mod.rs or lib.rs.
  - Blocked by: none

Task C: "Wire up CLI integration (src/lib.rs)" [DO NOT ASSIGN YET]
  - Owner: (unassigned — lead does this after wave 2)
  - Blocked by: Task A, Task B

Task D: "Implement JSON output mode" [DO NOT ASSIGN YET]
  - Owner: (unassigned)
  - Blocked by: Task C

Task E: "Build ftui TUI dashboard" [DO NOT ASSIGN YET]
  - Owner: (unassigned)
  - Blocked by: Task C

Task F: "Integration tests" [DO NOT ASSIGN YET]
  - Owner: (unassigned — lead does this)
  - Blocked by: Task D, Task E
```

**Spawn agents (parallel, both in worktrees):**

```
Task tool: name="discovery", isolation="worktree", subagent_type="general-purpose"
Prompt:
  You are implementing process discovery for `cass monitor`.

  1. Read docs/plans/2026-03-11-monitor-implementation.md, Task 3
  2. Copy the code from the plan into src/monitor/discovery.rs
  3. Run `cargo test monitor::discovery` — fix any compilation issues
     (the ftui/franken_agent_detection path deps may need the full
     project to compile; if cargo check fails due to missing path deps,
     focus on writing the code correctly and note what failed)
  4. Commit on your worktree branch

  IMPORTANT: Only touch src/monitor/discovery.rs. Nothing else.
  Return: summary of what you implemented, test results, any issues.

Task tool: name="session", isolation="worktree", subagent_type="general-purpose"
Prompt:
  You are implementing the JSONL session reader for `cass monitor`.

  1. Read docs/plans/2026-03-11-monitor-implementation.md, Task 4
  2. Copy the code from the plan into src/monitor/session.rs
  3. Check if `tempfile` is in Cargo.toml dev-dependencies; add if not
  4. Run `cargo test monitor::session` — fix any compilation issues
  5. Commit on your worktree branch

  IMPORTANT: Only touch src/monitor/session.rs (and Cargo.toml if
  adding tempfile). Nothing else.
  Return: summary of what you implemented, test results, any issues.
```

**Wait for both agents to complete.**

**Lead merge step:**
1. Review both agents' worktree branches
2. Merge discovery branch into feat/cass-monitor
3. Merge session branch into feat/cass-monitor
4. Resolve any Cargo.toml conflicts (likely just tempfile addition)
5. Run `cargo test monitor` — verify all tests pass together
6. Commit merge if needed

### Wave 3: CLI Integration (Lead does this directly)

**Duration:** ~5 min

1. Follow implementation plan Task 5 exactly
2. Add `Commands::Monitor` variant to `src/lib.rs`
3. Add dispatch in `execute_cli`
4. Add `describe_command` and `is_robot_mode` entries
5. Update `src/monitor/mod.rs` with `run_monitor`, `collect_snapshot`
6. Follow implementation plan Task 6 for the `MonitorSnapshot` struct
   and `collect_snapshot` function (they go in mod.rs)
7. Add `dirs` dependency to Cargo.toml if not present
8. Run `cargo build` to verify
9. Test: `cargo run -- monitor --json` (should output JSON)
10. Commit: `feat(monitor): CLI integration and JSON output`

**Why lead does this:** CLI integration touches lib.rs (the 594KB monolith)
and mod.rs (the coordination point). Too risky to parallelize.

### Wave 4: Parallel — JSON Polish + TUI Dashboard

**Spawn agents (parallel, both in worktrees):**

```
Task tool: name="json-output", isolation="worktree", subagent_type="general-purpose"
Prompt:
  You are polishing the JSON output mode for `cass monitor --json`.

  The basic JSON loop is already in src/monitor/mod.rs (collect_snapshot +
  run_json_monitor). Your job:

  1. Read the current src/monitor/mod.rs
  2. Test: `cargo run -- monitor --json` and verify output format
  3. Add tests for MonitorSnapshot serialization (Task 6 in the plan)
  4. Verify the JSON output matches the design doc format
  5. Fix any issues with the JSON output
  6. Commit

  Only touch src/monitor/mod.rs (test additions only, don't restructure).
  Return: JSON output sample, test results.

Task tool: name="tui-builder", isolation="worktree", subagent_type="general-purpose"
Prompt:
  You are building the ftui TUI dashboard for `cass monitor`.

  1. Read docs/plans/2026-03-11-monitor-implementation.md, Tasks 7+8
  2. Read src/ui/app.rs lines 12600-12700 (From<Event> impl) to match
     the exact ftui Event API
  3. Read src/ui/app.rs lines 12879-12950 (Model impl, view method)
     to match the exact ftui rendering API
  4. Implement src/monitor/tui.rs following the plan but adapting to
     the actual ftui API you discover in app.rs
  5. Key things to verify:
     - How Event is structured (Event::Key, KeyCode, Modifiers)
     - How Frame works (frame.buffer vs frame directly)
     - How Rect is constructed
     - How widgets render (render(area, frame) vs render(area, buf))
     - How Cmd works (Cmd::task, Cmd::msg, Cmd::quit, Cmd::batch)
  6. Include the tick timer (Task 8) — use Cmd::task with sleep
  7. Try `cargo build` — note any ftui API mismatches and fix them
  8. Commit

  IMPORTANT: Only touch src/monitor/tui.rs.
  Return: what ftui API patterns you discovered, any deviations from
  the plan, compilation status.
```

**Wait for both agents to complete.**

**Lead merge step:**
1. Review both worktree branches
2. Merge json-output branch
3. Merge tui-builder branch
4. Run `cargo build` — fix any integration issues
5. Run `cargo test monitor`
6. Manual test: `cargo run -- monitor` (TUI should launch)
7. Manual test: `cargo run -- monitor --json` (JSON should stream)

### Wave 5: Integration (Lead does this directly)

1. Follow implementation plan Task 9
2. Add integration tests to `src/monitor/mod.rs`
3. Run full test suite: `cargo test monitor`
4. Manual smoke test with real running Claude instances
5. Final commit: `test(monitor): integration tests`

---

## Shutdown Sequence

After all waves complete:
1. Verify all tests pass: `cargo test monitor`
2. Verify TUI works: `cargo run -- monitor`
3. Verify JSON works: `cargo run -- monitor --json`
4. Send shutdown_request to any remaining teammates
5. TeamDelete to clean up
6. Final commit log review
7. Consider squash or keep as-is

---

## Contingency: ftui Not Available

If `../frankentui/` doesn't exist and `cargo build` fails:

1. Complete waves 1-3 and Task 6 (JSON output) — these don't need ftui
2. Skip Task 7+8 (TUI) — defer to a session where ftui is available
3. `cass monitor --json` will work as a useful MVP
4. The TUI can be added later once ftui path is resolved

Check first:
```bash
ls ../frankentui/crates/ftui/Cargo.toml 2>/dev/null && echo "ftui available" || echo "ftui MISSING — skip TUI"
```

---

## Estimated Timeline

| Wave | Work | Agents | Est. Duration |
|------|------|--------|---------------|
| 1 | Scaffolding | Lead only | 2 min |
| 2 | Discovery + Session | 2 parallel | 5-8 min |
| — | Merge wave 2 | Lead | 2 min |
| 3 | CLI integration | Lead only | 5 min |
| 4 | JSON + TUI | 2 parallel | 5-10 min |
| — | Merge wave 4 | Lead | 3 min |
| 5 | Integration tests | Lead only | 3 min |
| **Total** | | | **~25-35 min** |
