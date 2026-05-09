# Robot Mode Guide (cass)

Updated: 2026-05-09

## TL;DR (copy/paste)
- First command: `cass triage --json` (follow `next_command` when present)
- Zero-context fallback: `cass --json`, `cass --robot`, and `cass --robot-format json` also run triage
- Leading flag recovery: `cass --json search "query"` is accepted as `cass search "query" --json`
- First index: `cass index --full --json`
- Search JSON: `cass search "query" --robot`
- Handoff pack: `cass pack "query" --robot --max-tokens 12000 --limit 40`
- Default search: hybrid-preferred. Lexical is required; semantic refinement joins when ready.
- Paginate: use `_meta.next_cursor` → `cass search "query" --robot --cursor <value>`
- Budget tokens: `--max-tokens 200 --robot-meta`
- Minimal fields: `--fields minimal` (path,line,agent)
- Freshness and fallback hints: `--robot-meta` (adds search mode, semantic refinement, lexical fallback reason, index freshness, and warnings)
- View source: `cass view <path> -n <line> --json`
- Health: `cass health --json` or `cass state --json`

## Core commands for agents
| Need | Command |
| --- | --- |
| One-shot preflight | `cass triage --json` |
| Search with JSON | `cass search "panic" --robot` |
| Build cited handoff evidence | `cass pack "panic root cause" --robot --max-tokens 12000 --limit 40` |
| Search today | `cass search "auth" --robot --today` |
| Wildcards | `cass search "http*" --robot` |
| Aggregations | `cass search "error" --robot --aggregate agent,workspace` |
| Pagination | pass `_meta.next_cursor` back via `--cursor` |
| Limit output fields | `--fields minimal` or comma list (`source_path,line_number,agent,title`) |
| Truncate content | `--max-content-length 400` or budgeted `--max-tokens 200` |
| Metadata | `--robot-meta` (elapsed_ms, cache stats, index freshness, cursor, warnings) |
| Health snapshot | `cass state --json` (alias `status`) |
| Capabilities | `cass capabilities --json` (static workflows, mistake recoveries, commands, flags, env vars, exit codes, limits) |
| Introspection | `cass introspect --json` (schemas for responses) |

## Search asset contract
- SQLite is the source of truth for indexed conversations and messages.
- Lexical search is the required fast path. If the lexical derivative is missing, stale, schema-drifted, or corrupt, cass reports that state and should rebuild it from SQLite instead of requiring routine manual repair.
- Hybrid is the default search intent. With `--robot-meta`, `_meta.requested_search_mode`, `_meta.search_mode`, `_meta.semantic_refinement`, `_meta.fallback_tier`, and `_meta.fallback_reason` tell agents what actually happened.
- Semantic search is opportunistic enrichment. Lexical-only behavior is expected during first indexing, semantic backfill, disabled semantic policy, or missing local model/vector assets.
- Treat `next_command` / `recommended_commands[]` from triage as the exact next-command contract. Health/status expose the same readiness recommendations for narrower probes. `recommended_action` is the human-readable summary; do not run repair commands by habit when cass is already rebuilding or when lexical fallback is an expected state.

## Response shapes (robot)
- Search:
  - top-level: `query, limit, offset, count, total_matches, hits, cursor, hits_clamped, request_id`
  - `_meta` (with `--robot-meta`): `elapsed_ms, search_mode, requested_search_mode, mode_defaulted, semantic_refinement, fallback_tier, fallback_reason, wildcard_fallback, cache_stats{hits,misses,shortfall}, tokens_estimated, max_tokens, next_cursor, hits_clamped, state{index, database}, index_freshness`
  - `_warning` present when index is stale (age/pending sessions)
  - `aggregations` present when `--aggregate` is used
- Pack:
  - top-level: `schema_version, query, _meta, limits, realized, health, freshness, pack, evidence, omitted, privacy, warnings`
  - `evidence[]`: redacted excerpt, citation, selection reason/score, token cost, roles, matched terms, and redactions
  - `health`: lexical readiness, semantic state, active rebuild/lock/database flags, source sync gaps, and recommended action
  - `freshness`: policy, window, newest/oldest evidence times, and stale evidence count
  - `privacy`: redaction policy, whether redaction was applied, sensitive-output flag, skill-content flag, and redaction counts
  - `warnings`: machine-readable strings such as `privacy_redactions_applied`, `semantic_fallback_lexical`, or `no_evidence_found`; selected evidence age is structural via `freshness.stale_evidence_count`
- State/Status: `status, healthy, initialized, recommended_action, recommended_commands[{id,command,safety,run_when,success_signal,parse_fields,retry_after_ms}], index{exists,fresh,last_indexed_at,age_seconds,stale}, database{exists,conversations,messages,path}, pending{sessions,watch_active}, rebuild{active,...}, semantic{status,availability,can_search,fallback_mode,hint}, _meta{timestamp,data_dir,db_path}`
- Triage: `surface, schema_version, status, healthy, initialized, recommended_action, recommended_commands[], next_command, readiness{index,database,pending,rebuild,rebuild_progress,semantic}, discovery{capabilities_command,schemas_command,docs_command,api_version_command}, starter_workflows[], mistake_recoveries[], _meta`
- Capabilities: `crate_version, api_version, contract_version, features[], connectors[], workflows[], mistake_recoveries[], limits{max_limit,max_content_length,max_fields,max_agg_buckets}`

## Flags worth knowing
- `--fields minimal|summary|<list>`: reduce payload size
- `--max-content-length N` / `--max-tokens N`: truncate per-field / by budget
- `--robot-format json|jsonl|compact`: choose encoding
- `--request-id ID`: echoed in results/meta; good for correlation
- Time filters: `--today --yesterday --week --days N --since DATE --until DATE`
- Aggregations: `--aggregate agent,workspace,date,match_type`
- Output display (humans): `--display table|lines|markdown`
- Progress: `--progress bars|plain|none|auto`; Color: `--color auto|always|never`
- Pack budgets: `cass pack "query" --robot --max-tokens N --max-evidence N --max-sessions N --max-excerpt-chars N`
- Pack freshness: `--freshness-policy prefer-recent|strict|allow-stale --freshness-window-seconds N`
- Pack input narrowing: `--sessions-from FILE|-`, `--source NAME`, `--agent NAME`, `--workspace DIR`, time filters

## Best practices for agents
- Always pass `--robot`/`--json` and `--robot-meta` when you care about freshness or pagination.
- Start unknown automation with `cass triage --json`; aliases `cass ready --json` and `cass preflight --json` are accepted. If an agent only knows to request structured output, `cass --json`, `cass --robot`, and `cass --robot-format json` default to the same read-only triage response. If the agent puts `--json` or `--robot` before a robot-capable subcommand, cass moves it to that subcommand.
- Use `--fields minimal` during wide scans; fetch details with `cass view` if needed.
- Respect `_warning`, `index_freshness.stale`, and health/status `recommended_action`; run `cass index --full` for first setup or explicit recommended refresh, not as a blind repair loop.
- Treat lexical fallback in default hybrid search as expected when semantic assets are not ready. Escalate only when lexical itself is unavailable after the recommended rebuild path.
- Store `_meta.next_cursor` for long result sets; avoid re-running the base query.
- Include `--request-id` to correlate retries and logs.
- Clamp limits to published caps (see `cass capabilities --json`).
- Prefer `--max-tokens` to keep outputs small in LLM loops.
- Use `cass pack ... --robot` when another agent or human needs a cited handoff. Do not run bare `cass` in automation.
- Read pack `health`, `freshness`, `privacy`, and `warnings` before copying evidence into another tool. Treat redaction and stale-evidence warnings as branchable contract fields.

## Pack handoff workflow

Use `pack` after you know the question you want to hand off. It is extractive
and cited: it selects evidence from the indexed archive, redacts sensitive text,
reports freshness/readiness, and does not call an external summarizer or mutate
source logs.

Copy-paste examples:

```bash
# 1. Pre-flight readiness for freshness-sensitive handoffs
cass triage --json
cass status --json

# 2. Broad exploration
cass search "checkout timeout after redirect" --robot --robot-meta --fields summary --limit 20

# 3. Cited handoff pack
cass pack "checkout timeout after redirect" --robot --max-tokens 12000 --limit 40

# 4. Strict freshness window; empty or stale evidence is an error when required
cass pack "checkout timeout after redirect" --robot \
  --freshness-policy strict --freshness-window-seconds 604800 --require-evidence

# 5. Tight paste budget for another agent
cass pack "checkout timeout after redirect" --robot \
  --max-tokens 4000 --max-evidence 8 --max-sessions 3 --max-excerpt-chars 600

# 6. Privacy-focused summary envelope
cass pack "checkout timeout after redirect" --robot \
  --fields summary,health,freshness,privacy,warnings --max-tokens 4000

# 7. Search first, then restrict pack evidence to those sessions
cass search "checkout timeout" --robot-format sessions \
  | cass pack "checkout timeout root cause" --robot --sessions-from -
```

Pack vs search/export-html/doctor/status:
- Use `search` to discover candidate sessions, paginate, aggregate, or inspect broad result sets.
- Use `pack` to produce a bounded, cited evidence bundle for a handoff prompt or operator note.
- Use `export-html` when you need a complete browsable session artifact; it is not token-budgeted.
- Use `status`/`health` to decide whether freshness and fallback states are trustworthy before handoff.
- Use `doctor` for diagnostics and safe repair workflows; it is not a summarization or handoff command.

Contributor verification for robot-doc changes:

```bash
rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_answer_pack_docs \
  cargo test --test golden_robot_docs
```

## TUI drill-in contract (operator reference)
- `Enter` with a selected hit opens the contextual detail modal on the Messages tab.
- `Enter` with no selected hit follows query-submit behavior (safe no-op when query is empty).
- In detail modal: `/` opens find, `n`/`N` navigate matches, `Esc` exits find before closing the modal.
- Use `F8` to open the selected hit in `$EDITOR` when you need raw file navigation.

## Integration snippets

### Python
```python
import json, subprocess

cmd = ["cass", "search", "error", "--robot", "--robot-meta", "--max-tokens", "200"]
out = subprocess.check_output(cmd, text=True)
data = json.loads(out)
print(data["_meta"]["elapsed_ms"], "ms", "hits:", len(data["hits"]))
```

### Node.js
```js
import { execFileSync } from "node:child_process";

const out = execFileSync("cass", ["search", "timeout", "--robot", "--fields", "minimal"], { encoding: "utf8" });
const result = JSON.parse(out);
console.log(result.hits.map(h => `${h.source_path}:${h.line_number || 0}`).join("\n"));
```

### Bash
```bash
cass search "panic" --robot --fields minimal --robot-meta \
  | jq -r '.hits[] | "\(.source_path):\(.line_number // 0) \(.title // "")"'
```

## Troubleshooting
- “not initialized” → run `cass triage --json` and then its `next_command` (usually `cass index --full ...`)
- Stale warning → read `recommended_action`; wait if rebuild is active, otherwise refresh with `cass index`
- Hybrid returned lexical → check `_meta.fallback_reason`; this is normal when semantic assets are unavailable or backfilling
- Pack warning `privacy_redactions_applied` → inspect `privacy.redaction_counts` before copying the pack; the cited excerpt text has been redacted.
- Nonzero `freshness.stale_evidence_count` → check `health.recommended_action`, rerun with a tighter `--freshness-policy strict`, or refresh/index if status recommends it.
- Pack warning `semantic_fallback_lexical` → evidence is lexical-only; install/backfill semantic assets only if semantic recall is required for this handoff.
- `--require-evidence` with no matches → JSON error envelope with `err.kind="not-found"`; broaden the query or remove the requirement.
- Empty results but expected matches → try `--aggregate agent,workspace` to confirm ingest; check `watch_state.json` pending
- JSON parsing errors → use `--robot-format compact` to avoid pretty whitespace issues

## Change log (robot-facing)
- 2026-04-22: Documented hybrid-default search, lexical self-heal expectations, semantic fail-open metadata, and health/status readiness contract.
- 0.1.30: `_meta.index_freshness` + `_warning` in search robot output; capabilities limits enforced; cursor/request-id exposed.

---
For one-shot readiness plus discovery use `cass triage --json`; for static self-description use `cass capabilities --json`; for deeper response schemas use `cass introspect --json`.
