# SOP: Heartbeat Check

## Trigger
Heartbeat timer fires (default: every 4h for critical repos, 24h for standard).

## Steps

### 1. Load Context
1. Read `.servitor/soul.md` — embody your identity
2. Read `.servitor/journal.md` — recent decisions and context
3. Read `.servitor/state.json` — structured project state
4. Read `.servitor/sops/` — load all SOPs for reference

### 2. Check Agent-Mail
5. Check for pending agent-mail messages (if MCP tools available)
6. Process all pending messages before other work (see `base-mail-processing.md`)

### 3. Repo Health Scan
7. `git log --oneline -20` — recent changes since last heartbeat
8. `gh run list --limit 5` — CI status (if available)
9. `gh pr list` — open PRs, note age in days
10. `bd ready` and `bd list --status=open` — beads issues
11. Check for new lint warnings or build errors (language-appropriate)

### 4. Assess and Act
12. If Lee has uncommitted WIP (`git status` shows modified files): **stand clear** — skip build/lint checks on modified files, note "standing clear" in journal
13. If actionable work found within autonomy boundaries: execute (create branch, fix, open PR)
14. If nothing actionable: proceed to journal entry

### 5. Domain-Specific Checks
15. Run any domain-specific SOPs in `.servitor/sops/` that are triggered by heartbeat

### 6. Close Out
16. Update `.servitor/state.json` with any state changes
17. Append journal entry (follow `base-journal-discipline.md`)
18. If actionable findings: notify Lee via agent-mail with summary
19. If nothing changed: log quiet wake, do not send noise

## Success Criteria
- All 6 phases completed without error
- Journal entry reflects actual findings (not boilerplate)
- State.json updated with current timestamps
- Actionable items either executed or escalated — nothing silently dropped

## Eval
| Metric | Target | Measurement |
|--------|--------|-------------|
| Completion rate | 100% of wakes complete all phases | Count incomplete wakes in journal |
| Signal-to-noise | <20% of wakes generate Lee notifications | Count notification wakes / total wakes |
| Standing clear accuracy | 100% when Lee has WIP | Audit journal for WIP-awareness |
| Action-to-finding ratio | >80% of findings get acted on or escalated | Audit findings vs actions in journal |

## Escalation
- Build failures or CI red: escalate to Lee via agent-mail (Needs Attention)
- Security vulnerabilities discovered: escalate immediately (Urgent)
- Stale PRs >7 days: flag in journal, escalate at >14 days
