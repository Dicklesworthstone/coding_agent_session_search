# Golden File Provenance

Golden files under this directory freeze known-good outputs from cass
subcommands. Each file should be human-reviewed before commit; any diff
from a golden in CI is either a bug or an intentional schema change
that requires re-approval.

## Regeneration

```bash
# Regenerate every golden
UPDATE_GOLDENS=1 cargo test --test golden_robot_json

# Regenerate a specific test
UPDATE_GOLDENS=1 cargo test --test golden_robot_json -- capabilities_json

# After regeneration, review and commit
git diff tests/golden/
git add tests/golden/
git commit -m "Update golden <name>: <why>"
```

## Scrubbing

Dynamic values are scrubbed before golden comparison — see
`tests/golden_robot_json.rs::scrub_robot_json` for the rule set:

| Token | Replacement | Reason |
|---|---|---|
| `"crate_version": "x.y.z"` | `"[VERSION]"` | Survives `cargo publish` bumps |
| ISO-8601 timestamps | `[TIMESTAMP]` | Non-deterministic |
| Paths rooted at the isolated test HOME | `[TEST_HOME]` | Test-dir specific |
| UUIDs | `[UUID]` | Non-deterministic |

Keys are also sorted and the payload is re-indented by
`serde_json::to_string_pretty` so whitespace / key-order drift is not
treated as shape drift.

## Files

### `robot/capabilities.json.golden`

Frozen output of `cass capabilities --json`. Captures the LLM-facing
contract surface: `api_version`, `contract_version`, `features` list,
`connectors` list, and `limits` block. `crate_version` is scrubbed.

**Generated from:** cass @ commit HEAD of the authoring commit (see
`git log tests/golden/robot/capabilities.json.golden`).
**Command:** `cass capabilities --json` with `XDG_DATA_HOME` pinned to
an isolated TempDir and `CASS_IGNORE_SOURCES_CONFIG=1` so no ambient
sources leak into the output.

## Follow-ups

Goldens still to add under bead u9osp scope (each needs its own
environment-scoped fixture — e.g. a TempDir with a known-empty or
known-seeded data dir before the command can produce deterministic
output):

- `robot/health.json.golden` — needs a seeded data dir with a stable
  fixture index so the `last_indexed_at` / counts are scrubbable.
- `robot/models_status.json.golden` — needs a pinned `CASS_DATA_DIR`
  with a known model-cache state (empty vs installed) so `state`,
  `next_step`, and per-file `size_match` are deterministic.
- `robot/robot_docs.json.golden` — needs a topic-specific fixture
  since the output is a large formatted doc string per topic.
