# CASS Large Seeded Rebuild: Footprint-Aware Shard Planning

Workload:

```bash
timeout 140s env CASS_RESPONSIVENESS_DISABLE=1 CASS_PREP_PROFILE=1 \
  cass index --watch-once /home/ubuntu/cass-large-rebuild-nextpass-missing-*.jsonl \
  --data-dir <fresh-reflinked-21GB-db-copy> \
  --json --progress-interval-ms 5000 --color=never
```

Baseline (`baseline.*`, debug binary from `/data/tmp/cargo-target/debug/cass`):

- Exit: `124` timeout after `144.30s`.
- Progress at timeout: `38,994 / 51,214` conversations.
- Peak RSS: `38,895,820 KB`.
- Shard planning: `944ms`, ID-only shard sizing.

Kept change (`footprint-plan-valid.*`, profiling binary from `/data/tmp/cass-target-nextpass-20260503/profiling/cass`):

- Exit: `0`.
- Completed: `51,214` conversations / `4,711,566` messages.
- CLI elapsed: `119,559ms`; `/usr/bin/time` elapsed: `2:00.69`.
- Peak RSS: `54,422,676 KB`.
- Shard planning: `45,351ms`.

Interpretation:

- The ID-only shard plan under-partitioned message-heavy regions and missed the 140s cap.
- The new plan uses existing per-conversation message-count footprints to size shard boundaries, then marks per-shard message counts as unknown because those footprints are sizing estimates, not validation contracts.
- Wall time now fits under the cap, but memory and planning time are still the next targets: the aggregate footprint scan costs ~45s and peak RSS rises to ~54GB while shard build/merge workers drain the deeper frontier.
- Binary profiles differ between the baseline and kept run, so treat this as a cap/progress proof rather than a clean microbenchmark. The workload-level result still moved from timeout to successful completion under the same 140s cap.

Rejected intermediate:

- `footprint-plan.*` used footprint message counts as validation upper bounds.
- It reached `51,065 / 51,214` conversations quickly but failed with `indexed_docs > planned source messages` on shard 273.
- The final change keeps footprint-based boundaries while preserving exact rebuild accounting as the validation authority.
