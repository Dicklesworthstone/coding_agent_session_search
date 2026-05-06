# ftui/ftui-extras Feature Audit for cass

Bead: `coding_agent_session_search-3nwtd.2.1`  
Date: 2026-02-13  
Owner: `GreenMountain`

## Scope and method

This audit compares:

1. What is available in pinned FrankenTUI (`ftui` + `ftui-extras`) at commit `f0ad8a6d9b016ce6a000dc5e1461e1411aee1441`.
2. What cass currently uses in `src/ui/*` and related UI paths.
3. Which high-value capabilities are currently unused.

Primary evidence:

- `Cargo.toml` and `Cargo.lock` (pinned rev + enabled features)
- `crates/ftui-extras/src/*` and `crates/ftui-widgets/src/lib.rs` in the pinned checkout
- cass usage scans in `src/ui/app.rs`, `src/ui/analytics_charts.rs`, `src/ui/style_system.rs`

## Version and enabled features

From `Cargo.toml`, cass enables `ftui-extras` features:

- `markdown`, `syntax`, `charts`, `canvas`, `theme`, `clipboard`, `clipboard-fallback`, `export`, `visual-fx`, `forms`, `validation`, `help`

Pinned to:

- `ftui` `0.1.1` at `f0ad8a6d9b016ce6a000dc5e1461e1411aee1441`
- `ftui-extras` `0.1.1` at same rev

## Capability inventory vs current usage

### ftui-extras feature modules

| Feature | Key public capabilities | cass usage now | Evidence |
|---|---|---|---|
| `charts` | `Sparkline`, `BarChart`, `LineChart`, `heatmap_gradient` | Used heavily in analytics | `src/ui/analytics_charts.rs:17`, `src/ui/analytics_charts.rs:19` |
| `canvas` | `Painter` + `CanvasRef`, modes (`Braille`, `Block`, `HalfBlock`), primitives (`point`, `line`, `rect`, `rect_filled`, `polygon_filled`, `circle`) | Used narrowly for heatmap raster fill | `src/ui/analytics_charts.rs:16`, `src/ui/analytics_charts.rs:1069` |
| `markdown` | `MarkdownRenderer`, `MarkdownTheme`, markdown detection helpers | Used in detail modal rendering | `src/ui/app.rs:141`, `src/ui/style_system.rs:25` |
| `clipboard` | `Clipboard::auto`, selections/backends | Used for copy actions | `src/ui/app.rs:15560` |
| `export` | `HtmlExporter`, `SvgExporter`, `TextExporter` | Used for screenshot export path | `src/ui/app.rs:14565` |
| `visual-fx` | `FxQuality`, `Backdrop`, `Scrim`, `FxLayer`, `StackedFx`, effects (`MetaballsFx`, `PlasmaFx`), canvas adapters | Not used | no `ftui_extras::visual_fx` usage in `src/` |
| `forms` | `Form`, `FormField`, `FormState`, `ConfirmDialog` | Not used | no `ftui_extras::forms` usage in `src/` |
| `validation` | composable validators (`Required`, `MinLength`, `Email`, `ValidatorBuilder`) + async/deadline coordinators | Not used | no `ftui_extras::validation` usage in `src/` |
| `help` | `Tooltip`, `Spotlight`, guided `Tour` state | Not used | no `ftui_extras::help` usage in `src/` |
| `theme` | theme registry + semantic token helpers | Not used directly (cass uses its own style system) | no `ftui_extras::theme` usage in `src/` |
| `syntax` | language tokenizers/highlighter APIs | Not used directly | no `ftui_extras::syntax` usage in `src/` |

### Chart/canvas/FX detail inventory (requested deep scope)

#### Charts (available)

- Chart types: `Sparkline`, `BarChart`, `LineChart`.
- Useful builder knobs currently available:
  - `Sparkline`: `style`, `min`, `max`, `gradient`
  - `BarChart`: direction (`Vertical`/`Horizontal`), mode (`Grouped`/`Stacked`), `bar_width`, `bar_gap`, `group_gap`, `colors`, bounds
  - `LineChart`: bounds, labels, legend toggle, per-series markers

Current cass usage:

- Uses all three chart types, but in static render mode.
- No chart widget-level hit test plumbing for direct click-to-drill; drilldown is list-row based.

#### Canvas (available)

- Modes: `Braille` (2x4), `Block` (2x2), `HalfBlock` (1x2).
- Primitives: point, colored point, line, colored line, rect, filled rect, filled polygon, circle, metaball field rendering helper.

Current cass usage:

- Uses `Painter::for_area` + `point_colored` loops for heatmap block fills.
- Does not use line/circle/polygon/metaball primitives yet.

#### Visual FX (available)

- Core: `FxQuality`, `ThemeInputs`, `BackdropFx`, `FxLayer`, `StackedFx`, scrim blending and quality clamping.
- Exported effects: `MetaballsFx`, `PlasmaFx`, samplers, optional canvas adapters.
- Infrastructure supports graceful degradation alignment with frame budget.

Current cass usage:

- No active usage in app rendering pipeline.
- Existing app has its own stagger reveal hooks but no `ftui_extras::visual_fx` backdrop composition.

## ftui widgets surface: available vs used

`ftui-widgets` exposes 57 widget modules at this pinned rev.  
cass directly imports a small subset in production UI:

- `block`, `borders`, `command_palette`, `help_registry`, `hint_ranker`, `json_view`, `paragraph`, `focus`, `virtualized`, `inspector`

Observation:

- cass already uses advanced core widgets (command palette, hint ranker, virtualized list, inspector).
- There is still unused headroom in core widgets (`table`, `progress`, `spinner`, `modal`, `toast`, etc.) that could simplify some custom rendering logic.

## Prioritized gap analysis

### High value gaps

1. No direct chart interaction bridge (click/select point -> filtered search drilldown)
- Impact: blocks a key goal in Feature B/C.
- Best follow-up: `3nwtd.2.2`, then `3nwtd.3.2`.

2. Canvas is underused for analytics beyond heatmap fill
- Missing: scatter/area/shape-based renderings despite primitive support.
- Best follow-up: `3nwtd.2.4`.

3. `visual-fx` not integrated into surface transitions/loading polish
- Missing: backdrop/scrim-based motion and composited transitions.
- Best follow-up: `3nwtd.4.1`, `3nwtd.4.2`.

### Medium value gaps

1. `syntax` feature is enabled but not directly exercised by cass UI code
- Markdown is rendered, but there is no explicit syntax highlighter integration path in cass-owned code.

2. `help` tours/tooltips are unused
- Could improve onboarding and discoverability of dense keyboard workflows.

3. `forms` + `validation` are unused
- Could standardize interactive config/filter dialogs and reduce hand-rolled validation logic.

### Low value / opportunistic gaps

1. `ftui_extras::theme` is unused directly
- cass has robust custom style system already; migration benefit is incremental, not urgent.

2. Export path could evolve from snapshot-only to richer share/report bundles
- APIs already present (`HtmlExporter`, `SvgExporter`, `TextExporter`) and partially used.

## Recommended execution sequence

1. Land this audit (current bead).
2. Implement interactive chart drilldown (`3nwtd.2.2`) with explicit selection model and query handoff contract.
3. Add one canvas-native analytics panel (`3nwtd.2.4`) as a proof point (scatter + area fill).
4. Integrate `visual-fx` backdrop transitions gated by degradation (`3nwtd.4.1`).
5. Add loading states tied to async boundaries (`3nwtd.4.2`), using spinner/progress where practical.

## Practical implementation notes for next beads

- Keep `analytics_charts.rs` as the chart orchestration layer; route all drilldown intents through typed `CassMsg` events.
- Reuse existing `selection` plumbing already present in analytics views before adding new input modes.
- Wire any visual FX through quality gates (`FxQuality::from_degradation_with_area`) to preserve frame budget guarantees.
- Prefer incremental view-level activation flags so new visuals can be A/B tested without destabilizing all analytics views at once.
