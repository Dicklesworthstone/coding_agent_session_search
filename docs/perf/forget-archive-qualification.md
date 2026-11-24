# GH565 large archive forget qualification

This harness qualifies the real current CASS binary on newly generated synthetic
archives. It measures the two workloads from [issue 565](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/565):
a single tiny conversation followed by full repair, and a directory containing
1,064 conversations, 10,839 messages and approximately 6.7 MB of message text.
It preserves the existing GH544 serial versus batch benchmark.

The inspected application baseline is `710435869716073a3b5aa015ad3f9872426e2e4a`. The relevant
implementation is already on main, including `f8956006`, `bcbb3d24` and
`17cddb59`. The harness preserves those bounded deletion and repair paths.
Applied forget also has the narrowly scoped WAL-index recovery described below;
the benchmark's observation probes remain strictly read-only.

## Evidence status

The delivered validation record distinguishes Python harness checks from CASS
execution. No representative native CASS run, measured CASS peak RSS, or
application throughput result is supplied by this implementation session.
The native verifier has syntax/format validation but has not been built against
the complete CASS dependency graph in this environment. Run the repository's
native quality gate and a native smoke case before launching the large matrix.

Never promote `plan_only`, `incomplete`, a skipped test, a setup refusal, an
OOM-killed run, or an uncompiled helper into a qualification pass.

## Source contracts reviewed

The implementation follows the existing `gh565-forget-memory` Bead, the storage
and benchmark sections of `docs/planning/PLAN_TO_MAKE_CODING_AGENT_SESSION_SEARCH.md`,
and the archive-scale acceptance in
`docs/planning/REALITY_CHECK_AND_BRIDGE_PLAN_2026-09-01.md`. Concrete APIs come
from `tests/cli_forget.rs`, `tests/connector_codex.rs`,
`scripts/benchmark_forget.py`, `examples/lexical_admission_fixture.rs`,
`src/storage/sqlite.rs`, `src/search/privacy.rs`, and the strict read-only
opener in `src/logical_archive/export.rs`.

Ordinary indexing defers analytics and token-ledger writes. Seed ingestion uses
the existing `CASS_INLINE_ANALYTICS_UPDATES=1` opt-in, then runs the public
analytics rebuild for both tracks and checks one token-ledger row per message
plus the derived totals. The measured forget environment has no inline-analytics
override. The benchmark never clears a marker,
relaxes a reader fence, injects SQL, disables an assertion, or defers repair.

Token-rollup repair uses the existing conversation index to obtain each
conversation's minimum and maximum ledger ID and exact row count. Its ordered
rowid pages start at that minimum and request at most 128 remaining matching
rows. The final page stops at the known row count instead of scanning the
unrelated archive tail to discover that no matches remain. Page commits, model
ordering, cancellation, and the pending privacy fence retain their existing
contracts; no new index or migration is required.

This removes repeated unrelated prefix/tail traversal without claiming a
general linear-time bound: interleaved ledger IDs can still require scanning
unrelated rows inside a conversation's ID span. The native query-plan and
correctness regressions must run through the normal gate, and the qualification
matrix must still measure wall time and RSS. No native speedup is established
by the source change alone.

## Synthetic profiles

| Profile | Conversations | Messages | Message text bytes | Batch deletion |
| --- | ---: | ---: | ---: | --- |
| Smoke | 40 | 600 | 600,000 | 8 conversations / 80 messages / 50,000 bytes |
| Small | 3,095 | 214,759 | 865,025,684 | 1,064 / 10,839 / 6,700,000 bytes |
| Representative | 21,363 | 2,050,000 | 8,589,934,592 | 1,064 / 10,839 / 6,700,000 bytes |

Every profile also has a separate one-conversation cohort with four messages and
2,472 content bytes. Each case starts from its own copy of the same indexed seed.
The small profile scales the retained cohort to about one tenth and leaves both
deletion workloads unchanged. This avoids making deletion volume a confounder
and keeps typical retained-conversation sizes comparable.

The reporter did not provide a conversation histogram or total message-text byte
count. The long tail and 8 GiB of text are explicit synthetic assumptions, not
reconstructions of private history. Counts and UTF-8 content bytes are exact.
At seed 565, representative conversation message counts have median 14,
90th percentile 207, 99th percentile 1,298 and maximum 5,941. The largest
conversation holds about 30.9 MB of content. Selected messages remain small,
matching the reported deletion cohort. Body sizes vary within conversations;
dates span a year, workspaces vary, and real-format token-count events populate
the token ledger. Every source contains only synthetic text.

Canonical database size is measured after real indexing. The representative
target is **11,300,000,000 bytes**, with a default 20% tolerance. Outside that
range the matrix remains incomplete even if the operation succeeds. Adjust
`--content-bytes representative=BYTES` and repeat in a new output directory if
calibration is needed. The full data-directory byte size is recorded separately;
the reported 38.6 GB directory is context, not an asserted synthetic match.

Generation streams one message at a time. It retains conversation descriptors,
not the whole corpus. `--oversized-message-bytes 9MiB` can add an explicitly
labeled large retained row without changing total content bytes. This optional
stress variant should be reported separately from the standard profile.

## Files

* `scripts/benchmark_forget.py qualify`: entry point; legacy invocation stays intact.
* `scripts/forget_archive_fixture.py`: exact plans and streaming Codex JSONL generation.
* `scripts/forget_benchmark_process.py`: Linux process and cgroup telemetry.
* `scripts/forget_archive_qualification.py`: matrix, verification, recovery and reports.
* `examples/forget_qualification_probe.rs`: strict read-only FrankenSQLite oracle.
* Four new Python test files under `scripts/tests/` exercise the harness.

Python 3.11 or newer is required. Runtime dependencies are Python's standard
library, GNU `cp`, the real CASS executable, and the verifier built from the same
source. A Linux kernel exposing cgroup v2 `memory.peak` and `cgroup.kill` is
required for capped trials. No model downloads are performed; semantic checks
use the built-in hash embedder.

## Build and bind the executable

Use the project's authorized DSR execution route. GitHub Actions is not a build
or benchmark execution path. Within that job, build both artifacts together:

```bash
cargo build --locked --profile release-perf --bin cass --example forget_qualification_probe
```

Keep the normal project quality gates, including the complete forget regressions,
the GH565 library cases, all-target Clippy, formatting and strict UBS. The helper
also has a typed-digest framing regression:

```bash
cargo test --locked --example forget_qualification_probe
python3 -m unittest discover -s scripts/tests -p 'test_*forget*.py' -v
```

Record a build receipt from the actual built files and the actual build commit
of a clean checkout containing this harness and its probe. The inspected
application baseline above is historical context; it is not a default build
revision. Native runs require `--source-revision` explicitly. Capture the build
revision in the build checkout, and preserve it with the artifacts:

```bash
CASS_BENCH_REVISION="$(git rev-parse HEAD)"
```

The qualification driver requires the receipt's executable SHA-256 values and
actual source revision to match:

```json
{
  "source_revision": "ACTUAL_FULL_BUILD_COMMIT_SHA",
  "cass_sha256": "SHA256_OF_ACTUAL_CASS_EXECUTABLE",
  "probe_sha256": "SHA256_OF_ACTUAL_PROBE_EXECUTABLE"
}
```

These placeholders are not accepted as hashes. Preserve the original DSR build
receipt and toolchain/profile/features information alongside this compact file.
Matching a supplied receipt establishes a file binding; it is not independent
attestation of the builder. The driver also checks both binary hashes at the end.
Native smoke permits an operator-asserted binding and reports it as such.

## Review a plan without an application binary

```bash
python3 scripts/benchmark_forget.py qualify \
  --plan-only --output-dir /scratch/gh565-plans
```

This writes exact plans and hashes for small and representative profiles. It
does not generate multi-GB sources, index CASS, or measure application performance.
The parent output directory must exist; the output directory itself must not.

## Native smoke

First use a newly created synthetic smoke archive:

```bash
python3 scripts/benchmark_forget.py qualify \
  --cass /build/target/release-perf/cass \
  --probe /build/target/release-perf/examples/forget_qualification_probe \
  --source-revision "$CASS_BENCH_REVISION" \
  --build-description 'DSR receipt path, pinned toolchain, release-perf, features' \
  --profile smoke --allow-uncapped \
  --output-dir /scratch/gh565-native-smoke
```

Uncapped execution is accepted only for the explicit smoke profile. A fully
successful smoke run returns `smoke_passed` and exit 0; it cannot produce
`qualified_synthetic_profiles`. Smoke defaults to analytics and lexical
interruption checks because eight small deletions fit one atomic batch and
cannot exercise a committed partial batch. Small correctness success is not an
archive-scale memory qualification. An explicitly requested canonical-delete
interruption can remain inconclusive; the report never invents a partial batch.

## Capped qualification matrix

Ask the runner administrator to provide a **delegated cgroup v2 parent** with
the memory controller already enabled below it. The harness never enables a
controller, changes a parent limit, uses sudo, or creates persistent services.
It creates one fresh child per command, sets and reads back `memory.max`,
`memory.swap.max=0`, and `memory.oom.group=1`, then attaches the child before
executing the binary. It refuses ordinary directories and missing controllers.

Example using two limits; replace the delegated parent and artifact paths:

```bash
python3 scripts/benchmark_forget.py qualify \
  --cass /build/target/release-perf/cass \
  --probe /build/target/release-perf/examples/forget_qualification_probe \
  --source-revision "$CASS_BENCH_REVISION" \
  --build-description 'DSR receipt path, pinned toolchain, release-perf, features' \
  --build-receipt /build/gh565-build.json \
  --cgroup-parent /sys/fs/cgroup/authorized-gh565 \
  --memory-max 8GiB --memory-max 40GiB \
  --verification-memory-max 40GiB \
  --profile small --profile representative \
  --repetitions 2 --timeout 14400 --quick-check \
  --output-dir /scratch/gh565-qualification
```

`40G` means 40 GiB, matching systemd-style binary units; `40GB` means decimal
40 billion bytes. `--memory-max 76GiB` can reproduce the reporter's larger cap.
The host must have adequate physical resources and a suitable parent limit.
The harness records requested limits, read-back values, peaks and OOM counters;
it does not reserve that memory or promise host capacity.

Setup, canonical probes, search checks and rescans use separate command cgroups
at `--verification-memory-max` (default: the largest trial cap). Their time/RSS
does not enter the forget measurement. This permits a restrictive deletion cap
without accidentally interpreting a verifier allocation failure as deletion RSS.
Recovery runs use the highest requested trial cap and retain separate measurements.

All seeds, successful copies, interrupted copies, logs and cgroups are retained.
No archive cleanup is automatic. GNU `cp --reflink=auto` uses independent CoW
copies when supported, and a full copy otherwise. It never hardlinks databases.
Budget hundreds of GB for a complete matrix on a non-reflink filesystem; exact
space depends on resulting index size, retained generations and repetition/cap
counts. The harness checks free space before generation and each trial but
cannot predict all future storage amplification. Run on a dedicated scratch
volume, then use the organization's normal reviewed cleanup process.

## Verification and recovery

Each copied seed first runs a recorded ordinary
`index --rebuild-profile bounded --json --no-progress-events` setup command.
This admits the new database file identity through CASS's normal writable
archive admission; copied namespace sidecars refer to the original inode and
cannot be silently repaired through the strict oracle. The driver then requires
canonical totals, every retained-row digest, analytics summaries and tombstones
to match the seed, with no pending repair. Any drift aborts before forgetting.
This preparation uses the verification cap and is excluded from forget timing.

Immediately after copied-seed admission, the measured forget or recovery, and
the full rescan, a recorded `status --json` check must establish a current,
complete lexical publication. This check runs before canonical scans or search
witnesses. It requires a ready, fresh index; the current MANIFEST; no stale,
partial, hollow or rebuilding state; and a completed checkpoint matching the
canonical database fingerprint, schema and compatible page-size profile. A
bounded profile can be compatible without matching the default page size.

The MANIFEST's `live_documents` must equal the exact remaining message count.
That equality is specific to this fixture's distinct, nonempty ordinary
user/assistant messages, each of which should produce one lexical document.
It is publication metadata, not a complete audit of indexed bodies, so full
canonical digests and retained search witnesses remain independent checks.
Missing authority, a previous-MANIFEST fallback or a stale publication fails
validation. Optional status sections may be omitted under the command's own
budget only when all required publication evidence remains present and valid;
process timeouts, OOMs and failed command receipts still fail.

Every verification search uses `--no-maintenance`, including pending-repair
privacy checks. Search cannot repair a failed publication after the timed
operation and turn that failure into a successful qualification. Status and
search receipts remain outside the forget measurement and are retained in the
trial report.

Every ordinary trial requires all of the following:

1. The indexed seed contains exactly the synthetic paths, message counts and
   content bytes, populated analytics/token tables, and no pending repair.
2. A lexical positive control proves the selected marker was searchable. The
   dry-run selector must match the exact cohort and message count.
3. Real `forget --apply --json` completes, reports the expected deletion and
   bounded lexical profile, emits every repair phase, and leaves a complete
   current lexical publication before verification begins.
4. A new read-only native process hashes every remaining canonical conversation,
   every field of every remaining message, and all retained token-ledger rows.
   Exact counts and digests must preserve all unselected history, including IDs.
5. Source-file tombstones cover exactly the deleted Codex files. Shared-store
   session tombstones remain unchanged. Reopening and a zero-match retry must
   preserve the tombstones and perform no further rebuild.
6. SQL and filesystem pending fences both clear. Analytics message totals and
   token rollups agree with the canonical rows.
7. Lexical marker search returns no forgotten hit. Hybrid, semantic and fast
   semantic probes expose no selected source or forgotten marker. A typed
   semantic-unavailable refusal is reported separately, not as a served empty
   result. Timeouts, missing hits arrays and unexpected errors fail validation.
8. A deterministic sample of retained search witnesses remains retrievable;
   complete unrelated-content preservation is established by the full canonical
   digests, not by this bounded search sample.
9. A real full rescan, while original transcripts remain present and unchanged,
   does not resurrect anything and keeps tombstones/pending state correct.

The fixture intentionally does not claim mixed-provider shared-store coverage,
native MiniLM relevance, retained reader/TUI lifetimes, or every foreign-key
constraint. Those existing regression/privacy gates remain necessary. The helper
checks missing conversation parents and optional physical `quick_check`; these
are accurately named checks rather than a universal database-integrity claim.

The recovery matrix sends an actual SIGKILL after observed `canonical-delete`,
`analytics`, and `lexical-index` stderr phases. Before post-interruption probes
or recovery, it preserves an independent archive copy, including database
sidecars, using the same checked reflink-or-copy policy. It reads the durable
interrupted state and requires pending readers to return `forget-repair-pending`. Partial
canonical deletion resumes with the same selectors. An interruption after
lexical rebuilding begins also tests `index --full` as the recovery route when
all canonical deletion has committed. Normal verification then runs again.

Recovery admission requires a clean requested-SIGKILL receipt and proof that
the owned process containment has stopped. A capped command must reach
`cgroup.events` `populated=0`, which includes nested child cgroups; an uncapped
smoke command proves only that its original process group has no live members.
Root exit and closed output pipes alone are insufficient. The observer records
the containment scope and cleanup result, and reads final cgroup memory counters
only after proving quiescence. The root's measured wall time excludes this
separate cleanup interval.

Timeouts, OOMs, truncated output, unresolved output pipes and observer failures
cannot be accepted as the requested interruption. If a command or strict probe
cannot establish quiescence, the driver starts no follow-up command and leaves
`live-data` at its original path instead of parking or copying an archive that
may still have a writer. The retained trial receipt explains the refusal; the
next trial refuses to reuse that directory. A stopped but otherwise tainted
command remains a failed observation, with its evidence retained normally.

Applied forget inspects through a strict read-only connection first. If the
engine returns the exact `BusyRecovery` error, the command can recover only the
derived WAL index under the existing maintenance and archive-writer leases,
doctor admission, and native recovery locks. The inspection still cannot write
the canonical database or WAL. It does not enter the ordinary opener's schema
salvage or dirty-WAL checkpoint fallbacks. This lets the same selectors reach
pending repair after their canonical rows were already deleted. Healthy no-op
applies, previews, and strict readers retain their existing behavior; unrelated
open failures are not converted into repair permission.

The native regressions cover real stale SHM from successive FrankenSQLite WAL
generations and the existing stock-SQLite interoperability fixture, database/WAL
byte preservation, live-writer refusal, retry after a failed lexical publication,
durable tombstones, and pending-reader fencing.
Their source is included in the existing GH477 storage and GH565 CLI tests;
they still require execution through the native quality gate. These correctness
checks do not substitute for the archive-scale memory matrix.

A strict read-only probe can refuse an interrupted WAL index that requires
recovery. The driver retains that failed probe receipt and the untouched
interrupted copy, then still attempts measured same-selector recovery. Complete
final canonical, tombstone, fence and search verification remains required.
Successful recovery does not replace the missing pre-recovery observation:
the interruption verdict stays **inconclusive**. A parsed canonical or privacy
invariant failure remains a failure, rather than being treated as unavailable
inspection. The probe never silently switches to a repairing opener.

`--canonical-interrupt-delay` defaults to one second after observing the delete
phase. It is configurable because commit speed depends on the host. A signal
before any commit, after all deletion commits, or after the selected phase
cannot prove partial-batch recovery. Such a run stays **inconclusive**; adjust
the delay and rerun into a new output directory. No benchmark-only failpoint or
production hook is added. The existing lexical failure-injection environment
variable is deliberately absent from the timing environment.

## Read the evidence

`report.json` contains profile sizes, every trial outcome and the scaling table.
`checkpoints/` retains intermediate reports. Each trial has a `trial.json` and
its archive; `command-logs/` holds exact argv, stdout, stderr, digests, terminal
exit/timeout/OOM/signal records, and timestamped `telemetry.jsonl`.

The primary memory measure is Linux `wait4().ru_maxrss`, in **KiB**, for the
actual executable and its threads. It is per invocation, not cumulative
`RUSAGE_CHILDREN`. A launcher enters the cgroup before `exec`; its small startup
RSS is included. Linux can also retain the controller's inherited pre-exec RSS
as a floor in this lifetime measurement. Receipts include parent RSS/HWM at launch
and sampled child-PID HWM (launcher, then application) to expose that effect.
The harness never subtracts parent RSS from a kernel peak. Cgroup `memory.peak` is a distinct byte measure including
descendants and charged file cache. Summed sampled process RSS can double-count
shared mappings and is labeled separately.

Phase time intervals use stderr-line arrival boundaries. They are not internal
function timers. Per-phase RSS samples can miss narrow spikes; the exact process
RSS high-water and kernel cgroup peak remain the whole-command measurements.
The OS cache is warm/uncontrolled after copying and verifying seeds; no global
cache drops occur. Existing shared page-cache charges can remain outside a fresh
cgroup, which is why both RSS and cgroup peaks are recorded and kept distinct.

Scaling comparisons group by case and memory cap. They report actual database
and message sizes, repeat medians, size/RSS/wall-time ratios, and an empirical
log slope when the startup floor is unambiguous. A raw peak at most 1.10 times
the controller's launch RSS, or missing launch RSS, conservatively marks the
comparison `rss_startup_floor_uncertain` and withholds the RSS log slope. Raw
peaks and ratios remain available, without baseline subtraction. This heuristic
does not estimate the application's true peak; inspect the recorded HWM samples
and repeat with an independently small controller if the floor is material.
Failed and inconclusive trials remain in a separate evidence list;
they are never silently pooled into successful medians. A comparison with one
successful size is labeled insufficient. These observations cannot prove an
asymptotic bound, and the nominal 128/256-row or 8 MiB planners are not RSS data.

`qualified_synthetic_profiles` requires both standard sizes, both deletion cases,
capped successful repetitions, interpretable comparisons at both sizes,
successful interruption/recovery cases, matching
binary identities and the representative database-size gate. It does not close
the reporter's issue automatically or authorize a release.
