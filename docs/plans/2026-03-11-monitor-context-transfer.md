═══════════════════════════════════════════════════════════════════
DEV CONTEXT TRANSFER
═══════════════════════════════════════════════════════════════════
Generated: 2026-03-11T22:30:00Z | Session: brainstorming + planning

**MISSION**: Build `cass monitor` — a live dashboard subcommand for the
cass CLI that monitors active Claude Code instances across ~/Projects/,
showing which agents are working, waiting for input, or need permission.

**STATUS**: ⧗ in-progress (planning complete, implementation not started)

**PROGRESS**: Full brainstorming → design → implementation plan → team
execution plan. Three docs committed. Zero implementation code written yet.

───────────────────────────────────────────────────────────────────
§ CODE CONTEXT
───────────────────────────────────────────────────────────────────

**Repo**: `/Users/leegonzales/Projects/leegonzales/cass/`
**Package**: `coding-agent-search` v0.1.64

**No code files changed yet.** All work is in docs/plans/:
- `docs/plans/2026-03-11-monitor-design.md` — approved design doc
- `docs/plans/2026-03-11-monitor-implementation.md` — 9-task TDD plan with full code
- `docs/plans/2026-03-11-monitor-team-execution.md` — 5-wave team orchestration plan

**Files to create** (all code is pre-written in the implementation plan):
- `src/monitor/mod.rs` — module root, MonitorSnapshot, collect_snapshot, run_monitor
- `src/monitor/state.rs` — AgentState enum, PermissionMode, AgentInstance, SessionContext
- `src/monitor/discovery.rs` — ps/lsof process discovery, path mapping
- `src/monitor/session.rs` — JSONL tail reader, state derivation
- `src/monitor/tui.rs` — ftui dashboard (MonitorApp, Msg, view)

**Files to modify**:
- `src/lib.rs:18` — add `pub mod monitor;`
- `src/lib.rs:731` — add `Commands::Monitor` variant
- `src/lib.rs:2547` — add to tracing match arm
- `src/lib.rs:~3261` — add dispatch arm
- `src/lib.rs:4308` — add to `describe_command`
- `src/lib.rs:4342` — add to `is_robot_mode`

───────────────────────────────────────────────────────────────────
§ GIT STATE
───────────────────────────────────────────────────────────────────

**Branch**: `feat/cass-monitor`
**Base**: `main`
**Commits**: 3 ahead of main

**Recent Commits**:
- `18e5b6a0` docs: add team execution plan for cass monitor build
- `48dc214c` docs: add implementation plan for cass monitor (9 tasks, TDD)
- `5f852b4d` docs: add design doc for cass monitor live agent cockpit

**Staged**: None
**Unstaged**: None
**Untracked**: None

**Merge Status**: Clean, no conflicts

───────────────────────────────────────────────────────────────────
§ ENVIRONMENT STATE
───────────────────────────────────────────────────────────────────

**Critical Dependency Issue**:
- `ftui` (FrankenTUI) is a **local path dependency** at `../frankentui/`
- **IT IS NOT CHECKED OUT** on this machine
- `cargo build` WILL FAIL until frankentui is cloned/available
- This blocks Task 7+8 (TUI dashboard) but NOT tasks 1-6

**Other local path deps** (status unknown, may also be missing):
- `../franken_agent_detection` — connector crate (JSONL parsing)
- `../frankensearch` — search crate
- `../asupersync` — async sync crate
- `../toon_rust` — output formatting

**To check all deps**: `cargo check 2>&1 | head -20`

**If deps missing**, the contingency plan says:
- Waves 1-3 + Task 6 (JSON output) can work if at least the core
  deps compile. Process discovery uses only std::process::Command.
- TUI (Task 7+8) is deferred until ftui is available
- `cass monitor --json` is a useful MVP without the TUI

**Possible resolution**: Check if these repos exist elsewhere on the
machine or in Lee's GitHub. They may need `git clone` into sibling dirs.

**Running Claude instances** (observed during planning):
- 7+ active processes on TTYs s000, s003, s005, s006, s009, s012, s014
- Mix of default, --dangerously-skip-permissions, --allow-dangerously-skip-permissions

───────────────────────────────────────────────────────────────────
§ TECHNICAL DECISIONS
───────────────────────────────────────────────────────────────────

| Decision | Rationale | Alternatives Rejected |
|----------|-----------|----------------------|
| `cass monitor` subcommand | Reuses infra, single binary | Standalone tool, separate crate |
| Read-only radar | User switches tabs manually | Auto-focus terminal, full interaction |
| ps + lsof discovery | OS-level liveness + file state | File watch only, Claude IPC |
| ftui TUI + --json | Purpose-built dashboard + Unix pipe | Tab in existing TUI |
| Claude-only V1 | Simpler discovery | All agents cass supports |
| 2s JSONL refresh, 5s ps | Responsive without excess I/O | Faster/slower intervals |

**Architecture**: 3 components (Discovery → State Machine → Renderer)
with 7 agent states: WORKING, TOOL_RUNNING, WAITING_PERMISSION,
WAITING_INPUT, QUEUED, IDLE, STARTING

**cass internals learned**:
- `src/lib.rs` is 594KB monolith — Commands enum at line 99-732,
  execute_cli at line 2359-3416
- Old ratatui TUI is gone — `ui/tui.rs` is a 5-line stub
- Real TUI uses ftui Elm architecture (Model/Message/Cmd) in `ui/app.rs`
- Connectors live in external `franken_agent_detection` crate
- No existing process detection — entirely new functionality
- Edition 2024 (needs recent stable or nightly Rust)

───────────────────────────────────────────────────────────────────
§ OPEN LOOPS
───────────────────────────────────────────────────────────────────

**Next Actions (in order)**:
- [ ] Check/resolve local path dependencies (cargo check)
- [ ] If deps OK: Execute team plan starting at Wave 1
- [ ] If deps missing: Clone sibling repos or adjust Cargo.toml
- [ ] Wave 1: Create state.rs + mod.rs + stubs (lead, ~2 min)
- [ ] Wave 2: Dispatch discovery + session agents in parallel
- [ ] Wave 3: CLI integration (lead)
- [ ] Wave 4: Dispatch json-output + tui-builder in parallel
- [ ] Wave 5: Integration tests (lead)

**Blockers**:
- **ftui not checked out**: Blocks TUI (Task 7+8). JSON-only MVP is fallback.
- **Other path deps**: May block compilation entirely. Must verify first.

**Questions to Resolve**:
- [ ] Where are the sibling repos (frankentui, franken_agent_detection, etc.)?
- [ ] Should we add `dirs` crate to Cargo.toml (for home_dir)?
- [ ] Should we add `tempfile` to dev-dependencies (for session tests)?

───────────────────────────────────────────────────────────────────
§ TESTING & VALIDATION
───────────────────────────────────────────────────────────────────

**Test Status**: No implementation tests yet (all code is in the plan doc)

**Planned Tests** (from implementation plan):
- state.rs: 4 tests (display, priority, needs_attention, permission_mode)
- discovery.rs: 8 tests (ps parsing, etime parsing, path mapping, lsof)
- session.rs: 7 tests (state derivation for each state, context extraction, tail_lines)
- mod.rs: 3 tests (snapshot serialization, collect runs, sort order)

**Manual Testing Plan**:
- `cargo run -- monitor --json` — verify JSON output with 7+ agents
- `cargo run -- monitor` — verify TUI dashboard renders
- Kill a claude process, verify it disappears from monitor

───────────────────────────────────────────────────────────────────
§ CONTEXT NOTES
───────────────────────────────────────────────────────────────────

**Key Insights**:
- JSONL session files are at `~/.claude/projects/{path-with-dashes}/{uuid}.jsonl`
- Path convention: `/Users/lee/Projects/foo` → `-Users-lee-Projects-foo`
- Session state is derivable from last ~20 JSONL lines by walking backwards
- Key JSONL types: "user", "assistant", "progress", "queue-operation"
- `stop_reason: null` = still streaming, `"end_turn"` = complete
- `tool_use` in content with no subsequent `tool_result` = waiting permission
- `cass` has 2,263 indexed conversations, 71K messages

**Developer Notes**:
- Lee runs 7+ Claude instances simultaneously — this is his daily workflow
- He knows Ratatui well from pf2e-tui (1,765 lines)
- "Compound, Don't Consume" — this tool should strengthen the cass ecosystem
- Lee's preferred name, timezone America/Denver, uses zsh

**References**:
- Design doc: `docs/plans/2026-03-11-monitor-design.md`
- Implementation plan: `docs/plans/2026-03-11-monitor-implementation.md`
- Team execution plan: `docs/plans/2026-03-11-monitor-team-execution.md`
- cass repo: `/Users/leegonzales/Projects/leegonzales/cass/`

═══════════════════════════════════════════════════════════════════
§ TRANSFER READY
═══════════════════════════════════════════════════════════════════
Review for accuracy before sharing. Check git state and file paths.

**To resume**: `cd ~/Projects/leegonzales/cass && git checkout feat/cass-monitor`
Then read `docs/plans/2026-03-11-monitor-team-execution.md` and execute.
