# Robot Mode Guide (cass)

Updated: 2026-04-22

## TL;DR (copy/paste)
- First index: `cass index --full`
- Search JSON: `cass search "query" --robot`
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
| Search with JSON | `cass search "panic" --robot` |
| Search today | `cass search "auth" --robot --today` |
| Wildcards | `cass search "http*" --robot` |
| Aggregations | `cass search "error" --robot --aggregate agent,workspace` |
| Pagination | pass `_meta.next_cursor` back via `--cursor` |
| Limit output fields | `--fields minimal` or comma list (`source_path,line_number,agent,title`) |
| Truncate content | `--max-content-length 400` or budgeted `--max-tokens 200` |
| Metadata | `--robot-meta` (elapsed_ms, cache stats, index freshness, cursor, warnings) |
| Health snapshot | `cass state --json` (alias `status`) |
| Capabilities | `cass capabilities --json` |
| Introspection | `cass introspect --json` (schemas for responses) |

## Search asset contract
- SQLite is the source of truth for indexed conversations and messages.
- Lexical search is the required fast path. If the lexical derivative is missing, stale, schema-drifted, or corrupt, cass reports that state and should rebuild it from SQLite instead of requiring routine manual repair.
- Hybrid is the default search intent. With `--robot-meta`, `_meta.requested_search_mode`, `_meta.search_mode`, `_meta.semantic_refinement`, `_meta.fallback_tier`, and `_meta.fallback_reason` tell agents what actually happened.
- Semantic search is opportunistic enrichment. Lexical-only behavior is expected during first indexing, semantic backfill, disabled semantic policy, or missing local model/vector assets.
- Treat `recommended_action` from health/status as authoritative. Do not run repair commands by habit when cass is already rebuilding or when lexical fallback is an expected state.

## Response shapes (robot)
- Search:
  - top-level: `query, limit, offset, count, total_matches, hits, cursor, hits_clamped, request_id`
  - `_meta` (with `--robot-meta`): `elapsed_ms, search_mode, requested_search_mode, mode_defaulted, semantic_refinement, fallback_tier, fallback_reason, wildcard_fallback, cache_stats{hits,misses,shortfall}, tokens_estimated, max_tokens, next_cursor, hits_clamped, state{index, database}, index_freshness`
  - `_warning` present when index is stale (age/pending sessions)
  - `aggregations` present when `--aggregate` is used
- State/Status: `status, healthy, initialized, recommended_action, index{exists,fresh,last_indexed_at,age_seconds,stale}, database{exists,conversations,messages,path}, pending{sessions,watch_active}, rebuild{active,...}, semantic{status,availability,can_search,fallback_mode,hint}, _meta{timestamp,data_dir,db_path}`
- Capabilities: `crate_version, api_version, contract_version, documentation_url, features[], connectors[], limits{max_limit,max_content_length,max_fields,max_agg_buckets}`

## Flags worth knowing
- `--fields minimal|summary|<list>`: reduce payload size
- `--max-content-length N` / `--max-tokens N`: truncate per-field / by budget
- `--robot-format json|jsonl|compact`: choose encoding
- `--request-id ID`: echoed in results/meta; good for correlation
- Time filters: `--today --yesterday --week --days N --since DATE --until DATE`
- Aggregations: `--aggregate agent,workspace,date,match_type`
- Output display (humans): `--display table|lines|markdown`
- Progress: `--progress bars|plain|none|auto`; Color: `--color auto|always|never`

## Best practices for agents
- Always pass `--robot`/`--json` and `--robot-meta` when you care about freshness or pagination.
- Use `--fields minimal` during wide scans; fetch details with `cass view` if needed.
- Respect `_warning`, `index_freshness.stale`, and health/status `recommended_action`; run `cass index --full` for first setup or explicit recommended refresh, not as a blind repair loop.
- Treat lexical fallback in default hybrid search as expected when semantic assets are not ready. Escalate only when lexical itself is unavailable after the recommended rebuild path.
- Store `_meta.next_cursor` for long result sets; avoid re-running the base query.
- Include `--request-id` to correlate retries and logs.
- Clamp limits to published caps (see `cass capabilities --json`).
- Prefer `--max-tokens` to keep outputs small in LLM loops.

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
- “not initialized” → run `cass index --full` once
- Stale warning → read `recommended_action`; wait if rebuild is active, otherwise refresh with `cass index`
- Hybrid returned lexical → check `_meta.fallback_reason`; this is normal when semantic assets are unavailable or backfilling
- Empty results but expected matches → try `--aggregate agent,workspace` to confirm ingest; check `watch_state.json` pending
- JSON parsing errors → use `--robot-format compact` to avoid pretty whitespace issues

## Change log (robot-facing)
- 2026-04-22: Documented hybrid-default search, lexical self-heal expectations, semantic fail-open metadata, and health/status readiness contract.
- 0.1.30: `_meta.index_freshness` + `_warning` in search robot output; capabilities limits enforced; cursor/request-id exposed.

---
For deeper schemas: `cass introspect --json` and `cass capabilities --json`.
