# FTUI Parity Baseline Bundle (Track A / Bead `2dccg.1.2`)

Status: in progress  
Owner: `BronzeCove`  
Primary manifest: `test-results/visual-parity/2026-02-08/manifest.json`

## Objective
Create a canonical, reproducible evidence bundle comparing ratatui `v0.1.64` and current FTUI, then map deficits to concrete remediation tracks/files so no major baseline failure is unowned.

## Artifacts Produced (Revision 1)
Bundle directory:
- `test-results/visual-parity/2026-02-08`

Baseline (ratatui-era captures):
- `test-results/visual-parity/2026-02-08/baseline-ratatui/search_results_main.webp`
- `test-results/visual-parity/2026-02-08/baseline-ratatui/detail_view.webp`
- `test-results/visual-parity/2026-02-08/baseline-ratatui/help_view.webp`

Candidate (current FTUI captures):
- `test-results/visual-parity/2026-02-08/current-ftui/cassapp_results_wide.snap`
- `test-results/visual-parity/2026-02-08/current-ftui/cassapp_results_narrow.snap`
- `test-results/visual-parity/2026-02-08/current-ftui/pty_search_detail_output.raw`
- `test-results/visual-parity/2026-02-08/current-ftui/pty_search_detail_summary.json`

## Deterministic Context
- Candidate commit: `226ef1d1a01bdfa5183a4c70b4003c189ce90560`
- Candidate version string: `cass 0.1.64`
- Example evidence commands:
  - `cargo test --test e2e_tui_smoke_flows tui_pty_search_detail_and_quit_flow -- --nocapture`
  - `cargo test style_token -- --nocapture`

## Canonical Scenario Matrix (Contract vs Current Coverage)
| Scenario | Contract | Coverage in Rev 1 | Artifact Pair |
|---|---|---|---|
| Search results | required | captured | `search_results_main.webp` vs `cassapp_results_wide.snap` |
| Detail/messages tab | required | captured | `detail_view.webp` vs `pty_search_detail_output.raw` |
| Detail/snippets tab | required | pending | missing pair |
| Detail/json tab | required | pending | missing pair |
| Detail/find-open | required | pending | missing pair |

Why partial coverage is still useful now:
- It already provides deterministic evidence for the highest-frequency surfaces (search + messages detail).
- It unblocks root-cause ownership mapping so implementation tracks can proceed without ambiguity.
- Remaining scenario captures are explicitly enumerated and can be added as follow-up in the same bundle format.

## Root-Cause to Workstream Mapping (Owner Map)
This section is authoritative for “what failure maps to what track/code”.

| Failure Signal | Primary File/Surface | Owning Track(s) | Verification Path |
|---|---|---|---|
| Pills/tabs/keyboard legend looked flat/unwired | `src/ui/app.rs`, `src/ui/style_system.rs` token wiring | Track B (`2dccg.2`) | snapshot + unit invariants (`2dccg.6.2`, `2dccg.11.1`) |
| Markdown theme mismatch against selected preset | `src/ui/app.rs` detail markdown renderer path | Track C (`2dccg.3`) | theme regression tests (`2dccg.3.3`) + E2E (`2dccg.11.3`) |
| Detail find bar weak discoverability | `src/ui/app.rs` detail find render path | Track D (`2dccg.4`) | find state tests (`2dccg.4.3`) + snapshot coverage (`2dccg.6.2`) |
| Over-aggressive degradation flattening | degradation policy + capability logic in UI runtime | Track E (`2dccg.5`) | degradation matrix tests (`2dccg.5.3`, `2dccg.6.3`) |
| Minimal fixture realism hiding regressions | test fixture/snapshot surface | Track F (`2dccg.6`) | fixture + snapshot expansion (`2dccg.6.1`, `2dccg.6.2`) |
| Search-surface context/hierarchy loss | search bar/pills/breadcrumbs/footer HUD | Track H (`2dccg.8`) | search regression suite (`2dccg.8.6`) |
| Results-pane scanability/motion/density regressions | results list/snippets/score cues/animations | Track I (`2dccg.9`) | results regression suite (`2dccg.9.5`) |
| Theme/env/capability inconsistency | style-system semantics, env flags, adaptive policies | Track J (`2dccg.10`) | capability diagnostics (`2dccg.10.9`) |
| End-to-end confidence + triage quality | unit/E2E/logging/CI artifacts | Track K (`2dccg.11`) | full verification bundle (`2dccg.11.8`) |

## Coupling Risks (Explicit)
1. Markdown theme coherence and degradation policy are coupled; one can hide or amplify the other.
2. Search/results visual improvements can mask style-token drift unless token-audit tests stay green.
3. Snapshot-only coverage is insufficient without E2E logging context for failure triage.

## Tradeoffs Taken in Rev 1
1. Prioritized deterministic and reproducible captures over broad one-off screenshot collection.
2. Published a partial scenario matrix now to unblock ownership, while explicitly tracking missing captures.
3. Used existing ratatui-era screenshot assets for baseline continuity in this first bundle revision.

## Remaining Work to Fully Complete `2dccg.1.2`
1. Capture snippets-tab side-by-side artifact pair.
2. Capture json-tab side-by-side artifact pair.
3. Capture find-open side-by-side artifact pair.
4. Add Light + HighContrast variants for the scenario set in the same manifest schema.
5. Add final scored deltas using the rubric in `docs/ftui_visual_parity_rubric.md`.

## How Future Agents Should Extend This Bundle
1. Add files under `test-results/visual-parity/<date>/...`.
2. Append scenario entries in `manifest.json` with explicit `status`.
3. Keep scenario IDs stable; do not rename previously published IDs.
4. Update this document’s matrix and residual-work section in the same commit.
