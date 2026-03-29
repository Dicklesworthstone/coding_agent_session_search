# SOP: Escalation Ladder

## Trigger
Any finding, issue, or decision that requires communication beyond the servitor's own journal.

## Steps

### 1. Classify Severity

| Level | Criteria | Examples |
|-------|----------|----------|
| **FYI** | Routine findings, minor observations, status updates | Clean heartbeat, minor lint warning, package update available |
| **Needs Attention** | Actionable issues, blocked work, decisions needed | Stale PR >7 days, failing CI, beads issue needs input |
| **Urgent** | Production issues, security concerns, time-sensitive blockers | Security vulnerability, data loss risk, deadline approaching |

### 2. Route by Level

#### FYI
2. Send agent-mail to Lee with summary
3. No immediate response expected
4. Log in journal as FYI

#### Needs Attention
5. Send agent-mail to Lee with summary and recommended action
6. Post to your home Mattermost channel (if available)
7. Log in journal as NEEDS_ATTENTION with action items

#### Urgent
8. Send agent-mail to Lee marked importance: urgent
9. Post to #fleet-ops Mattermost channel
10. If other agents can help: send coordination message to relevant agents
11. Log in journal as URGENT with full situation report

### 3. Anti-Patterns (Do NOT)
- Do not escalate routine heartbeat findings as Needs Attention
- Do not escalate the same issue repeatedly without new information
- Do not use Urgent for anything that can wait >4 hours
- Do not send noise — when you say Urgent, it must be urgent

### 4. Follow-Up
12. Track escalated items in state.json under open_directives
13. If no response within 2 heartbeat cycles: re-escalate one level up
14. If resolved: close the directive, log resolution in journal

## Success Criteria
- Severity classification is accurate (validated by Lee's response pattern)
- Urgent items get immediate attention; FYIs don't generate interrupts
- No duplicate escalations for the same issue without new information

## Eval
| Metric | Target | Measurement |
|--------|--------|-------------|
| Classification accuracy | >90% correct level | Audit Lee's responses (did he act on FYIs? Did he miss Urgents?) |
| False urgent rate | <5% of urgents are actually FYI-level | Count urgents that got no action |
| Follow-up rate | 100% of escalations tracked to resolution | Count open_directives without resolution |
| Signal quality | Lee responds to >80% of Needs Attention within 24h | Track response times |

## Escalation
This IS the escalation SOP. If the escalation ladder itself fails (agent-mail down, Mattermost unreachable), log the issue locally and retry on next wake.
