# Design: `cass monitor` — Live Agent Cockpit

**Date:** 2026-03-11
**Status:** Approved

## Problem

Running 7+ simultaneous Claude Code instances across `~/Projects/` subfolders.
No unified view of which agents are working, which need input, which are
waiting on permission approval. Cognitive overhead of alt-tabbing to find
the blocked agent scales linearly with instance count.

## Solution

New `cass monitor` subcommand: a read-only Ratatui dashboard that discovers
active Claude Code processes, tails their JSONL session files, derives agent
state, and presents a sortable table with a detail pane. Includes `--json`
mode for streaming JSONL output.

## Decisions

| Aspect | Decision | Rationale |
|--------|----------|-----------|
| Relationship | `cass monitor` subcommand | Reuses existing infra, single binary |
| Actions | Read-only (identify + jump) | User manually switches TTY tabs |
| Discovery | `ps` + `lsof` for PID/TTY/cwd | Combines OS liveness with file-level state |
| Approach | Dedicated TUI + `--json` flag | Purpose-built dashboard with Unix composability |
| Agent scope | Claude Code only (V1) | Simpler discovery; expand to other agents later |
| Refresh | Process table 5s, JSONL tail 2s | Responsive without excessive I/O |

## Architecture

```
Discovery Loop (5s)     State Machine (2s)     Renderer
  ps + lsof       --->   JSONL tail parse  --->  TUI / JSON
  PID, TTY, cwd         Last ~20 lines          Dashboard
  flags, uptime          State derivation        Detail pane
```

### Agent States

| State | Signal | Color |
|-------|--------|-------|
| WORKING | assistant entry, stop_reason: null (streaming) | Green |
| TOOL_RUNNING | progress entry with hook_progress | Cyan |
| WAITING_PERMISSION | tool_use emitted, no tool_result, stale >2s | Yellow (flashing) |
| WAITING_INPUT | complete assistant turn, no subsequent user msg, stale >5s | Red (flashing) |
| QUEUED | queue-operation: enqueue present | Blue |
| IDLE | process alive, no writes >30s | Dim gray |
| STARTING | process running, no JSONL found yet | Gray |

### Discovery Pipeline

1. `ps -eo pid,tty,etime,args` for `claude` processes
2. `lsof -d cwd -p PID -Fn` to get working directory per PID
3. Map cwd to project name (strip `~/Projects/` prefix)
4. Map cwd to JSONL via `~/.claude/projects/` naming convention
   (e.g., `/Users/foo/Projects/bar` -> `-Users-foo-Projects-bar/`)
5. Find most recently modified `.jsonl` (excluding `subagents/`)
6. Extract flags from ps args: `--dangerously-skip-permissions`, etc.

### State Derivation

Read last ~20 lines of JSONL, walk backwards:

1. `type: "assistant"` + `tool_use` in content + no subsequent `tool_result`
   -> WAITING_PERMISSION
2. `type: "assistant"` + `stop_reason: "end_turn"` + no subsequent `user`
   -> WAITING_INPUT
3. `type: "assistant"` + `stop_reason: null`
   -> WORKING (still streaming)
4. `type: "progress"` + `hook_progress`
   -> TOOL_RUNNING
5. Stale timestamp (>30s no new writes) + process alive
   -> IDLE

Context extracted: last user message, last assistant text, model,
git branch, permission mode.

## TUI Layout

Three panels:

1. **Header** — ASCII art "CASS monitor" logo + summary stats
2. **Instance table** — sorted by urgency (WAITING_INPUT first)
   - Columns: #, PROJECT, STATE, TTY, AGE, LAST
3. **Detail pane** — selected instance's context
   - Permission mode, model, git branch
   - Last user message, last assistant message
   - Recent activity log (last ~5 entries)

### Keybindings

| Key | Action |
|-----|--------|
| Up/Down, j/k | Navigate instances |
| Enter | Expand/collapse detail |
| s | Sort by column |
| r | Force refresh |
| q | Quit |
| 1-9 | Jump to instance |

### JSON Output Mode

`cass monitor --json` outputs one JSON object per refresh cycle:

```json
{
  "timestamp": "2026-03-11T22:14:08Z",
  "agents": [
    {
      "project": "leegonzales/cass",
      "state": "WAITING_INPUT",
      "tty": "s006",
      "pid": 32941,
      "age_secs": 1140,
      "last_activity_secs": 45,
      "last_message": "What should I work on next?",
      "model": "claude-opus-4-6",
      "branch": "feat/live-monitor",
      "permission_mode": "default"
    }
  ]
}
```

## New Files

```
src/monitor/
  mod.rs          # Public API, MonitorConfig, CLI args
  discovery.rs    # Process discovery (ps, lsof, PID->cwd mapping)
  session.rs      # JSONL tail reader, context extraction
  state.rs        # AgentState enum, state machine
  tui.rs          # Ratatui dashboard renderer
```

Estimated: 800-1200 lines. No new dependencies (Ratatui, crossterm,
serde, tokio already in cass).

## Edge Cases

1. **No JSONL yet** — new session, show STARTING state
2. **Multiple sessions per project** — pick most recent mtime
3. **Subagent processes** — filter out (share parent TTY)
4. **Permission mode detection** — parse `--dangerously-skip-permissions` from ps args
5. **JSONL compaction** — detect new file, switch
6. **Self-detection** — exclude own PID or label as "(monitor)"

## Future Work (V2+)

- Monitor other agents cass supports (Codex, Cursor, Gemini)
- Auto-focus terminal tab via osascript (iTerm2/Terminal.app)
- Notification integration (macOS notification center)
- Historical session state replay
- Cost tracking per instance (token usage from JSONL)
