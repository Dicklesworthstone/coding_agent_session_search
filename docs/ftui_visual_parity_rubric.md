# FTUI Visual Parity Rubric (Ratatui v0.1.64 vs Current FTUI)

Status: active  
Owner: cass UI maintainers  
Related bead: `coding_agent_session_search-2dccg.1.1`

## Purpose
Provide a deterministic, reviewer-repeatable rubric so visual quality decisions are evidence-driven, not gut-driven.

## Scope
- Compare **same scenario** across:
  - Baseline: ratatui `v0.1.64`
  - Candidate: current FTUI build under review
- Evaluate **presentation quality**, not backend search relevance.
- Use this rubric for go/no-go on installer unpin and release gate decisions.

## Prerequisites (Deterministic Setup)
Use identical conditions for both baseline and candidate:
- Terminal size: `160x50` (wide) and `80x24` (narrow)
- Theme preset: evaluate at minimum `Dark`, `Light`, `HighContrast`
- Fixture dataset: canonical high-fidelity fixture used by Track F
- Selected hit/state: identical query, selected row, active tab, find mode state
- Environment flags: document `NO_COLOR`, `CASS_NO_COLOR`, `TERM`, and degradation mode

Record metadata for every scored run:
- commit SHA
- terminal emulator/profile
- dimensions
- theme preset
- degradation mode
- fixture ID/hash
- scenario ID

## Scoring Model
- Five categories, each scored `0..5`
- Weighted total out of 100
- Category score contribution = `(raw_score / 5.0) * weight`

### Category Weights
| Category | Weight |
|---|---:|
| Information hierarchy clarity | 25 |
| Affordance discoverability (tabs/pills/find/selection) | 20 |
| Theme cohesion (pane + markdown + accents) | 20 |
| Role differentiation (user/assistant/tool/system) | 15 |
| Density/readability under realistic data | 20 |

## Anchor Definitions (0..5)
Use anchors exactly as written to improve reviewer consistency.

### 1) Information Hierarchy Clarity (Weight 25)
- `0`: Primary vs secondary information is visually collapsed; users cannot identify where to look first.
- `1`: Weak hierarchy; frequent ambiguity between labels, content, and controls.
- `2`: Basic hierarchy exists but breaks under narrow width or dense content.
- `3`: Generally clear hierarchy with occasional ambiguity in stressed views.
- `4`: Strong, consistent hierarchy across normal states and most edge states.
- `5`: Immediate scanability; hierarchy remains clear across width/theme/degradation variants.

### 2) Affordance Discoverability (Weight 20)
- `0`: Interactive controls are visually indistinguishable from plain text.
- `1`: Some controls are visible, many state changes unclear (active/inactive/focus).
- `2`: Core controls discoverable but weak active/focus signaling.
- `3`: Most controls and states discoverable; minor ambiguity remains.
- `4`: Clear active/focus/disabled state language for tabs, pills, find, selection.
- `5`: Discoverability is excellent in both wide and narrow layouts, including degraded modes.

### 3) Theme Cohesion (Weight 20)
- `0`: Major palette mismatch; markdown and chrome visibly conflict.
- `1`: Frequent clashes and inconsistent token usage.
- `2`: Mostly coherent with notable out-of-theme islands.
- `3`: Coherent in core paths, minor inconsistencies remain.
- `4`: Cohesive across panes, markdown, accents, and status cues.
- `5`: Fully coherent and intentional across all tested presets and transitions.

### 4) Role Differentiation (Weight 15)
- `0`: Role distinctions effectively absent.
- `1`: Role differences appear in isolated places only.
- `2`: Role cues present but weak, inconsistent, or hard to scan quickly.
- `3`: Role cues clear in main paths with occasional misses.
- `4`: Reliable role cues across list/detail and mixed conversations.
- `5`: Role differentiation is immediate, redundant (not color-only), and resilient in degraded modes.

### 5) Density/Readability with Realistic Data (Weight 20)
- `0`: Dense content is unreadable; clipping/overflow obscures meaning.
- `1`: Frequent truncation/overlap, poor snippet legibility.
- `2`: Usable but high cognitive load in realistic workloads.
- `3`: Readable with moderate effort; occasional stress-path issues.
- `4`: Strong readability and spacing under realistic mixed-content sessions.
- `5`: High information density without clutter; excellent readability under load.

## Pass Thresholds
For unpin/release decision, all conditions must pass:
- Weighted total score >= `80/100`
- No category raw score below `3`
- HighContrast run weighted total >= `75/100`
- Narrow (`80x24`) run weighted total >= `75/100`

If any threshold fails:
- Release gate is **not** passed
- Open or update remediation beads mapped to failed categories

## Independent Review Protocol
Two reviewers score independently before discussing:
1. Reviewer A scores baseline and candidate.
2. Reviewer B scores baseline and candidate.
3. Compare per-category deltas and rationale text.
4. If any category differs by >=2 points between reviewers:
   - rerun scenario once with shared deterministic metadata
   - reconcile to a final agreed score with explicit rationale notes

## Scoring Worksheet Template
Copy this table per scenario.

| Scenario ID | Build | Hierarchy (25) | Affordance (20) | Theme (20) | Roles (15) | Density (20) | Weighted Total |
|---|---|---:|---:|---:|---:|---:|---:|
| `<id>` | ratatui-v0.1.64 |  |  |  |  |  |  |
| `<id>` | ftui-current |  |  |  |  |  |  |

Rationale notes:
- Hierarchy:
- Affordance:
- Theme:
- Roles:
- Density:

## Worked Example (Representative Scenario)
Scenario ID: `S1-search-detail-find-open-wide`  
Conditions:
- width/height: `160x50`
- theme: `Dark`
- degradation: `Full`
- fixture: `high_fidelity_fixture_v1`
- query: `"theme mapping"`
- detail tab: `Messages`
- find mode: open, current match `2/7`

### Raw Scores
| Category | Weight | ratatui raw | ftui raw |
|---|---:|---:|---:|
| Information hierarchy clarity | 25 | 4 | 2 |
| Affordance discoverability | 20 | 4 | 2 |
| Theme cohesion | 20 | 4 | 1 |
| Role differentiation | 15 | 4 | 2 |
| Density/readability | 20 | 4 | 3 |

### Weighted Totals
- ratatui total:
  - `(4/5)*25 + (4/5)*20 + (4/5)*20 + (4/5)*15 + (4/5)*20`
  - `20 + 16 + 16 + 12 + 16 = 80`
- ftui total:
  - `(2/5)*25 + (2/5)*20 + (1/5)*20 + (2/5)*15 + (3/5)*20`
  - `10 + 8 + 4 + 6 + 12 = 40`

Interpretation:
- Candidate fails release threshold (`40 < 80`) and has category scores below `3`.
- Primary deficits map to:
  - Theme cohesion (markdown/themed rendering mismatch)
  - Affordance discoverability (flat pills/tabs/find cues)
  - Hierarchy clarity (reduced emphasis and structural contrast)

## Mapping Rule (Rubric -> Workstream)
When a category fails, map to owning tracks:
- Hierarchy + Affordance: Tracks B, D, H, I
- Theme cohesion: Tracks C, J
- Role differentiation: Track B
- Density/readability: Tracks F, I
- Cross-theme/degradation stability: Tracks E, F, K

## Change Control
If rubric weights or anchors change:
- Update this document in the same PR
- Include rationale and expected effect on pass/fail decisions
- Re-score at least one representative scenario to show impact

## 2dccg.1.2 Canonical Scenario Matrix
This section is the authoritative baseline matrix for bead `coding_agent_session_search-2dccg.1.2`.
Use it as the source of truth for parity evidence and root-cause ownership.

| Scenario ID | Required state | Baseline artifact (ratatui v0.1.64) | Candidate artifact (current ftui) | Status | Owner beads for remaining gaps |
|---|---|---|---|---|---|
| `S1-search-results-wide` | Query submitted, wide dual-pane layout, selected hit visible | `docs/assets/screenshots/screenshot_01.webp` | `tests/snapshots/cassapp_search_surface_breakpoint_medium.snap` | captured | n/a |
| `S2-detail-messages-tab` | Detail modal open on Messages tab | `docs/assets/screenshots/screenshot_02.webp` | `test-results/e2e/tui/tui-19c3f31b889_pty_search_detail_output.raw` | captured | n/a |
| `S3-detail-snippets-tab` | Detail modal open on Snippets tab | `docs/assets/screenshots/screenshot_02.webp` | `tests/snapshots/cassapp_baseline_detail_tabs_snippets_active.snap` | captured | n/a |
| `S4-detail-json-tab` | Detail modal open on Json tab | `docs/assets/screenshots/screenshot_02.webp` | `tests/snapshots/cassapp_baseline_detail_tabs_json_active.snap` | captured | n/a |
| `S5-detail-find-bar-open` | Detail modal with find bar active + match counter | `docs/assets/screenshots/screenshot_02.webp` | `tests/snapshots/cassapp_baseline_detail_find_current_match.snap` | captured | n/a |
| `S6-search-results-narrow` | Query submitted, narrow single-pane layout | `docs/assets/screenshots/screenshot_01.webp` | `tests/snapshots/cassapp_search_surface_breakpoint_narrow.snap` | captured | n/a |
| `S7-command-palette-open` | Palette open over search/detail surfaces | `docs/assets/screenshots/screenshot_03.webp` | `tests/snapshots/cassapp_command_palette.snap` | captured | n/a |

Canonical machine-readable manifest:
- `docs/artifacts/ftui-parity/visual_parity_manifest.json`

## Regeneration Commands (Deterministic)
Run from repo root:

```bash
cargo test --test e2e_tui_smoke_flows tui_pty_search_detail_and_quit_flow -- --nocapture
cargo test --test e2e_tui_smoke_flows tui_pty_help_overlay_open_close_flow -- --nocapture
cargo test --test e2e_tui_smoke_flows tui_pty_launch_quit_and_terminal_cleanup -- --nocapture
```

These commands refresh deterministic PTY artifacts under:
- `test-results/e2e/tui/`

Current validated traces in this bundle:
- `tui-19c3f31b889` (search -> detail)
- `tui-19c3f31d8d0` (help overlay open/close)
- `tui-19c3f320444` (launch/quit lifecycle)

## Root-Cause To Workstream Mapping (Canonical)
| Failed dimension / symptom | Evidence surface | Concrete code references | Owning beads / tracks | Ownership status |
|---|---|---|---|---|
| Theme cohesion mismatch between pane chrome and markdown | Rubric Theme category failures in representative scenario | `src/ui/app.rs:4337`, `src/ui/app.rs:4410` | Track C: `coding_agent_session_search-2dccg.3.1`, `coding_agent_session_search-2dccg.3.2`, `coding_agent_session_search-2dccg.3.3` | owned |
| Missing role differentiation in message gutters (legacy symptom) | Role category score deltas vs ratatui | `src/ui/app.rs:4258`, `src/ui/app.rs:4259`, `src/ui/app.rs:4260`, `src/ui/app.rs:4261` | Track B complete + Track K verify: `coding_agent_session_search-2dccg.2`, `coding_agent_session_search-2dccg.11.1` | owned |
| Flat affordances for tabs/pills/key hints (legacy symptom) | Affordance category deficits | `src/ui/app.rs:4820`, `src/ui/app.rs:4821`, `src/ui/app.rs:11789` | Track B complete + Track H/I hardening: `coding_agent_session_search-2dccg.2`, `coding_agent_session_search-2dccg.8`, `coding_agent_session_search-2dccg.9` | owned |
| Detail find bar still lacks final polished container/focus treatment | Find mode visual ambiguity | `src/ui/app.rs:4947`, `src/ui/app.rs:4919` | Track D: `coding_agent_session_search-2dccg.4.1`, `coding_agent_session_search-2dccg.4.2`, `coding_agent_session_search-2dccg.4.3` | owned |
| Potential over-degradation / policy cliffs under capability pressure | Hierarchy drop-off under degraded conditions | `src/ui/app.rs:12646`, `src/ui/app.rs:11594`, `src/ui/app.rs:11604` | Track E + J: `coding_agent_session_search-2dccg.5.1`, `coding_agent_session_search-2dccg.5.2`, `coding_agent_session_search-2dccg.10.4`, `coding_agent_session_search-2dccg.10.6` | owned |
| Fixture realism too thin for parity-grade snapshot confidence | Snapshot diffs miss real-world complexity | `tests/ftui_harness_snapshots.rs:21`, `tests/ftui_harness_snapshots.rs:32` | Track F + K: `coding_agent_session_search-2dccg.6.1`, `coding_agent_session_search-2dccg.6.2`, `coding_agent_session_search-2dccg.11.3` | owned |
| README/install path still pinned to ratatui release while parity gate remains open | Install defaults and screenshots lag behind ftui state | `README.md:16`, `README.md:22`, `README.md:25` | Track G: `coding_agent_session_search-2dccg.7.1`, `coding_agent_session_search-2dccg.7.3`, `coding_agent_session_search-2dccg.7.5` | owned |

No major baseline failure is left unowned. Scenario captures `S1`-`S7` are now materialized with deterministic baseline/candidate artifact mapping.

## Cross-Track Coupling Risks
| Coupling | Why it matters | Coordinated owners |
|---|---|---|
| Markdown theming x degradation policy | Theme fixes can regress readability under low-style modes | Track C + Track E + Track J |
| Find-bar polish x narrow breakpoint behavior | Added chrome can collide with tight-height detail layouts | Track D + Track I + Track F |
| Semantic token wiring x env overrides (`NO_COLOR`, TERM capability) | Correct token use can still look wrong if override precedence is inconsistent | Track B + Track J |
| Screenshot refresh x release gate | Visual updates must align with verification bundle to avoid stale evidence | Track G + Track K |

## Decision Log (For Future Sessions)
- Keep legacy TUI screenshot captures in `docs/assets/screenshots/*.webp` as ratatui baseline anchors until dedicated ratatui replay capture is added.
- Use deterministic snapshot + PTY raw artifacts for current ftui evidence to avoid subjective comparisons.
- Keep scenario IDs stable so future evidence bundles and release reports can diff by scenario key.
- Treat any newly introduced scenario captures as explicit backlog work, never implicit TODOs.

## Prioritized Handoff (Post-1.2)
1. Complete markdown theme parity hardening in Track C (`coding_agent_session_search-2dccg.3.1` to `coding_agent_session_search-2dccg.3.3`).
2. Execute capability/degradation hardening in Track E/J to prevent environment-specific regressions.
3. Feed captured artifacts into Track K evidence bundle (`coding_agent_session_search-2dccg.11.8`) before release unpin decisions.

## 2dccg.1.5 Ratatui -> FTUI Capability Crosswalk
Status legend:
- `kept`: behavior parity retained
- `improved`: parity retained plus higher quality/usability
- `replaced`: intentional behavior change (must document rationale)
- `at_risk`: partially implemented or missing deterministic evidence

| Capability | Ratatui v0.1.64 expectation | FTUI status | Evidence anchors | Owning beads |
|---|---|---|---|---|
| Search input cues + editing behavior | Query entry/editing is always discoverable and Enter submits deterministic search | improved | `src/ui/app.rs:14616`, `src/ui/app.rs:14877`, `tests/snapshots/cassapp_results_wide.snap` | `coding_agent_session_search-2dccg.8.1`, `coding_agent_session_search-2dccg.11.2` |
| Filter pills (state, affordance, click targets) | Pills clearly indicate active filters and support interaction | improved | `src/ui/app.rs:4030`, `src/ui/app.rs:4077`, `src/ui/app.rs:17071`, `src/ui/app.rs:17093`, `src/ui/app.rs:4877`, `src/ui/app.rs:20964` | `coding_agent_session_search-2dccg.8.2`, `coding_agent_session_search-2dccg.8.3`, `coding_agent_session_search-2dccg.6.2`, `coding_agent_session_search-2xg36` |
| Result list hierarchy (score/source/snippet) | Fast scan of score + provenance + snippet quality | improved | `tests/snapshots/cassapp_search_surface_breakpoint_medium.snap`, `tests/snapshots/cassapp_search_surface_breakpoint_narrow.snap`, `tests/snapshots/cassapp_search_surface_structure_default.snap`, `src/ui/app.rs:17375`, `src/ui/app.rs:17696`, `src/ui/app.rs:28270` | `coding_agent_session_search-2dccg.9.1`, `coding_agent_session_search-2dccg.9.2`, `coding_agent_session_search-2dccg.9.3`, `coding_agent_session_search-2dccg.9.5`, `coding_agent_session_search-m050g` |
| Detail tabs + active-state clarity | Messages/Snippets/Raw/Json tabs remain explicit and keyboard navigable | improved | `src/ui/app.rs:4820`, `src/ui/app.rs:15549`, `src/ui/app.rs:15787` | `coding_agent_session_search-2dccg.2`, `coding_agent_session_search-2dccg.8.6`, `coding_agent_session_search-2dccg.11.2` |
| Detail find bar + match counters + key flows | `/`, `n`, `N`, `Esc` flows remain reliable with visible match state | improved | `src/ui/app.rs:2870`, `src/ui/app.rs:17801`, `src/ui/app.rs:17934`, `tests/snapshots/cassapp_baseline_detail_find_current_match.snap` | `coding_agent_session_search-2dccg.4.1`, `coding_agent_session_search-2dccg.4.2`, `coding_agent_session_search-2dccg.4.3`, `coding_agent_session_search-1dkp4` |
| Role differentiation + metadata readability | User/assistant/tool/system remain visually distinct with readable metadata | improved | `src/ui/app.rs:4256`, `src/ui/app.rs:4267`, `src/ui/app.rs:15860` | `coding_agent_session_search-2dccg.2`, `coding_agent_session_search-2dccg.9.3`, `coding_agent_session_search-2dccg.11.1` |
| Footer HUD/status/degradation diagnostics | Status footer communicates mode, density, degradation, and guidance | improved | `src/ui/app.rs:3661`, `src/ui/app.rs:16317`, `src/ui/app.rs:16753` | `coding_agent_session_search-2dccg.8.5`, `coding_agent_session_search-2dccg.10.9`, `coding_agent_session_search-2dccg.11.6` |
| Keyboard + mouse navigation affordances | Full keyboard coverage with mouse parity for common actions | improved | `src/ui/app.rs:19959`, `src/ui/app.rs:17144`, `src/ui/app.rs:17219`, `src/ui/app.rs:17841` | `coding_agent_session_search-2dccg.11.2`, `coding_agent_session_search-2dccg.9.5` |

### Intentional Replacements
- None declared yet. Any future replacement must include rationale + migration notes in this section before release gate.

## 2dccg.1.5 Non-Regression Checklist (Release Consumption)
Use this checklist as the release-gate crosswalk consumed by `coding_agent_session_search-2dccg.11.8` and `coding_agent_session_search-2dccg.7.4`.

| Check ID | Capability guardrail | Pass criteria | Current state | Evidence anchors | Owning beads |
|---|---|---|---|---|---|
| `NR-01` | Search input editing/submit | Enter from query mode triggers deterministic search dispatch; empty query safely no-op | pass | `src/ui/app.rs:14616`, `src/ui/app.rs:14877` | `coding_agent_session_search-2dccg.8.1`, `coding_agent_session_search-2dccg.11.2` |
| `NR-02` | Filter-pill interaction fidelity | Left-click enters edit mode, right-click clears target filter, pills remain visible at medium/wide widths | pass | `src/ui/app.rs:17071`, `src/ui/app.rs:17093`, `src/ui/app.rs:4077`, `src/ui/app.rs:4877`, `src/ui/app.rs:20964` | `coding_agent_session_search-2dccg.8.2`, `coding_agent_session_search-2dccg.8.3`, `coding_agent_session_search-2dccg.6.2`, `coding_agent_session_search-2xg36` |
| `NR-03` | Results hierarchy scanability | Score/source/snippet cues remain legible across wide+narrow and no key metadata silently disappears | pass | `tests/snapshots/cassapp_search_surface_breakpoint_medium.snap`, `tests/snapshots/cassapp_search_surface_breakpoint_narrow.snap`, `tests/snapshots/cassapp_search_surface_structure_default.snap`, `src/ui/app.rs:17375`, `src/ui/app.rs:17696`, `src/ui/app.rs:28270` | `coding_agent_session_search-2dccg.9.1`, `coding_agent_session_search-2dccg.9.2`, `coding_agent_session_search-2dccg.9.3`, `coding_agent_session_search-m050g` |
| `NR-04` | Detail-tab navigation parity | Tab cycling traverses Messages -> Snippets -> Raw -> Json -> Messages with visible active-state cues | pass | `src/ui/app.rs:15549`, `src/ui/app.rs:4820`, `src/ui/app.rs:15787` | `coding_agent_session_search-2dccg.2`, `coding_agent_session_search-2dccg.8.6` |
| `NR-05` | Detail-find behavior + visibility | `/` opens find, `Esc` closes find before detail modal, match counters/highlighting are visible and correct | pass | `src/ui/app.rs:2870`, `src/ui/app.rs:17801`, `src/ui/app.rs:17934`, `tests/snapshots/cassapp_baseline_detail_find_current_match.snap` | `coding_agent_session_search-2dccg.4.1`, `coding_agent_session_search-2dccg.4.2`, `coding_agent_session_search-2dccg.4.3`, `coding_agent_session_search-1dkp4` |
| `NR-06` | Role differentiation | Role gutter/prefix and metadata remain distinct for all roles in mixed conversations | pass | `src/ui/app.rs:4256`, `src/ui/app.rs:4267`, `src/ui/app.rs:15860` | `coding_agent_session_search-2dccg.2`, `coding_agent_session_search-2dccg.11.1` |
| `NR-07` | Footer diagnostics | Footer reflects mode/rank/context/degradation without truncating critical state in narrow layouts | pass | `src/ui/app.rs:3661`, `src/ui/app.rs:16317`, `src/ui/app.rs:16753` | `coding_agent_session_search-2dccg.8.5`, `coding_agent_session_search-2dccg.10.9`, `coding_agent_session_search-2dccg.11.6` |
| `NR-08` | Keyboard/mouse parity | Keyboard focus graph remains valid and mouse hit-regions keep expected behaviors (select/open/scroll/split drag) | pass | `src/ui/app.rs:19959`, `src/ui/app.rs:17144`, `src/ui/app.rs:17219`, `src/ui/app.rs:17282` | `coding_agent_session_search-2dccg.11.2`, `coding_agent_session_search-2dccg.9.5` |

Gate policy:
- `NR-01`, `NR-04`, `NR-06`, `NR-07`, and `NR-08` are required-pass before release unpin.
- `NR-02` verification refresh (2026-02-11): filter-pill affordance/state tests pass after `coding_agent_session_search-2xg36` closure.
- `NR-03` verification refresh (2026-02-11): `snapshot_search_surface_breakpoint_matrix` and `snapshot_search_surface_*` pass on current tree state.
