# SOP: Agent-Mail Processing

## Trigger
Pending agent-mail messages detected on wake (any wake type).

## Steps

### 1. Fetch and Triage
1. Fetch all unread messages from inbox
2. Sort by importance: urgent > normal > FYI
3. Group by thread_id where applicable

### 2. Process by Message Type

#### CHECK_IN from Worker
4. Read the worker's task description
5. Compose BRIEFING response with:
   - Current project state (from state.json)
   - Active concerns and gotchas
   - Relevant SOPs the worker should follow
   - Any files or areas to avoid (Lee's WIP, etc.)
6. Send BRIEFING via reply_message

#### REVIEW_REQUEST from Worker
7. Read the diff/PR referenced in the message
8. Review against soul.md standards (correctness, consistency, clarity)
9. Check for security vulnerabilities (OWASP top 10)
10. Verify test coverage for new functionality
11. Send REVIEW_PASS (with notes) or REVIEW_REJECT (with specific feedback)

#### TASK_COMPLETE from Worker
12. Verify the work matches the original task
13. Update journal.md with completion record
14. Update state.json (close relevant items, update stats)
15. Close relevant beads issues (`bd close <id>`)
16. Send acknowledgment

#### DISPATCH_REQUEST
17. Evaluate whether request is within autonomy boundaries
18. If within bounds: spawn the work, acknowledge receipt
19. If outside bounds: forward to Lee with your assessment
20. If ambiguous: ask Lee before acting

#### STATUS_QUERY (fleet broadcast)
21. Compile current state summary from state.json
22. Include: active work, blockers, recent changes, health metrics
23. Reply with structured status report

### 3. Mark Processed
24. Mark each message as read via mark_message_read
25. Acknowledge actionable messages via acknowledge_message

## Success Criteria
- All messages processed in priority order
- Every message gets a response or acknowledgment
- No messages silently dropped
- REVIEW_REJECT includes specific, actionable feedback (not vague)

## Eval
| Metric | Target | Measurement |
|--------|--------|-------------|
| Response rate | 100% of messages get response | Count unresponded messages |
| Response time | <1 heartbeat cycle | Measure time between message arrival and response |
| Review quality | REVIEW_REJECT includes specific line references | Audit reject messages |
| Escalation accuracy | Dispatches outside bounds forwarded, not attempted | Audit dispatch handling |

## Escalation
- Malformed or unparseable messages: log and skip, don't crash
- Messages from unknown senders: process normally, note in journal
- Flood of messages (>20 in one wake): process first 20, note overflow, schedule follow-up
