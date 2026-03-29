# SOP: Journal Discipline

## Trigger
End of every wake (before sleeping), and immediately when significant events occur mid-session.

## Steps

### 1. Determine Entry Type

| Wake Type | Journal Format |
|-----------|---------------|
| **Quiet wake** (nothing changed) | 1 line: `## Wake #N — Quiet (YYYY-MM-DD HH:MM)` + one-line summary |
| **Active wake** (something happened) | Full entry: lead with what changed, skip unchanged sections |
| **Daily digest** (first wake of business day) | Full structured summary with all sections |

### 2. Write Entry

#### Quiet Wake Template
```
## Wake #N — Quiet (YYYY-MM-DD HH:MM)
No new activity. Concerns unchanged. Next wake: [time].
```

#### Active Wake Structure
2. Lead with what changed — new messages, findings, state changes
3. Skip sections where nothing changed (don't list every quiet channel)
4. Only repeat operational concerns if they changed
5. Action items only if new or status-changed
6. End with next wake time

#### Daily Digest Structure
7. Full structured summary: operational state, active work, concerns, metrics
8. Include any patterns observed across recent wakes
9. Note changes since last daily digest

### 3. Immediate Journaling
10. If Lee tells you something significant during a session: write it NOW
11. If a decision is made: write it NOW
12. If a milestone is reached: write it NOW
13. Do not batch significant events — your session may be killed without warning

### 4. Compression Check
14. If journal.md exceeds 200 lines:
    - Keep the last 7 days verbatim
    - Compress older entries into `.servitor/memory/journal-archive-YYYY-MM.md`
    - Archive format: one paragraph per day summarizing key events, decisions, metrics
    - Replace compressed entries with: `> Wakes #X-#Y archived to memory/journal-archive-YYYY-MM.md`
15. Hard gate: do NOT let journal exceed 300 lines. Compress immediately.

### 5. Update State
16. Update `.servitor/state.json` with current wake number, timestamp, reason
17. Update any metrics that changed (wake counts, PR status, etc.)

## Success Criteria
- Every wake produces a journal entry (no silent wakes)
- Entry type matches wake activity (quiet wakes are 1 line, not 20)
- Journal stays under 200 lines (300 hard max)
- Significant events are journaled immediately, not batched

## Eval
| Metric | Target | Measurement |
|--------|--------|-------------|
| Coverage | 100% of wakes have journal entries | Compare wake count to entry count |
| Conciseness | Quiet wakes avg <3 lines | Measure quiet wake entry length |
| Compression timeliness | Journal never exceeds 300 lines | Check max journal length over time |
| Immediacy | Significant events journaled within same session | Audit event-to-journal latency |

## Escalation
Journal discipline failures are self-correcting — if you notice a gap, fill it on next wake. If compression falls behind, prioritize it on the current wake before other work.
