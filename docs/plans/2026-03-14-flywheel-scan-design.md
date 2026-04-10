# Flywheel Scan — Cross-Project Roadmap Discovery

> Date: 2026-03-14
> Goal: Scan all repos, triage, build roadmaps for top 5, produce ranked master work queue

## Team Structure

```
Team Lead (orchestrator)
  ├── scout-biz        → Catalyst + DiffLab business repos
  ├── scout-training   → AI Training ecosystem
  ├── scout-tooling    → Developer tools & infra
  ├── scout-personal   → Personal, content, hobby projects
  └── lee-doppelganger → Strategic reviewer (goals graph + persona schema)
```

## Scout Protocol

Each scout reads per repo:
1. CLAUDE.md / README
2. `docs/plans/*.md` (skip worktree dupes)
3. `bd list --status=open` (beads)
4. `git log --oneline -20`
5. Classify: `active-invest` / `maintain` / `archive` / `merge-into`
6. Extract actionable items or write 1-paragraph status
7. Emit events to `simulation-events.json`

## Lee-Doppelganger Protocol

1. Load silicon-doppelganger-actual skill data
2. Query goals_query.py: status, next, threads, tensions
3. Review all scout outputs
4. Draft 2-3 options per open thread (6 threads)
5. Score proposed beads: goals alignment, energy/impact, unblocking value, tension awareness
6. Produce ranked master work queue

## Event Schema (Replay-Compatible)

Adapted from sand-table replay schema. Events written to:
`output/flywheel-scan-{date}/simulation-events.json`

### Top-Level
```json
{
  "session": { "id": "flywheel-scan", "title": "Cross-Project Roadmap Discovery", "date": "2026-03-14" },
  "agents": [
    { "id": "scout-biz", "name": "Scout: Business", "role": "Domain Scanner", "color": "#3182CE" },
    { "id": "scout-training", "name": "Scout: Training", "role": "Domain Scanner", "color": "#38A169" },
    { "id": "scout-tooling", "name": "Scout: Tooling", "role": "Domain Scanner", "color": "#805AD5" },
    { "id": "scout-personal", "name": "Scout: Personal", "role": "Domain Scanner", "color": "#D69E2E" },
    { "id": "lee-doppelganger", "name": "Lee Doppelganger", "role": "Strategic Reviewer", "color": "#E53E3E" }
  ],
  "events": [ ... ]
}
```

### Event Types

**`scan_start`** — Agent begins scanning a repo
```json
{ "type": "scan_start", "agent": "scout-tooling", "repo": "leegonzales/cass", "time_offset": 0 }
```

**`scan_finding`** — Discovery during scan
```json
{
  "type": "scan_finding", "agent": "scout-tooling", "repo": "leegonzales/cass",
  "finding_type": "plan_doc|beads_issue|code_state|gap|blocker",
  "text": "4 plan docs for cass-monitor, none implemented. FrankenTUI blocker.",
  "severity": "info|warning|critical"
}
```

**`repo_triage`** — Classification decision
```json
{
  "type": "repo_triage", "agent": "scout-tooling", "repo": "leegonzales/cass",
  "classification": "active-invest|maintain|archive|merge-into",
  "rationale": "Core infrastructure, monitor work designed but blocked",
  "goals_alignment": ["Capability", "Tooling"],
  "proposed_beads": [
    { "title": "...", "priority": 1, "energy": "high", "description": "..." }
  ]
}
```

**`roadmap_proposal`** — Full roadmap for top-5 repos
```json
{
  "type": "roadmap_proposal", "agent": "scout-tooling", "repo": "leegonzales/cass",
  "milestones": [
    { "title": "JSON-only MVP", "quarter": "Q1", "effort": "medium" }
  ],
  "text": "Full roadmap narrative..."
}
```

**`thread_proposal`** — Doppelganger resolves an open thread
```json
{
  "type": "thread_proposal", "agent": "lee-doppelganger",
  "thread_id": "thread-differential-mechanics",
  "thread_title": "Differential Revenue Mechanics",
  "options": [
    { "label": "Option A", "description": "...", "trade_offs": "...", "recommendation": true }
  ]
}
```

**`priority_score`** — Doppelganger scores a proposed bead
```json
{
  "type": "priority_score", "agent": "lee-doppelganger",
  "repo": "leegonzales/cass", "bead_title": "Implement cass monitor JSON-only MVP",
  "scores": {
    "goals_alignment": 4, "energy_impact_ratio": 3,
    "unblocking_value": 5, "tension_awareness": 3
  },
  "total": 15, "rank": 1,
  "rationale": "Unblocks visibility into all agent sessions, feeds flywheel"
}
```

**`master_queue`** — Final ranked output
```json
{
  "type": "master_queue", "agent": "lee-doppelganger",
  "queue": [
    { "rank": 1, "repo": "...", "title": "...", "score": 15, "energy": "high" }
  ]
}
```

**`observation`** — Any agent's commentary
```json
{ "type": "observation", "agent": "lee-doppelganger", "text": "..." }
```

## Output Artifacts

```
output/flywheel-scan-2026-03-14/
  ├── simulation-events.json     # Replay-compatible event log
  ├── flywheel-scan-report.md    # Consolidated human-readable report
  ├── triage-table.md            # All repos classified
  ├── master-work-queue.md       # Ranked beads proposals
  └── thread-proposals.md        # Open thread resolution options
```

## Repo Assignments

### scout-biz
- `~/Projects/catalyst/coaching/`
- `~/Projects/catalyst/bizops/`
- `~/Projects/catalyst/website/`
- `~/Projects/catalyst/ai-training-business/`
- `~/Projects/catalyst/clients/fitminded/`
- `~/Projects/Difflab/bizops/`
- `~/Projects/Difflab/ai-fluency-explorer/`

### scout-training
- `~/Projects/leegonzales/AIEnablementTraining/`
- `~/Projects/leegonzales/AI-Rangers/`
- `~/Projects/leegonzales/range-framework/`
- `~/Projects/leegonzales/PresentationKit/`

### scout-tooling
- `~/Projects/leegonzales/cass/`
- `~/Projects/leegonzales/freshell/`
- `~/Projects/leegonzales/kilroy/`
- `~/Projects/leegonzales/ai-talkshow-cli/`
- `~/Projects/leegonzales/agent-orchestra/`
- `~/Projects/leegonzales/dev-environment/`
- `~/Projects/claude-sandboxes/`
- `~/Projects/leegonzales/AISkills/`
- `~/Projects/leegonzales/MCPServers/`
- `~/Projects/leegonzales/claude-guardrails/`
- `~/Projects/leegonzales/claude-allowlist/`
- `~/Projects/claude-speak/`

### scout-personal
- `~/Projects/leegonzales/substack/`
- `~/Projects/leegonzales/SecondBrain/`
- `~/Projects/leegonzales/pf2e-engine/`
- `~/Projects/leegonzales/AIPresentationMaker/`
- `~/Projects/leegonzales/AIVideoTalkShow/`
- `~/Projects/leegonzales/read-aloud/`
- `~/Projects/leegonzales/inevitabilityengine/`
- `~/Projects/leegonzales/ChatGPTArchiver/`
- `~/Projects/leegonzales/ai-judgment-battery/`
- `~/Projects/leegonzales/google-workspace-mcp/`
