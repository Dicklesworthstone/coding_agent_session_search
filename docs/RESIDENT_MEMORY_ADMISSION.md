# Resident-memory admission

This describes the controller and its guarded native integration. It is active
only after `responsiveness.rs` imports `resident`, calls its sampler from the
governor thread, and applies its capacity and byte limit to admission. The
integration workflow refuses to publish those call sites until Rust validation
succeeds. Source files alone are not evidence of an active controller.

The integrated native responsiveness governor combines CPU pressure with process
resident memory and effective memory headroom. It limits **new** worker and in-flight byte
admission. It does not reclaim memory already owned by an engine or allocator,
resize an existing thread pool, interrupt a running operation, or promise that
process RSS will stay below a hard ceiling. Owner-scale qualification for
`2l1b0.72` remains necessary.

## Policy

The existing governor thread samples memory; foreground admission reads retained
state and does not run an OS probe. Linux uses the existing RSS and cgroup-aware
headroom readers. macOS uses the existing bounded/cached physical-footprint
reader, with its RSS fallback. An unsupported initial sample leaves admission
unchanged. After observed pressure, missing telemetry does not count as healthy
recovery and cannot reopen byte admission.

The automatic resident target is seven eighths of the effective total memory.
The effective total includes the cgroup limit on Linux, rather than assuming that
all host RAM is available. A positive `CASS_RESPONSIVENESS_MAX_RSS_BYTES` can lower
this target; it cannot raise it past the automatic target. The override is read
once per process and is expressed in integer bytes. Missing, zero or invalid
values select the automatic target.

Admission halves at 90% of the resident target or at available headroom at or below
one sixteenth of effective total. At the resident target, or at headroom at or below
one thirty-second of effective total, memory capacity becomes 1%. Recovery adds
10 percentage points after each three consecutive healthy observations. The
recovery band requires resident usage at or below 80% of the target and available
headroom of at least one eighth of total when that probe exists.

The effective capacity is the smaller of CPU and memory capacity. Thus the
memory controller can go below `CASS_RESPONSIVENESS_MIN_CAPACITY_PCT`, which still
sets the CPU controller's floor. Positive worker requests retain at least one
worker. `CASS_RESPONSIVENESS_DISABLE=1` bypasses both controllers.

The byte ceiling is further limited to half the observed slack below the resident
target and half the available headroom above the emergency reserve. The existing
`CASS_RESPONSIVENESS_MIN_INFLIGHT_BYTES` remains a drain floor, capped by the
configured ceiling and the caller's request. A zero-byte request stays zero.
A floor can therefore be admitted even when no slack remains: this mechanism
is backpressure, not an allocation quota. Already-admitted work still needs to
drain and checkpoint normally.

## Observation and validation

Capacity transitions log `resident-memory admission updated` with the observed
resident/available bytes, effective total, target, byte ceiling and reason.
Repeated samples at the same capacity do not flood the log. Passive governor
telemetry includes a `memory` observation only after one has been recorded in
that process; no sample must not be interpreted as proof of healthy memory.

The policy's standalone Rust tests exercise pressure, boundaries, recovery,
missing telemetry, configured floors, overflow and monotonic admission across
4,097 increasing footprints. The integration workflow also runs the existing
native governor tests and lib Clippy under the unchanged CASS lockfile before
publishing its call-site integration. A policy test is not an owner-archive RSS
benchmark, and these tests do not close the broader engine-memory work.
