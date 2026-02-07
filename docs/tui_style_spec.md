# TUI Style System Spec (cass)

Status: draft  
Owner: RedHill  
Scope: FrankenTUI (ftui) Elm-architecture UI (interactive + robot-safe fallbacks)

## Goals
- Make cass “instant, legible, delightful” without sacrificing low-end terminals.
- Provide one source of truth for colors, spacing, motion, density, and icons.
- Keep accessibility: high contrast, color is never the sole carrier of meaning.
- Ensure every visual flourish has a performance and opt-out policy.

## Color System

### Base tokens
- `bg`: #0f1115 (dark), #f8f9fb (light)
- `bg-panel`: #161922 (dark), #ffffff (light)
- `bg-muted`: #1c202a (dark), #f0f2f6 (light)
- `fg`: #e6e9f2 (dark), #1b1f2a (light)
- `fg-muted`: #a9b1c7 (dark), #51586b (light)
- `accent`: #7aa2ff (blue), secondary #ffb86c (amber) for warnings
- `border`: #2b3242 (dark), #d8dde7 (light)
- `success`: #6bd49a, `warn`: #ffb86c, `error`: #ff6b6b, `info`: #7aa2ff

### Role colors (gutter + label)
- user: #7aa2ff
- assistant: #8ef1c7
- system: #c5b3ff
- tool: #ffb86c
- other/unknown: #94a3b8

### Gradients (subtle 2-stop)
- Header/pill active: `linear( accent 0%, accent*0.75 100% )`
- Selection glow (optional): `accent 20% alpha` to `accent 0% alpha`
- Never rely on gradients for legibility; text sits on solid overlays.

### Contrast rules
- Minimum contrast 4.5:1 for text; if terminal reports 8-color, fall back to high-contrast flat palette (no gradients, no alpha).
- No-color mode: map to ANSI defaults; remove gradients and icons; keep layout.

## Typography & Layout
- Font assumption: terminal mono; use weight via color/spacing, not bold spam.
- Title caps: Section headers use Title Case; pills use Sentence case.
- Truncation: prefer middle-ellipsis for long paths; never wrap glyph icons alone.

## Spacing & Sizing
- Base unit: 2 chars.
- Spacing scale: 0, 1 (2ch), 2 (4ch), 3 (6ch).
- Border radius: small (1) when width >= 90 cols; square when narrower.
- Card padding: Compact (1,1), Cozy (1,2), Spacious (2,3).

## Density Presets
- Compact: max 1 snippet line, tighter row height; default for 80x24.
- Cozy (default): 2 snippet lines, standard padding.
- Spacious: 3 snippet lines, extra line gap in detail pane.
- Toggle key: `D`. Persist in config; auto-fallback to Compact if cols < 90.

## Iconography
- Glyph set (ASCII-safe fallbacks):
  - agent: `@` (fallback), `󰚩` when supported
  - file/snippet: `` fallback `[]`
  - workspace: `` fallback `/`
  - latency badge: `⏱`
  - cache hit: `●`, miss: `○`
- Icons always paired with text; hidden in no-color or minimal modes.

## Motion
- Default: staggered fade/slide for top 10 results, 90–120 ms, ease-out.
- Disable via `CASS_ANIM=0` or minimal mode; auto-disable if fps < 45 or width < 80.
- Never animate input latency; motion only on render transitions.

## Components (visual rules)
- Filter pills: rounded when wide; square when narrow; active uses gradient; inactive uses `bg-muted`; close/edit glyph on the right.
- Breadcrumb bar: `Agent › Workspace › Date › Ranking`; each crumb uses muted text with active crumb accent underline.
- Results list: alternating muted stripes + 1px role-colored gutter; snippet highlight bold; non-match lines dimmed.
- Drill-in modal: solid backdrop (no blur); bordered panel with role gutters; footer for quick actions.
- Footer HUD: left = help strip; middle = progress/indexer sparkline; right = latency/cache badges.
- Empty states: icon + friendly copy + 3 quick buttons; must fit in 80x24.

## Accessibility & Resilience
- Colorblind support: role gutters pair color with pattern (solid vs dotted vs dashed) in gutters when `CASS_A11Y=1`.
- Mouse optional; all interactions keyboard-first.
- Respect `NO_COLOR` / `CASS_NO_COLOR`; degrade to flat monochrome.
- Headless/robot: suppress gradients, icons, motion; keep structure for tests.

## Performance Guards
- Highlighting: cap at 2 ms per snippet; fallback to plain when exceeded or cols < 100.
- Animation budget: total frame < 16 ms on typical CPU; otherwise auto-disables.
- Cache theme objects for reuse; avoid allocs in draw loop; precompute truncations.

## Opt-out Matrix
- Animations: `CASS_ANIM=0`
- Gradients: `CASS_NO_GRADIENT=1`
- Icons: `CASS_NO_ICONS=1`
- Color: `NO_COLOR=1` or `CASS_NO_COLOR=1`
- A11y patterns: `CASS_A11Y=1`
- Minimal mode (tests/CI): `TUI_HEADLESS=1` or `--once` flows strip embellishments.

## Persistence
- Density, theme (dark/light), a11y, animation toggles, and gradient/icon flags persist in config file under data dir; defaults are safe for 80x24, 8-color.

## Telemetry (local only)
- If enabled, log render timing, highlight fallback counts, animation opt-out events to trace/log; no PII and disabled by default.

## Acceptance checklist (BD-001)
- Spec stored at `docs/tui_style_spec.md`.
- Defines concrete tokens, spacing, motion budgets, opt-out flags.
- Addresses accessibility (contrast + patterns) and low-end terminal fallback.
- Provides guardrails for perf and persistence defaults.
