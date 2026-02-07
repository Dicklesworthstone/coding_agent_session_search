# TUI Interaction Model & Keymap RFC (cass)

Status: draft  
Owner: RedHill  
Related issues: coding_agent_session_search-2noh9.1.4, coding_agent_session_search-2noh9.1.5, coding_agent_session_search-2noh9.2.2

## Principles
- Keyboard-first; mouse optional, never required.
- Consistency: same chord does the same thing across panes; avoid mode confusion.
- Discoverability: contextual help strip + command palette make actions findable.
- Safety: no destructive actions without confirmation; ESC always exits modals/search.
- Terminal resilience: graceful degradation on limited key support (esp. Ctrl+arrows, function keys).

## Global Keymap
- `Ctrl+P` — Command palette (fuzzy actions)
- `?` — Quick tour / help overlay
- `F1` — Toggle help strip pin/unpin (if available)
- `Esc` — Close modal/overlay; clear inline searches; cancel prompts
- `Tab` / `Shift+Tab` — Cycle focus panes (search → results → detail → footer/help)
- `Ctrl+S` — Save view (see Saved Views)
- `Ctrl+R` — Reload index/view state (non-destructive)
- `F12` — Cycle ranking mode (recent / balanced / relevance / quality)
- `Ctrl+B` — Toggle rounded/plain borders
- `D` — Cycle density (Compact/Cozy/Spacious)
- `g` / `G` — Jump to top/bottom of list
- `PageUp/PageDown` or `Ctrl+U/Ctrl+D` — Scroll results pane by page

## Search Bar
- Direct typing — live query
- `Up/Down` — Navigate query history
- `Ctrl+L` — Clear search query
- `Ctrl+W` — Delete last token
- `Enter` — Run search immediately (forces query even during debounce)
- `Ctrl+F` — Toggle wildcard fallback indicator (UI only; does not change query)

## Filters & Pills
- `F3` — Agent filter picker
- `F4` — Workspace filter picker
- `F5` — Time filter cycle (today/week/30d/all)
- `F6` — Custom date range prompt
- `F10` — Ranking mode cycle (alias of F12 if function keys limited)
- In pills: `Enter` to edit, `Backspace` to remove, `Left/Right` to move between pills
- Mouse: click pill to edit/remove

## Results Pane
- `Up/Down` — Move selection
- `Enter` — Open drill-in modal (thread view)
- `m` — Toggle multi-select
- `Space` — Peek XL context (tap again to restore)
- `A` — Bulk actions menu (open all, copy paths, export JSON, tag)
- `/` — In-pane quick filter; `Esc` clears
- `y` — Copy current snippet to clipboard
- `o` — Open source file in $EDITOR
- `v` — View raw source (non-interactive)
- `r` — Refresh results (re-run query)

## Detail Pane (Drill-In Modal)
- `Left/Right` — Switch tabs (Messages / Snippets / Raw)
- `c` — Copy path
- `y` — Copy selected snippet
- `o` — Open in $EDITOR
- `f` — Toggle wrap in detail view
- `Esc` — Close modal, return focus to results

## Saved Views
- `Ctrl+1..9` — Save current filters/ranking to slot
- `Shift+1..9` — Recall slot
- Toast confirms save/load; errors surface in footer

## Density & Theme
- `D` — Cycle density presets
- `Ctrl+T` — Toggle theme (dark/light)
- `F2` — Theme toggle (legacy alias)

## Update Assistant
- When banner shown: `U` upgrade, `s` skip this version, `d` view notes, `Esc` dismiss

## Minimal / Robot Mode Behavior
- When `TUI_HEADLESS=1` or stdout non-TTY: disable command palette, animation, icons; only allow `search`/`stats`/`view` via CLI.
- Shortcut hints hidden; actions reachable via flags.

## Fallbacks for Limited Key Support
- If function keys unavailable: map F3/F4/F5/F6 to `Ctrl+3/4/5/6`.
- If Ctrl+P conflicts: palette alias `Alt+P`.
- If clipboard unsupported: `y` writes to temporary file path displayed in footer.

## Mouse (optional)
- Click focus between panes.
- Scroll wheel scrolls results/detail.
- Click pill to edit/remove; click breadcrumb to change scope.
- Drag not required anywhere.

## Safety / Destructive Actions
- Bulk operations that open files only; no delete actions exist.
- Any future destructive command must confirm via y/N prompt; default No.

## Acceptance (coding_agent_session_search-002)
- Keymap is conflict-free, discoverable (help strip + palette), and defined for degraded terminals.
- ESC always backs out safely; no orphaned modal states.
- Saved view, density, and theme toggles have keybindings and documented fallbacks.
- Update assistant keys defined.

## FrankenTUI Architecture Mapping (coding_agent_session_search-2noh9.1.4)

### Core Mapping Table

| Concern | Status | cass (ftui) | Implementation Notes |
|---|---|---|---|
| Terminal lifecycle | Done | `ftui_core::terminal_session::TerminalSession` + `SessionOptions` | Centralized startup/shutdown in one session owner; guarantees cleanup on panic/exit paths. |
| Screen mode | Done | `ftui_runtime::ScreenMode::{Fullscreen, Inline { ui_height }}` + `UiAnchor` | Inline mode via `--inline` flag; fullscreen remains default. |
| Render pipeline | Done | `ftui_render::Frame` -> `BufferDiff` -> `Presenter` | Deterministic diff-based rendering with Bayesian strategy selection. |
| Event model | Done | `ftui_core::event::Event` consumed by `Program` update loop | All feature logic receives normalized ftui events via `CassMsg`. |
| Runtime orchestration | Done | `ftui_runtime::{Program, Model, Cmd, Subscription}` | Side effects are explicit (`Cmd`) and composable; cancellation/debounce via runtime. |
| Layout system | Done | `ftui_layout::{Flex, Grid, Constraint, LayoutSizeHint}` | Responsive layout with intrinsic sizing and explicit breakpoints. |
| Widget primitives | Done | `ftui_widgets::{Widget, StatefulWidget}` + targeted built-ins | Built-ins used where available; cass-specific wrappers where needed. |
| Command palette | Done | `ftui_widgets::command_palette` | Keybinding contract preserved, internals use standard widget. |
| Help system | Done | `ftui_widgets::{help, help_registry, hint_ranker}` | Discoverability preserved with context ranking. |
| Results virtualization | Done | `ftui_widgets::VirtualizedList` | Scales to large result sets with bounded render cost. |
| Modal/toast stack | Done | `ftui_widgets::{modal, toast, notification_queue}` | ESC/back semantics preserved with standardized stack behavior. |
| Focus traversal | Done | `ftui_widgets::focus::{FocusGraph, FocusManager}` | One focus graph for panes, modals, and command palette. |
| Testing harness | Done | `ftui-harness` snapshots + `ProgramSimulator` + render traces | Snapshot and state-transition testing is primary path. |
| Debug traceability | Done | `ftui_runtime::{render_trace, input_macro, AsciicastRecorder}` | Replayable traces for keyflow and rendering debugging. |

### Widget Adoption Plan (Use Built-ins First)

| Existing cass surface | Preferred ftui widget/module | Fallback if gap remains |
|---|---|---|
| Query/filter top bar | `input`, `group`, `status_line`, `help_registry` | Local wrapper in `src/ui/components` |
| Results pane | `VirtualizedList`, `scrollbar`, `list` | Keep temporary local renderer until feature parity |
| Detail pane | `paragraph`, `tree`, `json_view`, `log_viewer` | Local detail renderer while migrating tabs incrementally |
| Bulk actions / menus | `command_palette`, `modal` | Local modal wrapper with same action contract |
| Notifications | `toast` + `notification_queue` | Existing toast manager (temporary) |

### Design Decisions (Locked for Migration)

1. Use a single top-level `Model` with explicit sub-state enums rather than multiple independent runtime programs.
2. Use `FocusGraph` as the authoritative focus-routing mechanism for `Tab`/`Shift+Tab` and modal focus restore.
3. Keep current keybinding contract stable during migration; behavior changes must be explicit RFC updates.
4. Use built-in ftui widgets for palette/help/modal/virtualized list whenever available; do not re-implement equivalents.
5. Start with explicit state updates in `update()` and only adopt ftui reactive bindings in targeted areas where it reduces complexity.

### Gap-Handling Policy

- If a missing capability is generally reusable, upstream to FrankenTUI and consume via pinned git revision.
- If the behavior is cass-specific, implement a thin local adapter/wrapper in cass.
- Never block migration on "perfect widget parity" if a temporary local wrapper can preserve user behavior.
- Record each gap as a bead linked to the owning migration epic, with explicit exit criteria.

### Migration Execution Order

1. Foundation (`2noh9.2.x`): dependency + runtime skeleton + terminal/session wiring. **Done.**
2. Parity (`2noh9.3.x`): search/filter/results/detail/modals reimplemented on ftui. **Done.**
3. Enhancements (`2noh9.4.x`): inline mode, traces, advanced UX, dashboards. **In progress.**
4. QA and removal (`2noh9.5.x`, `2noh9.6.x`): test hardening, ratatui removed. **Ratatui fully removed (2noh9.6.1).**

### Acceptance for This Mapping Bead

- Mapping covers terminal lifecycle, screen modes, render pipeline, event model, layout, widgets, runtime, and testing hooks.
- Includes explicit decisions for model structure, focus strategy, reactive usage, and built-in widget adoption.
- Includes a concrete gap policy so implementation beads can proceed without reopening architecture debates.

## Analytics Dashboard IA & Interaction Spec (coding_agent_session_search-2noh9.4.18.1)

Status: finalized draft  
Owner: CoralLantern  
Related analytics beads: `z9fse.12`, `z9fse.6`, `z9fse.10`, `z9fse.11`  
Time semantics: UTC-only for v1

### Scope and Constraints

- Defines a single interaction contract for ftui analytics surfaces so CLI + TUI semantics do not drift.
- Uses rollup-first queries by default; deep/fact scans are explicit opt-in paths.
- Requires full keyboard operation; mouse is an enhancement only.
- Keeps existing search workflows central: analytics drilldown must jump into the main Search surface with pre-applied filters.

### Information Architecture

Top-level analytics entry points:

- Command palette entries:
  - `Analytics: Dashboard`
  - `Analytics: Explorer`
  - `Analytics: Heatmap`
  - `Analytics: Breakdowns`
  - `Analytics: Tools`
  - `Analytics: Cost Models`
  - `Analytics: Coverage`
- Global hotkey: `Alt+A` (fallback `g a` chord)

Analytics subviews:

| View | Primary Goal | Core Widgets | Primary Data Source |
|---|---|---|---|
| Dashboard | At-a-glance health and trend deltas | KPI tiles + sparklines + top movers | `usage_daily`, `token_daily_stats`, cost rollups |
| Explorer | Time-series comparison and overlays | Line/area chart + overlay legend + cursor tooltip | rollups by bucket (hour/day/week/month) |
| Heatmap | Calendar activity scan | Calendar heatmap + legend + day cursor | daily rollups |
| Breakdowns | Ranked dimensions and trends | Virtualized sortable table + per-row sparkline | grouped rollups |
| Tools | Tool usage efficiency | per-tool table + trend strip + jump action | `tool_usage_hourly/daily`, `tool_calls_detail` |
| Cost Models | Model/provider spend behavior | stacked token/cost bars + model table | pricing + model rollups |
| Coverage | Data quality visibility | coverage matrix + warning list | API coverage counters + connector health |

### Layout and Minimum Terminal Sizes

- Recommended: `>= 160x44` for full analytics layout with side panels.
- Supported baseline: `>= 120x36` with compressed sidebars.
- Narrow mode (`< 120 cols`): single-primary-pane with drawer overlays for filters and legend.
- Hard minimum for reliable operation: `100x30`; below this, show inline "expand terminal" guidance.

### Global Filters (Persisted While in Analytics)

Filter ribbon (top) plus collapsible filter drawer (narrow mode):

- Time range:
  - presets: today, 7d, 30d, 90d, YTD, all
  - custom: since/until
- Agent: multi-select with fuzzy match
- Workspace: multi-select with fuzzy match
- Source: all/local/remote/source-id
- Optional advanced: role filter for message role segmentation

Persistence rules:

- Analytics filters persist separately from general search ad hoc filters.
- On leaving analytics and returning in the same session, previous analytics filter state is restored.
- On app restart, last analytics filter state is restored from `tui_state.json`-compatible persistence.

### Drilldown Semantics (Contract)

- `Enter` on a chart point, heatmap day, or KPI delta opens Search view with inherited filters:
  - time bucket mapped to `created_from/created_to`
  - analytics agent/workspace/source selections applied
  - query left empty by default
- `Enter` on a breakdown row opens Search with inherited filters plus row dimension:
  - examples: `agent=codex`, `workspace=<id>`, `tool_name=Read`
- Drilldown always pushes a view-stack entry.
- `Esc` returns to prior analytics view without losing filter context.

### Keyboard Model

Global within analytics:

- `Tab` / `Shift+Tab`: cycle analytics subviews
- `g`: open analytics-local "go to view" selector
- `/`: open filter/search within the active analytics view
- `Enter`: drilldown to Search
- `Esc`: back (pop view stack)
- `?`: analytics-context help overlay

Explorer-specific:

- `Left/Right`: move bucket cursor
- `Up/Down`: cycle overlay dimension (agent/workspace/source/model)
- `[` / `]`: previous/next metric

Breakdowns-specific:

- `s`: cycle sort column
- `r`: reverse sort direction
- `Space`: pin/unpin selected row for comparison

### Command Palette Contract

Required command entries:

- `Analytics: Dashboard`
- `Analytics: Explorer`
- `Analytics: Heatmap`
- `Analytics: Breakdowns`
- `Analytics: Tools`
- `Analytics: Cost Models`
- `Analytics: Coverage`
- `Analytics: Reset Filters`
- `Analytics: Jump to Search (Current Scope)`

### Data Semantics and Definitions

- All bucketing in v1 is UTC.
- Week definition: ISO week (Mon-Sun).
- Month definition: calendar month UTC.
- Costs are marked as:
  - measured (API cost data present)
  - estimated (derived from pricing table)
  - unavailable (insufficient metadata)
- Coverage panel must disclose when metrics are partially estimated or stale.

### Dependency Mapping to Analytics Beads

| UI Capability | Dependency Bead | Why |
|---|---|---|
| Shared analytics query semantics across CLI/TUI | `z9fse.12` | prevents drift and duplicated logic |
| Tool breakdown and trends | `z9fse.6` | provides per-tool rollups and fact linkage |
| USD cost estimations and labels | `z9fse.10` | powers cost/cost-model surfaces |
| Stable model attribution dimensions | `z9fse.11` | enables model/provider grouping consistency |

### Snapshot and PTY Test Targets

Snapshot targets (ftui-harness):

- Dashboard default state with 30d preset
- Explorer with agent overlay active
- Heatmap with legend + selected day
- Breakdowns sorted by descending tokens
- Tools view with selected tool row and trend sparkline
- Coverage matrix with at least one warning badge

PTY e2e flow targets:

- Launch TUI -> open analytics (`Alt+A`) -> switch Dashboard -> Explorer -> Heatmap.
- Apply filters (time + agent + source) and verify ribbon state.
- Perform drilldown with `Enter` and verify Search is scoped correctly.
- Use `Esc` back-stack unwind and confirm prior analytics state restored.
- Exit cleanly with no terminal corruption.

### Acceptance Criteria

- IA is explicit enough for implementation without reopening interaction design.
- Keymap + command palette entries are specified and non-conflicting.
- Drilldown behavior is deterministic and reversible (`Esc`).
- Dependencies on `z9fse.12`, `.6`, `.10`, `.11` are explicit and actionable.
- Test targets are concrete for both snapshot and PTY coverage.

## Finalized Interaction Contract (coding_agent_session_search-2noh9.1.5)

> Status: **finalized**
> Audited against: `src/ui/tui.rs`, `src/ui/shortcuts.rs`, `src/ui/components/export_modal.rs`
> Date: 2026-02-06

---

### 1. Complete Context Keymap Matrix

Every binding below is **conflict-free within its context scope**. Contexts are
listed from broadest (Global) to narrowest (sub-modal). A key listed in a
narrower scope shadows the same key in a broader scope while that context is
active.

#### 1.1 Global (always active unless a modal captures input)

| Key | Action | Notes |
|---|---|---|
| `Ctrl+C` | Force quit | Overrides ALL modes, never intercepted |
| `Esc` | Context-sensitive unwind (see ESC contract below) | |
| `F10` | Quit alias (same as Esc when no pending state) | |
| `F1` / `?` / `Ctrl+?` | Toggle help overlay | |
| `F2` | Toggle theme (dark/light) | Alias: `Ctrl+T` |
| `Ctrl+T` | Toggle theme | Primary after migration; F2 kept for compat |
| `F3` | Agent filter picker | Enter InputMode::Agent |
| `Shift+F3` | Quick-scope to current result's agent | |
| `F4` | Workspace filter picker | Enter InputMode::Workspace |
| `Shift+F4` | Clear agent scope (show all) | |
| `F5` | Date-from filter prompt | Enter InputMode::CreatedFrom |
| `Shift+F5` | Cycle time presets (24h / 7d / 30d / all) | |
| `F6` | Date-to filter prompt | Enter InputMode::CreatedTo |
| `F7` | Cycle context window (Small / Medium / Large / XLarge) | |
| `F8` | Open selected result in $EDITOR | No-op if nothing selected |
| `F9` | Cycle match mode (Standard / Prefix) | |
| `F11` | Cycle source filter (All / Local / Remote / per-host) | |
| `Shift+F11` | Open source filter popup menu | |
| `F12` | Cycle ranking mode (Recent / Balanced / Relevance / Quality / Date Newest / Date Oldest) | |
| `Alt+S` | Cycle search mode (Lexical / Semantic / Hybrid) | |
| `Ctrl+P` / `Alt+P` | Open command palette | `Alt+P` fallback for Ctrl+P conflicts |
| `Ctrl+R` | Cycle through query history | When history exists |
| `Ctrl+Shift+R` | Refresh search index (trigger re-index) | |
| `Ctrl+Del` | Clear all active filters | |
| `Ctrl+Shift+Del` | Reset entire UI state (delete tui_state.json) | |
| `Ctrl+B` | Toggle fancy/plain borders | |
| `Ctrl+D` | Cycle density (Compact / Cozy / Spacious) | |
| `Tab` / `Shift+Tab` | Cycle focus (SearchInput -> Results -> Detail) | |
| `Alt+h/j/k/l` | Vim-style directional pane nav | |
| `Alt+g` / `Alt+G` | Jump first / last item in focused pane | |
| `Alt+1..9` | Quick-switch to pane N | |
| `Shift+=` / `+` | Increase pane size (+2 items, max 50) | |
| `Alt+-` | Decrease pane size (-2 items, min 4) | |
| `PageUp` / `PageDown` | Page-level scroll in focused pane | |
| `Ctrl+1..9` | Save current view to slot N | Persists filters + ranking + density |
| `Shift+1..9` | Load view from slot N | Toast confirms |

#### 1.2 Search Input (when query bar has focus)

| Key | Action | Notes |
|---|---|---|
| Printable chars | Append to query, trigger live search | |
| `Backspace` | Delete char; if empty, clear last filter (time -> workspace -> agent) | |
| `Enter` | Force search now (skip debounce); if empty + history, load most recent | |
| `Up` / `Down` | Navigate query suggestions / history | |
| `Ctrl+n` / `Ctrl+p` | History next / previous | |
| `Ctrl+L` | Clear search query text | |
| `Ctrl+W` | Delete last token (word) | |
| `Ctrl+F` | Toggle wildcard fallback indicator | UI indicator only |
| `/` | Enter PaneFilter mode (local results filter) | Context-sensitive: in query bar = pane filter |

#### 1.3 Results Pane (when results list has focus)

| Key | Action | Notes |
|---|---|---|
| `Up` / `Down` / `j` / `k` | Move selection | |
| `Home` / `End` / `g` / `G` | Jump to first / last result | |
| `Enter` | Open detail modal for selected result | |
| `Space` / `Ctrl+Space` | Peek XL context (toggle; tap again to restore) | |
| `y` / `Ctrl+Y` | Copy current snippet to clipboard | |
| `o` | Open source file in $EDITOR | |
| `v` | View raw source (non-interactive) | |
| `r` | Refresh results (re-run current query) | |
| `/` | Enter in-pane quick filter; Esc clears | |
| `Ctrl+X` | Toggle multi-select on current item | |
| `Ctrl+A` | Select / deselect all in pane | |
| `Ctrl+Enter` | Enqueue item (select + advance to next) | |
| `Ctrl+O` | Open all enqueued items in $EDITOR | |
| `Ctrl+E` | Quick export with defaults | |
| `A` | Open bulk actions menu | Only when items selected |
| `Left` / `h` | Move focus to previous pane or exit to results | |
| `Right` / `l` | Move focus to detail pane | |

#### 1.4 Detail Pane (when detail view has focus)

| Key | Action | Notes |
|---|---|---|
| `Up` / `Down` / `j` / `k` | Scroll content | |
| `PageUp` / `PageDown` | Page scroll | |
| `Home` / `g` | Jump to top | |
| `End` / `G` | Jump to bottom | |
| `[` / `]` or `Left` / `Right` | Cycle tabs (Messages / Snippets / Raw) | |
| `c` | Copy rendered content to clipboard | |
| `p` | Copy source path to clipboard | |
| `s` | Copy snippet to clipboard | |
| `y` | Copy selected snippet (alias of `s`) | |
| `o` | Open source file in $EDITOR | |
| `n` | Open content in nano editor | |
| `e` | Open export modal | |
| `Ctrl+E` | Quick export with defaults | |
| `f` | Toggle text wrap in detail view | |
| `/` | Enter detail-local find mode | |
| `Esc` | Close detail modal, restore focus to results | |

#### 1.5 Detail Find Mode (sub-mode within Detail)

| Key | Action | Notes |
|---|---|---|
| Printable chars | Build find query, highlight matches | |
| `Backspace` | Delete char from find query | |
| `Enter` | Apply find, jump to first match | |
| `n` | Next match | |
| `N` (Shift+n) | Previous match | |
| `Esc` | Exit find mode, return to detail navigation | |

#### 1.6 In-Pane Filter Mode (sub-mode within Results)

| Key | Action | Notes |
|---|---|---|
| Printable chars | Build filter text, narrow results live | |
| `Backspace` | Delete char from filter | |
| `Enter` | Apply pane filter (persist filtering) | |
| `Esc` | Cancel filter, restore unfiltered results | |

#### 1.7 Filter Input Modes (Agent, Workspace, DateFrom, DateTo)

| Key | Action | Notes |
|---|---|---|
| Printable chars | Build filter value | Agent/workspace show suggestions |
| `Tab` | Auto-complete to first matching suggestion | |
| `Backspace` | Delete char | |
| `Enter` | Apply filter, return to Query mode | |
| `Esc` | Cancel, return to Query mode | |

#### 1.8 Command Palette (Ctrl+P / Alt+P)

| Key | Action | Notes |
|---|---|---|
| Printable chars | Search/filter palette actions | |
| `Backspace` | Delete char from palette search | |
| `Up` / `Down` | Move selection (-1 / +1) | |
| `PageUp` / `PageDown` | Move selection (-5 / +5) | |
| `Enter` | Execute selected action | |
| `Esc` | Close palette, clear search | |

#### 1.9 Help Overlay (F1 / ?)

| Key | Action | Notes |
|---|---|---|
| `Up` / `Down` / `j` / `k` | Scroll help content | |
| `PageUp` / `PageDown` | Page scroll | |
| `Home` / `End` | Jump to top / bottom | |
| `Esc` / `F1` / `?` | Close help overlay | |

#### 1.10 Bulk Actions Modal (A key)

| Key | Action | Notes |
|---|---|---|
| `Up` / `Down` / `j` / `k` | Navigate action list | |
| `Enter` | Execute selected bulk action | |
| `Esc` | Cancel, keep selections intact | |

Actions: Copy selected paths, Copy selected content, Open selected in editor, Export selected.

#### 1.11 Export Modal (e key in detail)

| Key | Action | Notes |
|---|---|---|
| `Tab` / `Shift+Tab` | Cycle focus between form fields | |
| `Space` | Toggle checkbox (encryption, timestamps, etc.) | |
| `Backspace` / chars | Edit text fields (password, filename) | |
| `Ctrl+H` | Toggle password visibility | |
| `Enter` | Execute export | |
| `Esc` | Cancel export | |

#### 1.12 Consent Dialog (semantic model download)

| Key | Action | Notes |
|---|---|---|
| `d` / `D` | Start model download (~23MB) | |
| `h` / `H` | Show help/info about model | |
| `Esc` | Cancel dialog (or cancel active download) | |

#### 1.13 Update Assistant (when version banner shown)

| Key | Action | Notes |
|---|---|---|
| `U` | Open release page in browser | |
| `s` / `S` | Skip this version (persisted) | |
| `d` | View release notes | |
| `Esc` | Dismiss for session only | |

#### 1.14 Source Filter Menu (Shift+F11)

| Key | Action | Notes |
|---|---|---|
| `Up` / `Down` / `j` / `k` | Navigate source options | |
| `Enter` | Select source filter | |
| `Esc` | Close menu | |

#### 1.15 Did-You-Mean Suggestions

| Key | Action | Notes |
|---|---|---|
| `1` / `2` / `3` | Apply suggestion N | Only shown when results empty + misspelling detected |

---

### 2. Reconciliation with `src/ui/shortcuts.rs`

| Constant | Current Value | Final Contract | Status |
|---|---|---|---|
| `HELP` | `F1` | `F1` / `?` / `Ctrl+?` | keep (extend aliases) |
| `THEME` | `F2` | `F2` + `Ctrl+T` | keep (add alias) |
| `FILTER_AGENT` | `F3` | `F3` | keep |
| `FILTER_WORKSPACE` | `F4` | `F4` | keep |
| `FILTER_DATE_FROM` | `F5` | `F5` | keep |
| `FILTER_DATE_TO` | `F6` | `F6` | keep |
| `CONTEXT_WINDOW` | `F7` | `F7` | keep |
| `EDITOR` | `F8` | `F8` | keep |
| `MATCH_MODE` | `F9` | `F9` | keep |
| `SEARCH_MODE` | `Alt+S` | `Alt+S` | keep |
| `QUIT` | `Esc/F10` | `Esc/F10` | keep |
| `CLEAR_FILTERS` | `Ctrl+Del` | `Ctrl+Del` | keep |
| `RESET_STATE` | `Ctrl+Shift+Del` | `Ctrl+Shift+Del` | keep |
| `RANKING` | `F12` | `F12` | keep |
| `REFRESH` | `Ctrl+Shift+R` | `Ctrl+Shift+R` | keep |
| `DETAIL_OPEN` | `Enter` | `Enter` | keep |
| `DETAIL_CLOSE` | `Esc` | `Esc` | keep |
| `FOCUS_QUERY` | `/` | `/` (context-sensitive) | keep (clarified) |
| `HISTORY_NEXT` | `Ctrl+n` | `Ctrl+n` | keep |
| `HISTORY_PREV` | `Ctrl+p` | `Ctrl+p` | keep |
| `HISTORY_CYCLE` | `Ctrl+R` | `Ctrl+R` | keep |
| `SCOPE_AGENT` | `Shift+F3` | `Shift+F3` | keep |
| `SCOPE_WORKSPACE` | `Shift+F4` | `Shift+F4` | keep |
| `CYCLE_TIME_PRESETS` | `Shift+F5` | `Shift+F5` | keep |
| `COPY` | `y` | `y` / `Ctrl+Y` | keep (add modifier alias) |
| `BULK_MENU` | `A` | `A` | keep |
| `TOGGLE_SELECT` | `Ctrl+X` | `Ctrl+X` | keep (reverted from draft `Ctrl+M`) |
| `PANE_FILTER` | `/` | `/` (context-sensitive) | keep (clarified) |
| `TAB_FOCUS` | `Tab` | `Tab` / `Shift+Tab` | keep |
| `VIM_NAV` | `Alt+h/j/k/l` | `Alt+h/j/k/l` | keep |
| `JUMP_TOP` | `Home` | `Home` / `g` | keep (extend alias) |
| `JUMP_BOTTOM` | `End` | `End` / `G` | keep (extend alias) |

**New constants to add to `shortcuts.rs` post-migration:**

| Constant | Value | Purpose |
|---|---|---|
| `SOURCE_FILTER` | `F11` | Cycle source filter |
| `SOURCE_FILTER_MENU` | `Shift+F11` | Source filter popup |
| `PALETTE` | `Ctrl+P` | Command palette |
| `PALETTE_ALT` | `Alt+P` | Palette fallback |
| `SELECT_ALL` | `Ctrl+A` | Select/deselect all |
| `ENQUEUE` | `Ctrl+Enter` | Multi-select enqueue |
| `OPEN_QUEUED` | `Ctrl+O` | Open all enqueued |
| `QUICK_EXPORT` | `Ctrl+E` | Quick export |
| `PEEK_XL` | `Ctrl+Space` | Peek XL context |
| `BORDERS` | `Ctrl+B` | Toggle border style |
| `DENSITY` | `Ctrl+D` | Cycle density |
| `THEME_ALT` | `Ctrl+T` | Theme toggle |
| `PANE_GROW` | `Shift+=` | Increase pane size |
| `PANE_SHRINK` | `Alt+-` | Decrease pane size |

---

### 3. Focus Model (ftui FocusGraph Integration)

#### 3.1 Focus Nodes

Each focusable UI region maps to a `FocusNode` with a stable `FocusId`:

| FocusId | Region | tab_index | group_id |
|---|---|---|---|
| `FOCUS_SEARCH` | Search/query input bar | 0 | `GROUP_MAIN` |
| `FOCUS_RESULTS` | Results list pane | 1 | `GROUP_MAIN` |
| `FOCUS_DETAIL` | Detail/preview pane | 2 | `GROUP_MAIN` |

Modal focus nodes (created on push, destroyed on pop):

| FocusId | Region | group_id |
|---|---|---|
| `FOCUS_PALETTE_INPUT` | Command palette search | `GROUP_PALETTE` |
| `FOCUS_PALETTE_LIST` | Command palette results | `GROUP_PALETTE` |
| `FOCUS_HELP_CONTENT` | Help overlay scroll area | `GROUP_HELP` |
| `FOCUS_BULK_LIST` | Bulk actions list | `GROUP_BULK` |
| `FOCUS_EXPORT_*` | Export modal form fields | `GROUP_EXPORT` |
| `FOCUS_DETAIL_MODAL` | Detail drill-in modal | `GROUP_DETAIL_MODAL` |
| `FOCUS_CONSENT` | Consent dialog | `GROUP_CONSENT` |
| `FOCUS_SOURCE_MENU` | Source filter menu | `GROUP_SOURCE` |

#### 3.2 Focus Graph Edges (directional navigation)

```
Tab order (Next/Prev):
  FOCUS_SEARCH <-> FOCUS_RESULTS <-> FOCUS_DETAIL (wraps)

Spatial edges (Left/Right/Up/Down):
  FOCUS_SEARCH  --Down-->  FOCUS_RESULTS
  FOCUS_RESULTS --Up-->    FOCUS_SEARCH
  FOCUS_RESULTS --Right--> FOCUS_DETAIL
  FOCUS_DETAIL  --Left-->  FOCUS_RESULTS
```

#### 3.3 Focus Groups and Trapping

```rust
// Main application group - Tab cycles through these
FocusGroup { id: GROUP_MAIN, members: [SEARCH, RESULTS, DETAIL], wrap: true }

// Modal groups - focus trapped within until popped
FocusGroup { id: GROUP_PALETTE, members: [PALETTE_INPUT, PALETTE_LIST], wrap: true, exit_key: Escape }
FocusGroup { id: GROUP_EXPORT, members: [EXPORT_FIELDS...], wrap: true, exit_key: Escape }
FocusGroup { id: GROUP_DETAIL_MODAL, members: [DETAIL_MODAL], wrap: false, exit_key: Escape }
FocusGroup { id: GROUP_HELP, members: [HELP_CONTENT], wrap: false, exit_key: Escape }
FocusGroup { id: GROUP_BULK, members: [BULK_LIST], wrap: false, exit_key: Escape }
FocusGroup { id: GROUP_CONSENT, members: [CONSENT], wrap: false, exit_key: Escape }
FocusGroup { id: GROUP_SOURCE, members: [SOURCE_MENU], wrap: false, exit_key: Escape }
```

#### 3.4 Focus Trap Stack (Modal Lifecycle)

When a modal opens:
1. `focus_manager.push_trap(GROUP_<modal>)` — saves `return_focus` to current node
2. Focus moves to first member of the modal's group
3. Tab/Shift+Tab cycle within group only (trap)

When a modal closes:
1. `focus_manager.pop_trap()` — restores `return_focus`
2. Modal group nodes are removed from graph

**Stack ordering (innermost = highest priority):**

```
[bottom] GROUP_MAIN
         GROUP_HELP          (F1/?)
         GROUP_PALETTE       (Ctrl+P)
         GROUP_DETAIL_MODAL  (Enter on result)
         GROUP_EXPORT        (e in detail)
         GROUP_CONSENT       (semantic model prompt)
         GROUP_BULK          (A key)
         GROUP_SOURCE        (Shift+F11)
[top]    (active prompt)
```

Only the top group receives keyboard input. Background groups render but do not interact.

#### 3.5 Focus Transitions (Concrete Examples)

**Opening detail modal:**
```
State: focus = FOCUS_RESULTS, trap_stack = [GROUP_MAIN]
User presses Enter
  -> push_trap(GROUP_DETAIL_MODAL)
  -> focus = FOCUS_DETAIL_MODAL
  -> trap_stack = [GROUP_MAIN, GROUP_DETAIL_MODAL]
  -> Tab cycles within: [FOCUS_DETAIL_MODAL] only
```

**Opening export from within detail:**
```
State: focus = FOCUS_DETAIL_MODAL, trap_stack = [GROUP_MAIN, GROUP_DETAIL_MODAL]
User presses e
  -> push_trap(GROUP_EXPORT)
  -> focus = FOCUS_EXPORT_FORMAT (first field)
  -> trap_stack = [GROUP_MAIN, GROUP_DETAIL_MODAL, GROUP_EXPORT]
```

**Closing export (Esc):**
```
  -> pop_trap() -> return_focus = FOCUS_DETAIL_MODAL
  -> trap_stack = [GROUP_MAIN, GROUP_DETAIL_MODAL]
```

**Closing detail (Esc again):**
```
  -> pop_trap() -> return_focus = FOCUS_RESULTS
  -> trap_stack = [GROUP_MAIN]
```

#### 3.6 Focus Flash Indicator

On focus change: render a 220ms highlight on the newly focused pane border.
Implementation: `Cmd::tick(Duration::from_millis(220))` -> message clears flash state.

---

### 4. Modal and Overlay Semantics

#### 4.1 Modal Stack Priority (rendering order, back-to-front)

1. Main panes (SearchInput, Results, Detail) — always rendered
2. Help overlay — renders over main, dims background
3. Command palette — renders over everything below
4. Detail drill-in modal — replaces detail pane area
5. Export modal — renders over detail modal
6. Source filter menu — dropdown overlay
7. Bulk actions menu — centered modal
8. Consent dialog — centered modal, highest non-prompt priority
9. Active text prompt — inline within its parent modal

#### 4.2 Input Routing

Only the **top-most overlay** receives keyboard input:
- If consent dialog is open -> consent keys only
- If export modal is open -> export keys only
- If detail modal is open -> detail keys only (unless export is stacked)
- If help overlay is open -> help scroll keys only
- If command palette is open -> palette keys only
- Otherwise -> main pane keys based on current focus

#### 4.3 Background Rendering

Panes below the active overlay continue rendering (results may update from async search).
They are visually dimmed to indicate non-interactivity.

---

### 5. ESC Unwind Contract

On `Esc`, unwind in this **exact priority order** until one action succeeds:

1. **Cancel active text input** (filter input, palette search, export field, find query).
2. **Close top-most modal** (pop_trap in FocusManager, restore prior focus).
3. **Exit pane-local find/filter** mode (ResultsList or DetailPane).
4. **Clear multi-select** if any items are selected.
5. **Close detail pane** if open (return focus to Results).
6. **Quit TUI session** if no pending state remains.

`Ctrl+C` bypasses this chain entirely and force-quits immediately.

---

### 6. State Persistence (tui_state.json)

**Location:** `~/.local/share/coding-agent-search/tui_state.json`

**Persisted on change:**

| Field | Type | Default |
|---|---|---|
| `search_mode` | `"lexical"` / `"semantic"` / `"hybrid"` | `"lexical"` |
| `match_mode` | `"standard"` / `"prefix"` | `"standard"` |
| `ranking_mode` | `"recent"` / `"balanced"` / `"relevance"` / `"quality"` / ... | `"balanced"` |
| `context_window` | `"small"` / `"medium"` / `"large"` / `"xlarge"` | `"medium"` |
| `theme_dark` | bool | `true` |
| `density_mode` | `"compact"` / `"cozy"` / `"spacious"` | `"cozy"` |
| `per_pane_limit` | usize (4..50) | `10` |
| `query_history` | Vec<String> (max 50, deduplicated) | `[]` |
| `saved_views` | Vec<SavedViewPersisted> (slots 1-9) | `[]` |
| `fancy_borders` | bool | `true` |
| `has_seen_help` | bool | `false` |

**Reset:** `Ctrl+Shift+Del` deletes tui_state.json and resets all fields to defaults.

**ftui integration:** Use `Cmd::SaveState` / `Cmd::RestoreState` for persistence lifecycle.

---

### 7. Headless / Robot Mode Constraints

When `TUI_HEADLESS=1` or stdout is non-TTY:

| Feature | Behavior |
|---|---|
| Terminal raw mode | **Disabled** — no raw mode setup |
| Mouse capture | **Disabled** |
| Alt-screen | **Disabled** |
| Rendering / event loop | **Disabled** — exits after non-interactive operation |
| Command palette | **Disabled** |
| Modal animations | **Disabled** |
| Decorative effects | **Disabled** |
| Focus management | **Disabled** |
| All interactive modals | **Disabled** |
| State persistence | **Enabled** (`--reset-state` supported) |
| CLI flag operations | **Enabled** (search, stats, view via flags) |

For ftui snapshot/smoke tests:
- Use `ProgramSimulator` from ftui-harness
- Keep deterministic render path
- Overlay-only actions are no-ops unless explicitly invoked by test harness API

---

### 8. Degraded Terminal Fallbacks

| Missing Feature | Fallback | Notes |
|---|---|---|
| Function keys (F1-F12) | `Ctrl+3/4/5/6` for F3-F6 filters; `Ctrl+T` for F2 theme; `?` for F1 help | |
| `Ctrl+P` conflict (tmux) | `Alt+P` palette alias | |
| Clipboard (no OSC 52) | `y` writes to temp file; path shown in footer toast | |
| Narrow terminal (<80 cols) | Collapse visual affordances; hide help strip; preserve all keybindings | |
| No Unicode support | Plain ASCII borders (`+`, `-`, `|`); no emoji in pills | Detected via `$TERM` |
| No color support | Monochrome mode; bold/underline for emphasis | |

---

### 9. Mouse Behavior (Optional, Never Required)

| Event | Region | Action |
|---|---|---|
| Left click | Pane header | Focus that pane |
| Left click | Result item | Select item + show detail |
| Left click | Detail area | Focus detail pane |
| Left click | Filter pill | Edit pill value |
| Left click | Breadcrumb | Change scope |
| Scroll up/down | Results pane | Scroll result selection |
| Scroll up/down | Detail pane | Scroll detail content |

**Ignored when any modal is open** (help, palette, export, bulk, consent, source menu).

**Drag:** Not required anywhere. No drag interactions.

---

### 10. Action System Contract

Every user action maps to a named action for palette registration and keybinding docs:

| Action Name | Keys | Requires |
|---|---|---|
| `help.toggle` | `F1`, `?`, `Ctrl+?` | — |
| `theme.toggle` | `F2`, `Ctrl+T` | — |
| `filter.agent` | `F3` | — |
| `filter.agent.scope` | `Shift+F3` | Selected result |
| `filter.workspace` | `F4` | — |
| `filter.agent.clear_scope` | `Shift+F4` | — |
| `filter.date_from` | `F5` | — |
| `filter.time_presets` | `Shift+F5` | — |
| `filter.date_to` | `F6` | — |
| `filter.clear_all` | `Ctrl+Del` | — |
| `context_window.cycle` | `F7` | — |
| `editor.open` | `F8`, `o` | Selected result |
| `match_mode.cycle` | `F9` | — |
| `source_filter.cycle` | `F11` | — |
| `source_filter.menu` | `Shift+F11` | — |
| `ranking.cycle` | `F12` | — |
| `search_mode.cycle` | `Alt+S` | — |
| `palette.open` | `Ctrl+P`, `Alt+P` | — |
| `history.cycle` | `Ctrl+R` | History non-empty |
| `history.next` | `Ctrl+n` | History non-empty |
| `history.prev` | `Ctrl+p` | History non-empty |
| `index.refresh` | `Ctrl+Shift+R` | — |
| `state.reset` | `Ctrl+Shift+Del` | — |
| `borders.toggle` | `Ctrl+B` | — |
| `density.cycle` | `Ctrl+D` | — |
| `view.save` | `Ctrl+1..9` | — |
| `view.load` | `Shift+1..9` | Slot non-empty |
| `result.select` | `Enter` | Result focused |
| `result.peek` | `Space`, `Ctrl+Space` | Result focused |
| `result.copy` | `y`, `Ctrl+Y` | Result focused |
| `result.open` | `o` | Result focused |
| `result.view_raw` | `v` | Result focused |
| `result.refresh` | `r` | — |
| `select.toggle` | `Ctrl+X` | Result focused |
| `select.all` | `Ctrl+A` | — |
| `select.enqueue` | `Ctrl+Enter` | Result focused |
| `select.open_all` | `Ctrl+O` | Selection non-empty |
| `bulk.menu` | `A` | Selection non-empty |
| `export.quick` | `Ctrl+E` | Result focused |
| `export.modal` | `e` | Detail open |
| `detail.copy_content` | `c` | Detail open |
| `detail.copy_path` | `p` | Detail open |
| `detail.copy_snippet` | `s` | Detail open |
| `detail.open_nano` | `n` | Detail open |
| `detail.wrap` | `f` | Detail open |
| `detail.tab_cycle` | `[`, `]` | Detail open |
| `detail.find` | `/` | Detail focused |
| `pane.filter` | `/` | Results focused |
| `pane.grow` | `Shift+=` | — |
| `pane.shrink` | `Alt+-` | — |
| `nav.focus_next` | `Tab` | — |
| `nav.focus_prev` | `Shift+Tab` | — |
| `nav.top` | `Home`, `g` | — |
| `nav.bottom` | `End`, `G` | — |
| `nav.page_up` | `PageUp` | — |
| `nav.page_down` | `PageDown` | — |
| `app.quit` | `Esc`, `F10` | No pending state |
| `app.force_quit` | `Ctrl+C` | Always |

---

### 11. Conflict Audit

**Verified conflict-free within each context scope.** Key observations:

- `/` is context-sensitive: in search bar focus = pane filter, in detail focus = find-in-detail. No conflict because they are different focus contexts.
- `y` in results = copy snippet, `y` in detail = copy snippet. Same semantic, no conflict.
- `g`/`G` for jump: only active when results/detail focused, not during text input. No conflict with filter char input.
- `D` for density: only active in query mode, not consumed during filter text input modes.
- `n` in detail = open nano, `n` in detail-find = next match. Different sub-contexts (find active vs not). No conflict.
- `Ctrl+R`: history cycle in search, index refresh is `Ctrl+Shift+R`. Different modifiers, no conflict.

---

### Acceptance Criteria for This Bead

- [x] Every shortcut in `src/ui/shortcuts.rs` is accounted for with keep/change rationale.
- [x] All shortcuts discovered in `src/ui/tui.rs` implementation are documented (100+ bindings).
- [x] No duplicate meaning exists for a shortcut within the same interaction context.
- [x] Focus traversal mapped to ftui `FocusGraph` / `FocusManager` / `FocusGroup` with concrete IDs.
- [x] Modal stack lifecycle (push_trap/pop_trap) is deterministic and documented.
- [x] ESC unwind contract has explicit priority ordering.
- [x] State persistence fields enumerated with defaults.
- [x] Headless and degraded-terminal behaviors are explicit and testable.
- [x] Action system named for palette registration and future keybinding customization.
- [x] Conflict audit performed and no conflicts found.
