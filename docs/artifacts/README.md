# Preserved Project Artifacts

This directory holds durable evidence bundles that are useful for future
maintenance but are not source code, runtime state, or live issue-tracker data.

- `migration-baseline/` contains the frankensearch/FAD migration baseline used
  by `scripts/migration_e2e_validate.sh`.
- `ftui-parity/` contains reviewed FTUI visual-parity audit evidence, baseline
  bundle notes, and the machine-readable parity manifest. The active scoring
  rubric remains in `docs/ftui_visual_parity_rubric.md` because it is a living
  reference, not a captured run artifact.
- `no-mock-audit.md` is the durable audit narrative for the no-mock CI policy;
  the machine-enforced allowlist lives in `tests/policies/`.
- `refactor-runs/` contains preserved simplification/refactor proof bundles
  from prior skill-loop passes.

Archived proof cards may mention their original capture paths, such as
`refactor/artifacts/...`, because those command transcripts are preserved as
historical evidence. For current references, use the paths under this directory.

Transient local logs, ad-hoc command output, temporary databases, and generated
test results should stay ignored. In particular, `tests/artifacts/` is for
local test/profiling output; move only reviewed, durable evidence here when it
has long-term maintenance value.
