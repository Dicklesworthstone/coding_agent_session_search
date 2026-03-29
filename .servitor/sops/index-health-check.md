# SOP: Index Health Check

## Trigger
Every heartbeat wake (24h interval). Geordi's primary domain procedure.

## Steps

### 1. Index Integrity
1. Check database file exists and is readable: `ls -la` the SQLite DB
2. Run a sample search to verify index responds: `cass search "test" --limit 1`
3. Check session count: `cass search "" --limit 0` (or equivalent stats command)
4. Compare session count to previous wake — flag significant drops

### 2. Indexer Health
5. Check launchd watcher is running: `launchctl list | grep cass`
6. Check for indexing errors in recent logs
7. Verify new sessions are being indexed (compare latest indexed timestamp to current time)
8. If indexing gap >24h: flag as degraded

### 3. Search Quality Spot-Check
9. Run one known-good query and verify relevance of top result
10. Run one semantic search and verify embedding quality
11. If results seem degraded: note in journal, recommend investigation

### 4. Code Quality
12. `cargo fmt --check` — verify formatting
13. `cargo clippy --all-targets` — zero warnings
14. Check for outdated dependencies: `cargo outdated` (if available)

### 5. Report Status
15. Compose health summary: green/yellow/red
16. Include: session count, last indexed timestamp, search quality, lint status
17. Log in journal; only notify Lee if yellow or red

## Success Criteria
- Index always available and responding to queries
- New sessions indexed within 24h of creation
- Search quality maintained (no silent degradation)
- Zero clippy warnings

## Eval
| Metric | Target | Measurement |
|--------|--------|-------------|
| Availability | 100% of wakes find index responsive | Count index-down events |
| Indexing freshness | Gap <24h between newest session and index | Measure indexing latency |
| Search quality | Known-good queries return expected top result | Track spot-check results |
| Code health | 0 clippy warnings sustained | Count warnings per wake |

## Escalation
- Index unresponsive: Urgent — Lee loses cross-session recall across all projects
- Indexing gap >48h: Needs Attention — launchd watcher may need restart
- Search quality degraded: Needs Attention — possible embedding drift or schema issue
- Clippy warnings introduced: FYI — create issue, fix on next opportunity
