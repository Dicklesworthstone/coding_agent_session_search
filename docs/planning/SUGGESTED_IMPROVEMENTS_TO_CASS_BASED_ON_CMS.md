# Suggested Improvements to `cass` (based on cass-memory-system / CMS)

Last updated: 2025-12-15

This document proposes a **cass-side** implementation plan for features that CMS needs (and that are generally useful for cass users), with a special focus on:

- **Remote session/log ingestion** via SSH (using existing SSH configs/keys).
- **Strong provenance/origin metadata** stored in first-class fields (not just “it’s in the path”).
- **Filtering** by origin/source/host in both CLI + TUI + robot outputs.
- **Visual distinction** for remote-origin records (e.g., “same agent color, darker shade”).

The intent is to push the “remote logs + provenance + filtering + UI distinction” capability **down into cass** where it naturally belongs, so CMS can remain simpler and consume cass as a canonical “agent history search layer”.

Note: this also aligns with `README.md` “Roadmap & Future Directions” → “Collaborative Features: Optional encrypted sync between machines”. The plan below focuses on a pragmatic “SSH mirror + provenance” first step (encrypted in transit); if we later want stronger guarantees, we can add optional at-rest encryption for remote caches and/or export/import bundles.

---

## 1) Current cass architecture (relevant to this plan)

Key modules (as of today):

- **Connectors** (discover + parse native agent logs): `src/connectors/*.rs`, traits/types in `src/connectors/mod.rs`.
  - Connectors output `NormalizedConversation` + `NormalizedMessage` (+ snippets).
- **Indexer** (orchestrates scans, persists, builds search index): `src/indexer/mod.rs`.
  - Writes normalized data to SQLite via `src/storage/sqlite.rs`.
  - Writes searchable docs to Tantivy via `src/search/tantivy.rs`.
- **Search** (query parsing + filtering + ranking + caching): `src/search/query.rs` (primary), Tantivy schema in `src/search/tantivy.rs`.
  - Result type used everywhere: `SearchHit` in `src/search/query.rs`.
- **TUI** (render panes, filters, detail view): `src/ui/tui.rs` and `src/ui/components/theme.rs`.
  - Filter state: `SearchFilters` in `src/search/query.rs`.
  - Detail loading: `src/ui/data.rs` reads from SQLite (`SqliteStorage`) by `source_path`.

Important existing constraints/assumptions that matter for remote ingestion:

- Tantivy schema fields are currently: `agent`, `workspace`, `source_path`, `msg_idx`, `created_at`, `title`, `content`, `preview`, etc. (`src/search/tantivy.rs`).
- SQLite `conversations` uniqueness is currently `UNIQUE(agent_id, external_id)` (`src/storage/sqlite.rs`).
- Indexer currently passes `ScanContext { data_root: data_dir, since_ts }` to all connectors (`src/indexer/mod.rs`).
- Search filters currently include only: agents, workspaces, created_from/to (`SearchFilters` in `src/search/query.rs`).
- TUI pane theme is derived from agent slug (`ThemePalette::agent_pane` in `src/ui/components/theme.rs`) and used for result list rendering (`src/ui/tui.rs`).

---

## 2) The CMS-driven feature request (what “good” looks like)

From the CMS planning work, plus the explicit request here:

1) **Pull logs from other machines**
   - Use SSH configs/keys (ideally: reuse `~/.ssh/config` host aliases).
   - Pull remote agent logs into the local cass data dir (or a sibling cache root).

2) **Remote logs must be “separate”**
   - Stored with provenance fields so results can be filtered by:
     - local vs remote (origin kind)
     - source id (user-friendly stable id)
     - host / machine identifier (for display and auditing)
   - Not just a path prefix hack; provenance should be queryable.

3) **Remote logs must look visually distinct**
   - Same base agent color identity, but a darker / dimmer variant for remote-origin entries.
   - Also add an explicit badge (e.g., `[work-laptop]`) so distinction is obvious even in monochrome terminals.

4) **Robot outputs must include provenance**
   - CMS and other automation should be able to parse origin/source metadata reliably.

---

## 3) Biggest correctness blockers to remote ingestion (fix first)

These are “make remote possible without breaking correctness” prerequisites.

### 3.1 Conversation identity collisions across sources

SQLite uniqueness currently depends on `(agent_id, external_id)` (`src/storage/sqlite.rs`).

Once we ingest multiple machines, **external_id WILL collide** across sources (especially for connectors that use simple ids or filenames, or for agents that generate ids with low entropy).

Example risk:
- `aider` currently sets `external_id` to the filename `.aider.chat.history.md` (`src/connectors/aider.rs`), which is trivially collision-prone even on a single machine.

Plan requirement:
- Conversation identity must incorporate **provenance** so “same agent + same external id” on different sources becomes “distinct conversations”.

### 3.2 Indexer “scan roots” vs “data dir” confusion (impacts remote + local)

Indexer sets `ScanContext.data_root = opts.data_dir` (`src/indexer/mod.rs`), but several connectors interpret `data_root` as a scan root (e.g., Aider scans under `data_root`).

Remote ingestion increases the need for:
- multiple scan roots (local default roots + remote mirror roots)
- per-root provenance

So we should formalize:
- `data_dir` (cass internal state) vs
- `scan_roots` (where logs are read from)

This is the right place to solve remote ingestion cleanly.

---

## 4) Proposal: first-class “Sources / Origins” in cass

### 4.1 Data model

Add a conceptual model:

- **Source**: “where this session came from”
  - `source_id` (stable, user-friendly): e.g. `local`, `work-laptop`, `home-server`
  - `kind`: `local` | `ssh` (later: `s3`, `git`, etc)
  - `host_label`: string to show in UI (often SSH alias or hostname)
  - optional: `machine_id` (stable id; hashed if desired)
  - optional: workspace path rewrite rules (see 4.4)

- **Origin** (per conversation/message doc):
  - `origin_source_id` (required)
  - `origin_kind` (required)
  - `origin_host` (optional, for display)

### 4.2 Storage representation (SQLite)

Preferred “proper” approach:

1) Add `sources` table (new)
   - `id TEXT PRIMARY KEY` (use `source_id`)
   - `kind TEXT NOT NULL`
   - `host_label TEXT`
   - `machine_id TEXT`
   - `created_at INTEGER`, `updated_at INTEGER`
   - `config_json TEXT` (for ssh params, path rewrite rules, etc)

2) Add provenance columns on `conversations` (new columns)
   - `source_id TEXT NOT NULL REFERENCES sources(id)` (default `local`)
   - `origin_host TEXT` (nullable, display)

3) Update uniqueness
   - New uniqueness should include `source_id`:
     - `UNIQUE(source_id, agent_id, external_id)`

This requires a migration that rewrites the conversations table (SQLite can’t easily alter unique constraints in place). This migration should be done via:
- create new table + copy + rewire foreign keys + drop/rename old table.

### 4.3 Search representation (Tantivy)

Add new Tantivy fields in `src/search/tantivy.rs`:
- `source_id` (STRING | STORED)  — filterable
- `origin_kind` (STRING | STORED) — filterable (if not derivable from source)
- optional `origin_host` (STRING | STORED) — filterable/display

Indexing:
- when `TantivyIndex::add_messages` builds each doc, add these fields per message doc.

Querying:
- extend `SearchFilters` (in `src/search/query.rs`) with:
  - `sources: HashSet<String>` (source_id)
  - optionally `origin_kinds: HashSet<String>` (or just source-based)
  - optionally `hosts: HashSet<String>`
- update `search_tantivy` and `search_sqlite` to apply these filters as `TermQuery` (tantivy) / `IN (...)` (sqlite backend).

### 4.4 Workspace path rewriting (high leverage for multi-machine)

Problem: remote sessions carry remote absolute workspace paths; filters and grouping become painful.

Add optional per-source rewrite rules:
- e.g. `/home/jemanuel/projects` → `/Users/jemanuel/projects`

Implementation idea:
- store rewrite rules in `sources.config_json`
- during normalization (ingest-time), rewrite `NormalizedConversation.workspace` for that source.

This makes:
- workspace filters stable across machines
- TUI display consistent
- potential future feature: “open workspace locally” more reliable

---

## 5) Proposal: Remote source ingestion via SSH (mirror into data dir)

### 5.1 Principle: mirror raw logs locally (so `cass view` keeps working)

Because `cass view` currently reads files by `source_path` (`run_view` in `src/lib.rs`), the simplest UX is:

- Remote logs are synced to local disk (under cass data dir).
- Indexing uses those local mirror files, so `source_path` exists locally.

### 5.2 Local mirror layout

Under the cass data dir (see `default_data_dir()` in `src/lib.rs` and `index_dir()` in `src/search/tantivy.rs`):

`<data_dir>/remotes/<source_id>/mirror/...`

Within `mirror/`, preserve a structure that makes it easy to:
- understand provenance
- scan multiple agent roots
- avoid collisions between different remote machines

Example (macOS remote):

- `<data_dir>/remotes/work-laptop/mirror/home/.codex/sessions/...`
- `<data_dir>/remotes/work-laptop/mirror/home/.claude/projects/...`
- `<data_dir>/remotes/work-laptop/mirror/home/.gemini/tmp/...`
- `<data_dir>/remotes/work-laptop/mirror/home/.pi/agent/sessions/...`
- `<data_dir>/remotes/work-laptop/mirror/Library/Application Support/Cursor/User/...` (mac)
- `<data_dir>/remotes/work-laptop/mirror/Library/Application Support/com.openai.chat/...` (mac)

For linux remotes, mirror linux paths (home-based; no Library/ support).

### 5.3 Sync engine

Add a new CLI family, for example:

- `cass sources add ssh <source_id> --host <ssh-alias> --platform macos|linux`
- `cass sources sync [--source <id>] [--all]`
- `cass sources list --json`
- `cass sources doctor --json` (validate ssh connectivity + paths)

Implementation options for transport:

Option A (pragmatic, leverages SSH config best): shell out to `rsync` over `ssh`
- Pros: fastest, incremental, uses existing SSH config/keys/jumps, handles mtimes well.
- Cons: external dependency; Windows may not have rsync.

Option B (portable, pure Rust): SFTP via `ssh2` / `russh`
- Pros: no external command dependency.
- Cons: more work; performance less great; parsing SSH config harder.

Recommended plan:
- Prefer rsync when available; fall back to SFTP when not.
- Never use rsync `--delete` by default (avoid accidental deletion semantics).
- Persist sync status and “last successful sync time” per source.

### 5.4 Mapping “remote default paths” per platform

Remote sessions live in different default locations depending on OS. We need a way to decide which remote paths to sync.

Recommended approach:
- Each `Source` has `platform` and an explicit list of remote paths to sync.
- Provide sensible presets:
  - `cass sources add ssh work-laptop --host work-laptop --preset macos-defaults`
  - `cass sources add ssh home-server --host home-server --preset linux-defaults`

Where presets include the known agent roots documented in `README.md` (Codex, Claude Code, etc).

---

## 6) Indexing pipeline changes required

### 6.1 Make “scan roots” explicit in the indexer

Today, the indexer passes `ScanContext { data_root: data_dir, ... }` to all connectors (`src/indexer/mod.rs`).

For remote ingestion we should:
- construct a list of scan roots:
  - local roots (existing `watch_roots()` in `src/indexer/mod.rs` is a good starting point)
  - plus remote mirror roots per source
- pass (root, source_id, origin_kind, host_label, rewrite rules) to the connector scan pass

Two viable implementation strategies:

1) Minimal refactor: “scan per root”
   - For each connector, for each scan root relevant to that connector, call `connector.scan(&ScanContext { data_root: root, since_ts })`
   - After `scan()`, inject provenance into `NormalizedConversation.metadata` before persistence/indexing.
   - Pros: smaller change footprint.
   - Cons: connectors’ “data_root override heuristics” are inconsistent today; requires some per-connector tweaks anyway.

2) Cleaner refactor: extend `ScanContext` to carry:
   - `data_dir` (internal state root)
   - `scan_roots: Vec<ScanRoot>` where `ScanRoot` carries provenance
   - Each connector decides which roots it cares about.
   - Pros: scales better; avoids repeated connector instantiation.
   - Cons: larger API change (but project is alpha; worth it).

Recommendation: do (2) if we’re serious about remote + correctness.

### 6.2 Provenance injection point

Regardless of approach, ensure provenance is set in one place (indexer) so connectors stay simpler.

Suggested reserved metadata namespace:

`metadata["cass"]["origin"] = { "source_id": "...", "kind": "...", "host": "..." }`

Then:
- SQLite persistence stores these fields into new columns (or into metadata_json during a transitional phase).
- Tantivy indexing extracts these fields into first-class index fields.
- TUI can display from `SearchHit` directly (preferred) or fallback to metadata in detail view.

### 6.3 Fix conversation identity stability across sources

Pick one (or both) of these:

- **Proper schema fix** (preferred): uniqueness includes `source_id`.
- **Transitional fallback**: prefix `external_id` with `source_id` at ingest time (still store the original external id in metadata).

Even if you do the “proper” schema fix, prefixing can still be useful for human debugging (but it’s optional).

### 6.4 Deduplication should not erase origin distinctions

Search dedup currently keys only on normalized content (`deduplicate_hits` in `src/search/query.rs`).

For remote support, consider changing the dedup key to:
- `(normalized_content, source_id)` or
- allow duplicates across sources while still deduping within a source.

Otherwise a “best hit” from one source could hide the corresponding hit from another, undermining the “remote logs are distinct” principle.

---

## 7) Search + CLI + robot-output changes

### 7.1 CLI flags

Add filters parallel to existing `--agent` and `--workspace`:

- `cass search ... --source <id>` (repeatable)
- `cass search ... --origin local|ssh` (optional; or derive via source)
- `cass search ... --host <label>` (repeatable; optional)

Also update:
- `cass timeline` to accept `--source` and `--host`
- `cass stats` to optionally group by source

### 7.2 Robot output fields

Extend `SearchHit` (`src/search/query.rs`) with optional fields:
- `source_id: Option<String>`
- `origin_kind: Option<String>`
- `origin_host: Option<String>`

Then update:
- `output_robot_results` known field list in `src/lib.rs`
- `cass robot-docs schemas` and `cass introspect --json` schema output to document the new fields.

Maintain backwards compatibility by making new fields optional and absent when not available.

---

## 8) TUI changes (visual distinction + filtering)

### 8.1 Visual distinction (“darker shade of same base color”)

In the results list render path (`src/ui/tui.rs`), each hit currently uses `ThemePalette::agent_pane(&pane.agent)` to get `bg/fg/accent`.

Implement per-hit styling variant:

- If `hit.source_id != "local"` (or `origin_kind == "ssh"`):
  - Dim the accent: use existing `dim_color()` in `src/ui/tui.rs` (or blend with `lerp_color()` toward background).
  - Optionally dim the row stripe background slightly more for remote hits.
  - Add a badge in the “location” line, e.g.:
    - `[src:work-laptop]` or `[remote:work-laptop]`

This gives both:
- color-based distinction (darker shade)
- explicit text label (works without color)

### 8.2 Filtering UI

Extend `SearchFilters` to include `sources` and update:
- filter chips (`chips_for_filters` in `src/ui/tui.rs`)
- saved views persistence (saved view struct in `src/ui/tui.rs`)
- new input mode and shortcut (suggest: F11 for “source filter” to keep F3/F4 pattern)

---

## 9) Testing & migration plan

### 9.0 Strong recommendation for existing users: treat search DB + index as rebuildable caches

Given there are existing users, the safest user experience is:

- **Never break startup/search** due to schema drift.
- Prefer **automatic rebuild** of *derived* artifacts (Tantivy index + the main “normalized conversations” SQLite DB) when the schema changes in incompatible ways.
- Preserve truly user-authored state separately (already true today for bookmarks: `src/bookmarks.rs` uses a separate `bookmarks.db`; and UI preferences live in `tui_state.json`).

This makes upgrades reliable: we don’t need brittle SQLite table-rewrite migrations for data we can re-derive by rescanning the original agent logs.

### 9.1 SQLite migration safety

For the “rebuildable cache” approach, the plan is:

- Still bump `SCHEMA_VERSION` in `src/storage/sqlite.rs` to document expected schema.
- If `migrate()` encounters an **unsupported** or **incompatible** schema version, do **not** error-out for users.
  - Instead: move the existing DB out of the way (e.g., rename to `agent_search.db.bak-<timestamp>`), create a fresh DB, and trigger/require a full reindex.
  - Optionally keep only a single backup (or keep indefinitely; deletion policy is a product decision).

If we decide we *do* want to preserve some DB-resident non-derived state in the future, then we should:

- Keep that state in a separate DB/table namespace (like bookmarks already do), so the main search DB remains safely rebuildable.

Include tests to validate:
- existing DB upgrades cleanly
- no data loss across migration
- uniqueness behaves as expected

### 9.2 Tantivy schema bump

Update `src/search/tantivy.rs`:
- add `source_id` / `origin_kind` / `origin_host` fields
- bump `SCHEMA_HASH` so rebuild triggers correctly
- strongly consider bumping Tantivy `SCHEMA_VERSION` (directory name) as well, so old indexes can remain side-by-side for easy rollback/debug

### 9.3 Connector + indexer integration tests

Add tests that simulate:
- local + remote mirrors producing same `external_id`
- verify they do not merge incorrectly
- verify search filters by `source_id` work

---

## 10) Suggested execution plan (dependency-ordered)

This is written in “bead-like” task granularity; you can translate to actual `bd create` later if desired.

1) **Define provenance model**
   - Add `Source`/`Origin` structs and decide canonical field names for robot outputs.

2) **Add source storage**
   - SQLite: `sources` table + `conversations.source_id` + updated uniqueness.
   - Tantivy: add `source_id`/`origin_kind`/`origin_host` fields.

3) **Plumb provenance into indexing**
   - Indexer injects provenance into each `NormalizedConversation` (or equivalent).
   - Ensure collisions cannot merge across sources.

4) **CLI + robot output updates**
   - Add `--source` / `--host` flags.
   - Update robot schemas/docs/introspect.

5) **TUI distinction + filter**
   - Add badge + dim color variant for remote hits.
   - Add source filter chip and input mode + shortcuts.

6) **Remote sources config + sync**
   - Create a config file in data dir (or reuse `.env` only for dev).
   - Implement `cass sources add/list/sync/doctor`.
   - Implement rsync-first sync engine with safe defaults (no delete).

7) **Workspace rewrite rules (optional but high value)**
   - Implement per-source path mapping.

8) **Tests + fixture coverage**
   - Add regression tests covering collisions, filters, and UI formatting decisions.

---

## 11) Notes for CMS integration

If cass implements the above:

- CMS can treat cass as the canonical “search over all agent history (including remote)” layer.
- CMS can rely on stable fields in robot output:
  - `agent`, `source_path`, `line_number`, plus new `source_id` / `origin_kind` / `origin_host`.
- CMS “remote logs” UI can match cass semantics:
  - same base agent color, darker variant when `origin_kind != local`.

This reduces duplication: CMS doesn’t need to re-implement SSH sync or provenance modeling unless it has additional needs beyond search/history.
