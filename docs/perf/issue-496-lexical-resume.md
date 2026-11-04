# Issue #496: durable lexical rebuild recovery across archive growth

## Landing direction and baseline

This is the implementation and validation report for the recovery change developed from `main` at `e5a2e9e1db3ad37be4f893988803c908b3f4a4fd`. After receiving the report of incomplete compiler/runtime validation and the strict UBS RED result, the repository owner explicitly directed committing the work directly to `main` through GitHub, with no pull request. The implementation landed as [`9aa9065ff3fc1f1d608dc88a261576e6f3849b44`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9aa9065ff3fc1f1d608dc88a261576e6f3849b44); this report accompanies the following documentation commit. This is an owner-directed landing with outstanding validation.

**Current execution status: formatting passed; strict UBS is RED; CASS compilation and runtime validation could not complete within the available shared build memory.** A dependency compiler was killed in attempt 5, and attempt 6 was stopped before exhausting the shared limit again. No CASS runtime regression or benchmark executed. The test cases and benchmark harness below are included in the implementation; their presence is not a passing test result. The evidence section records the observed failures and unexecuted behavioral validation.

The [issue](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/496) describes an approximately 8.4-million-message archive and rebuilds that could not reliably finish within a safe memory limit. A later production report described 33,641 conversations, a rebuild stopping at 26,268 after 2 hours 37 minutes, a reported 10.3 GiB memory peak plus 1.7 GiB swap, and approximately 97% producer wait time under `MemoryHigh=10G` and `MemoryMax=12G`. These are reporter observations, not measurements from this implementation.

The [November 4 maintainer update](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/496#issuecomment-6626538603) already identifies the diagnostic changes in `e5a2e9e1`, staged-shard reserve documentation in `6e48ea6f`, and earlier preparation-memory work in v0.10.0. This change addresses the remaining recovery problem: a changed whole-archive count/fingerprint must not, by itself, discard verifiable committed lexical work.

## Recovery contract

A resumable candidate is an off-live Quill generation whose committed native documents can all be matched to the current canonical archive. The saved conversation cursor is progress information; it is not proof that earlier conversations remained unchanged.

Every recovery attempt streams the entire current canonical projection. It reuses matching candidate documents and indexes missing identities. If a retained identity now has different indexed content or metadata, or a retained document has disappeared, the candidate cannot be published as the current generation. The implementation retains that candidate in quarantine and starts a fresh rebuild once.

This is deliberately **not a proof that the entire archive changed only by appending rows**. A missing document, including a backfill or a new message in an earlier conversation, can be added safely when all retained documents still match. Changes to rows that never contributed a retained lexical document can be processed from their current canonical values. The safety condition concerns the exact projected document set that will be published.

| Change or recovery condition | Behavior |
| --- | --- |
| New conversations after the checkpoint | Reuse verified existing documents; index the new documents. |
| New messages in an already processed conversation | Revisit that conversation; reuse its verified prefix and index the missing identities. |
| A missing canonical document or backfill below the saved cursor | Process it during the full audit; no cursor-based exclusion. |
| Edit to retained content, including text beyond stored previews | Refuse the candidate on its full native content witness; retain evidence and rebuild. |
| Retained title, timestamp, workspace, agent, source path, source ID, or origin metadata changes | Refuse when the changed field participates in the lexical projection. |
| Deleted retained message/conversation, or a moved retained identity | Refuse when final accounting cannot find every original live candidate document. |
| Changed metadata that is not part of the lexical projection | Does not invalidate an otherwise equal projected document. Ordinary advancement of `ended_at` is one example. |
| Bound archive replaced at the same pathname | Reject the binding. A pathname/count match is insufficient. |
| Schema, packet contract, or per-message content-cap change | Reject the incompatible binding or projection. |
| Memory or commit-batch settings change between attempts | Allowed; these control resource usage rather than document meaning. |
| Corrupt/missing candidate or inconsistent checkpoint accounting | Never certify it; preserve evidence and conservatively restart where the failure is classified as incompatibility. |
| I/O, permission, allocation, or source-epoch failure | Return the error and retain the candidate for another attempt. These failures do not prove its content is invalid. |
| Another interruption after a recovered batch commits | Retain that batch. The next attempt audits again from the beginning and skips documents now present. |

## Checkpoint and generation evidence

The checkpoint remains version 3, with additive optional fields:

| Field or mode | Purpose |
| --- | --- |
| `resume_binding` | Binds the archive's native file identity, lexical schema, packet version, and effective per-message content cap. It admits verification; it never certifies completion by itself. |
| `completed_source_epoch` | Captures physical database/WAL observations for a completed generation. Strict completed fast paths require this evidence to remain current, in addition to the binding, native manifest identity, and ordinary contract checks. |
| `StagedVerifiedReplay` | Marks a candidate being audited. Its persisted cursor cannot authorize skipping canonical rows on a later attempt. |
| `pending` and committed accounting | Preserve the existing pending-before-engine-commit protocol. Pending commits are reconciled against the same staged generation they describe. |
| `committed_meta_fingerprint` | Binds the checkpoint/receipt to the published native generation rather than an unrelated prior-live manifest. |

New staged builds record their binding before the first durable slice. An explicit version-3 `StagedSingleIndex` checkpoint from an older binary can enter the full audit even without a binding; the current binding is established before new writes. Incomplete `SharedWriter` and `LiveSingleIndex` checkpoints, whether bound or unbound, require a conservative off-live restart. Their cursors cannot skip a source prefix that has not been verified. Unrelated scratch evidence is retained in quarantine rather than adopted or deleted.

Completed legacy checkpoints without binding/epoch evidence do not receive the new completed fast path. They can still describe a historical publication, but count/maximum-ID fingerprints alone cannot certify freshness after a same-count edit. An explicit rebuild can adopt a matching completed generation through the same complete canonical/native audit, without rewriting its documents. If its projected content does not match, the rebuild creates a replacement. This conservative migration avoids preserving the old same-count-edit hole under a new completion flag.

The public checkpoint summary retains its historical meaning: it can describe a previously completed publication. Strict rebuild admission and the current-source publication validator additionally require valid source and native-generation evidence. A count-only refresh may update historical observed counts, but cannot manufacture or renew a source epoch for content it has not verified. It must not overwrite an incomplete staged audit, and it clears old source proof when the native generation or configuration binding changes.

There is no automatic epoch renewal around an operation merely declared metadata-only. A raw canonical writer could otherwise be absorbed into that operation's new physical timestamp. Benign bookkeeping or WAL changes can therefore invalidate the cheap epoch fast path; the explicit builder resolves that ambiguity through read-only content verification instead of trusting the operation's label.

Checkpoint and completed-artifact reads are bounded to 1 MiB regular files and avoid following a substituted symlink or blocking on a FIFO on Unix. Malformed or oversized bytes do not authorize publication. Checkpoint replacement uses the existing atomic JSON write and now syncs the parent directory as a durability boundary.

## Verification and write sequence

1. **Locate and admit the candidate.** Startup preserves an eligible staged generation even when the archive count/fingerprint has changed. It validates the native contract, reconciles a pending commit, and requires the actual initial live-document count to agree with the reconciled checkpoint. The candidate must be a real off-live directory.
2. **Pin source and candidate observations.** The audit holds one immutable native Keeper snapshot. Canonical counts, conversation metadata, source metadata, and bounded message reads come from one canonical read transaction. Native database-descriptor identity is checked against the current path, and database/WAL observations bracket the work.
3. **Persist the audit mode.** The saved cursor is reset for observation, `StagedVerifiedReplay` and the current binding are persisted, and stale completion evidence is cleared. No old offset is interpreted as a verified prefix.
4. **Project and classify every current canonical document.** The same projection, content cap, and hard-noise policy used by ordinary lexical indexing produce the documents. Native identity lookup classifies each document as matching, missing, or different. Matching documents need no tokenization or document insertion. Missing documents enter the existing Quill writer in bounded batches. A different retained document refuses the candidate.
5. **Commit bounded progress.** The existing pending-checkpoint/engine-commit/finalized-checkpoint sequence is used for each accumulated batch, including batches ending inside a large conversation. A restart constructs a new native snapshot containing all committed work and re-audits it.
6. **Prove full coverage.** The audit requires every original live candidate document to have been encountered exactly once, every canonical row/conversation count to agree with the pinned source, and the resulting live document count to equal the complete current lexical projection. Strictly ordered nonnegative canonical message indices prevent duplicate identities from being silently counted as independent work.
7. **Finalize off-live, then publish.** The normal bounded merge policy, native readability/count validation, generation artifacts, completed checkpoint, and independent publication receipt are applied to the candidate. Source epoch and binding checks precede the existing publication protocol. The complete generation is then published through the existing directory swap and validated again at the live path.

The native witness is Quill's schema-v1 **XXH3 content witness**, computed with the engine's field projection and framing. It includes the complete projected indexed content and metadata, rather than only a preview. It is not a cryptographic authenticity mechanism, and this audit does not independently reconstruct or verify every posting list. Existing native admission checks and actual-engine parity tests remain part of the trust boundary. Search-result comparison with an independent fresh rebuild provides an additional regression oracle on the test corpus.

The equivalence ledger includes the documents encountered during the full canonical audit, including retained documents. Its packet-oriented hashing can differ when batch boundaries differ; raw equality of two generation-ledger hashes is not used as the sole correctness test.

### Completed-generation revalidation

The completed-revalidation helper first considers a strict fast path: matching archive/configuration binding, matching source epoch, matching saved native manifest fingerprint, matching native document count, and valid paired generation/equivalence artifacts. The artifacts must use the current manifest format, describe a validated published generation, match the checkpoint's source fingerprint and exact canonical totals, and agree on indexed-document count and equivalence fingerprint. Missing or malformed artifacts trigger a full audit. Source and native-generation evidence are checked again after artifact and checkpoint admission, immediately before accepting the shortcut. Equal counts alone do not admit a different native generation.

When that evidence is absent or stale, the builder can run the shared canonical/native audit with no writer. Every current canonical document must match the live native generation, with exact document-set coverage. A successful audit regenerates and durably persists the paired generation/equivalence artifacts before updating the completed checkpoint and publication receipt. It records the epoch captured **before** the audit and rechecks the source, native generation, and checkpoint around publication of that evidence. Native `MANIFEST` and segment files remain unchanged. Missing, changed, or extra native documents require a fresh rebuild; source or I/O changes during the audit retain the existing evidence and return an error.

Empty conversations and hard-noise messages still contribute to canonical conversation/message totals even when they add no lexical documents. Completion status preserves those canonical totals separately from indexed-document counts.

Completed preflight classification validates the actual native publication as well as current source evidence. After a full source scan, the no-rebuild shortcut reclassifies that evidence and requires it to match the initially admitted status. Scan mutation counters alone cannot detect an independent same-count canonical edit or a native-generation replacement. These strict admission checks are separate from historical status readers; the latter do not claim to run a current content audit or validate every paired artifact.

This handles metadata-only database writes without absorbing an unrelated raw canonical edit into a renewed epoch. It also permits legacy completed generations to gain current evidence through an actual content proof. The additional verification is a full bounded source scan, performed by the explicit rebuild path; ordinary metadata writes and historical status reads do not silently start it.

## Publication, source writers, and crash windows

The prior publication remains separate from an incomplete rebuild. `.lexical-published-state.json` binds the historical live generation while `.lexical-rebuild-state.json` records progress against the scratch sibling. Serving the previous complete publication does not assert that an interrupted candidate is complete or that the archive has stopped changing.

The patch preserves the existing atomic publication protocol and its platform-specific recovery behavior; it does not redesign that protocol. On Linux, the normal path uses `RENAME_EXCHANGE`, then parks/retains the previous generation. The existing rename-pair fallback and recovery sidecar remain in use where an exchange is unavailable. Filesystem publication selects complete native generations; it does not expose a partially built candidate as the completed generation.

The first-publication boundary needs special handling. Publishing into a checkpoint-only live placeholder can remove that placeholder before the first rename succeeds. After a crash or failed rename, the completed native candidate may hold the only checkpoint. Startup now discovers that terminal scratch checkpoint **only if the live checkpoint file is absent**, converts it back to incomplete verified-audit state, persists it at the normal progress location, and performs the complete audit. A corrupt live checkpoint is not silently overridden, and a terminal scratch receipt is not permission to publish without examining current canonical data.

The fresh builder's final source batch now also uses the pending/native/finalized checkpoint protocol before segment folding. Previously a bare final native commit could leave durable documents ahead of the checkpoint during an interruption in that fold. Recording the tail before folding preserves recoverable progress without relaxing exact candidate accounting. The later terminal checkpoint still records the post-fold native fingerprint.

Ordinary CASS writers are coordinated by the existing index-run lease. Raw database writers that ignore that lease are additionally detected through the source read snapshot and database/WAL epoch checks. A deterministic regression writes canonical data at the pre-publication observer and verifies refusal followed by successful recovery.

There remains a narrow boundary between the last source-epoch check and the filesystem rename: an uncoordinated raw writer can commit during that interval. This implementation does not introduce a transaction spanning SQLite and a directory swap or block all external writers. A generation from the immediately preceding canonical snapshot can therefore be published in that race. Its persisted source epoch prevents a later freshness check or completed fast path from treating it as current once the change is observed. The publication guarantee is complete-generation visibility under the existing protocol, with explicit source-generation evidence; it is not global serialization against arbitrary external database writes.

Completed revalidation uses the same rule: persist the epoch that was actually audited, never a newly captured epoch after an unverified mutation interval. A raw write during its verification must cause refusal; a write after its final check makes the saved epoch stale on the next strict check rather than being silently included in the proof.

## Work and memory bounds

Let `N` be the current canonical message/source-byte volume, `R` the number of matching live candidate documents, and `D` the current projected document count.

| Work | Recovery | Restart from zero |
| --- | --- | --- |
| Read/project canonical source | Full bounded scan, `O(N)` | Full bounded scan, `O(N)` |
| Classify retained documents | Native identity/content comparison for the current projection | Not required for a fresh candidate |
| Add/tokenize documents | `D - R` missing documents | All `D` documents |
| Previously committed native segments | Retained and reused when verified | Rebuilt |
| Final merge/validation | Existing bounded policy and native checks | Existing bounded policy and native checks |

Recovery is not an `O(delta)` source scan. It saves rebuild/tokenization work and retains durable progress; it still reads and normalizes the canonical archive to prove that the candidate is reusable. Final concat/merge maintenance can rewrite retained segment bytes, so total filesystem writes are not promised to equal only the missing-document bytes.

The source batch and pending document aggregation use the existing startup/steady message, conversation, byte, and effective in-flight budget controls. The recovery path caps a batch at 16,384 messages and 64 MiB of content, with tighter existing settings taking precedence. It reevaluates the effective byte limit while streaming. A single message can exceed a batch-byte target but remains subject to the canonical per-message content cap, as in the ordinary chunked producer.

The audit retains one native candidate view and one bounded canonical batch rather than an archive-sized map of source messages. Native index metadata, mappings, the writer, and merge working sets still contribute to process memory. The batch limits are not an assertion that total RSS is constant or that an arbitrary process/cgroup memory limit can never be reached. The existing per-message cap and upstream transient materialization behavior also remain relevant for unusually large/non-ASCII source cells.

## Regression inventory

The new `src/indexer/gh496_tests.rs` exercises real FrankenSQLite archives, Quill segments, normal checkpoint writes, and the production publication seam. Additional completion-policy regressions are in `src/indexer/gh496_completed_tests.rs`. These are implemented tests awaiting the execution evidence below.

| Area | Regression evidence implemented |
| --- | --- |
| Ordinary archive growth | Append a conversation and extend an already processed conversation; retain original native segment bytes; compare complete native documents and search output with an independent fresh rebuild. |
| Upgrade of interrupted main-branch work | Remove the new binding from an explicit staged checkpoint and verify adoption only through a full audit. |
| Actual `index --full` startup | Exercise full-index preflight with closed connector roots and ensure the eligible stage survives. |
| Repeated interruption | Abort after real durable engine commits, grow again, resume repeatedly, and verify no missing/duplicate documents. |
| Pending commit window | Commit native documents after writing pending state but before finalizing the checkpoint; then append and recover. |
| Fresh final-batch process interruption | A CLI child is killed after the final native commit but before checkpoint finalization. The sentinel must prove that all expected documents reached the native generation; the existing middle-batch kill case remains covered separately. |
| Large-conversation boundaries | Use small production message/byte settings, interrupt within a conversation, lower the budget, and recover all messages. |
| Concurrent canonical writer | Commit through a real independent canonical writer before publication; require `SourceChanged`, an unchanged prior-live manifest, and successful retry. |
| Atomic publication failure | Inject an actual rename failure, verify retained completed native work, append more canonical data, and recover. |
| First-publication checkpoint loss | Fail `FirstPublish` after placeholder removal, verify the completed seven-document candidate with no live checkpoint, then grow and recover it. |
| Same-count mutations | Content, timestamp, title, source path/ID, origin host, workspace, agent, deletion, and message-identity changes cannot preserve a stale retained projection. |
| Content beyond previews | Replace a long document tail with a same-length term while the count/ID fingerprint stays unchanged; old query must disappear and new query must match. |
| Completed-state freshness | Edit after a successful guarded recovery; count-only refresh cannot renew its epoch, and the strict current-source validator must refuse it until explicit content verification/rebuild. |
| Completion migration and bookkeeping | Require content verification for legacy completion evidence; prevent a count refresh from completing an unrelated staged audit; revalidate benign epoch drift with no native writes. |
| Writer during completed revalidation | Inject a raw canonical edit after the audit has visited its source rows; require refusal without replacing the saved source proof. |
| Completed shortcut race | Prove unchanged evidence takes the shortcut without a canonical scan; mutate source or native generation at final admission and require refusal without renewing the checkpoint. |
| Legacy live cursors | Bound and unbound `SharedWriter`/`LiveSingleIndex` prefixes cannot skip same-count canonical edits. |
| Paired artifact repair | Missing or corrupt generation/equivalence artifacts force a full audit and are repaired without native document commits. |
| Unindexed canonical growth | Empty and whitespace-only conversations change canonical totals to four conversations/six messages while the native index remains at four documents; paired evidence and completed status retain that distinction. |
| Same-count native-generation replacement | Replace one indexed payload while keeping four live documents; both completed classifiers reject it before count refresh, count-only refresh clears old source proof, and the explicit builder restores canonical content. |
| Inconsistent completed counters | Existing GH494 coverage requires authoritative content verification and safe repair of inflated completed counters. |
| Corrupt completion flag | Flip `completed` on a two-document prefix whose native count matches a two-document live tree but whose canonical archive contains four documents. |
| Corrupt evidence | Inflated checkpoint counters, truncated checkpoint JSON, and a corrupt staged manifest cannot certify missing/stale documents. |
| Temporary source unavailability | Temporarily move the canonical database, require an error with byte-identical checkpoint/staged evidence and no quarantine, then restore the same file, append data, and recover the retained work. |
| Archive and configuration identity | Replace the database at the same path; change schema/content cap; require conservative behavior. |
| Native witness compatibility | Existing exact-verifier tests distinguish missing, matching, and different real engine documents and cover full projection parity. |

## Validation evidence

**There is no accepted final gate receipt and no passing runtime-test total.** This is a reviewable implementation with blocked behavioral validation, not a commit-ready green result. The latest attempted implementation source digest is `e1bd16abd7e9d7b1e93ce1a96ef10e271bb4a77e1955f04b0bea34ecbc74c77d`, based on `e5a2e9e1db3ad37be4f893988803c908b3f4a4fd`.

| Validation | Result | Exact evidence / source identity |
| --- | --- | --- |
| Source identity and freshness | PASS | Attempts 5/6: `STAGE=source-identity EXIT=0`, `STAGE=source-freshness EXIT=0`, digest above. |
| Formatting | PASS | Attempts 5/6: `STAGE=fmt EXIT=0`; scoped `git diff --check` also exited 0. |
| Required Clippy build | BLOCKED / RED | Attempt 5: `STAGE=clippy EXIT=101`, dependency compiler received `SIGKILL`. Attempt 6 was stopped during the same dependency check before any CASS diagnostic. |
| Strict UBS, all ten changed Rust files | RED | Standalone scan exited 1: 74 critical, 13,220 warning, 3,335 informational findings; all before/after source hashes matched. |
| Strict UBS, original tracked files at `main` | RED | Same-policy baseline scan exited 1: 71 critical, 12,504 warning, 3,266 informational findings across the seven original tracked files. |
| Strict UBS, new recovery module only | RED | Exit 1: 0 critical, 1 warning, 31 informational findings. This narrower diagnostic does not replace the full scan. |
| `gh496` recovery/completion regressions | NOT RUN | Compilation did not reach the CASS test binary. |
| Existing `gh494` and lexical rebuild regressions | NOT RUN | Selected in the gate; compilation remains blocked. |
| Native exact-content verifier tests | NOT RUN | Selected in the gate; compilation remains blocked. |
| CLI/integration and golden regressions | NOT RUN | Selected in the gate; compilation remains blocked. |
| Paired interrupted/resumed versus restart benchmark | NOT RUN | No executable worker measurements are available. |

Attempt 5 used the pinned `nightly-2026-08-31` toolchain (`rustc 1.100.0-nightly`, commit `908501772`, 2026-08-30), one Cargo job, `-Z threads=1`, disabled development/test debug information, and `CARGO_INCREMENTAL=0`. The shared cgroup had an 8 GiB cap and no swap. Near failure, anonymous memory was approximately 7.58 GiB with about 51 MiB file cache, while the observed CASS-owned dependency compiler was about 1.59 GiB RSS. The final log shows that compiler receiving `SIGKILL` before the subsequent manual interruption returned exit 130. Cgroup OOM counters also increased, but they do not identify every affected process. These are build-environment observations, not CASS indexing-memory measurements.

After shared pressure fell, attempt 6 repeated the full unchanged gate and code digest. It again passed identity, freshness, and formatting, and reached the `asupersync` dependency check. Its observed compiler RSS reached about 5.68 GiB (peak about 5.69 GiB); shared anonymous usage reached about 7.60 GiB with only about 76 MiB file cache. The owned job was stopped with exit 130 before another freeze. That attempt has no `SIGKILL` or compiler-error diagnostic, and its OOM counters did not increase during the final observation. No further repeated builds were launched under the same inadequate shared headroom.

Both attempts stopped before the gate consolidated its receipt and performed final source-stability/completeness checks. Their `.receipt` files contain only source headers; the actual stage lines and failure details are in the `.log` files and retained per-attempt streams. Neither is a completed passing receipt. Attempt 6's full source-diff SHA-256 was `4f31afccc610ba9bc263560e8ab761cc48a88416c4da25bc233cedeeaf9d7352`; documentation was subsequently updated to record this outcome, while Rust code and the implementation content digest remained unchanged.

Earlier attempts were interrupted while preparing dependencies or when reviewed source corrections invalidated the captured tree. Their partial formatting/source checks are not accepted full-gate receipts. No CASS compile error, runtime-test failure, or benchmark result is inferred from those attempts.

The strict standalone scan used the exact `run_ubs` and positive Rust AST-control functions from unchanged `scripts/gate.sh`, pinned UBS v5.4.4, the existing ignore file, `--ci --fail-on-warning`, and the same 300-second module timeout. The runner, routing contract, all module/helper hashes, and AST positive control were verified. No scanner policy or suppression was changed.

The same-policy baseline comparison extracted the seven original tracked Rust files at `e5a2e9e1` into an isolated directory, with identical scanner configuration; the three new modules are absent at that commit. Its Rust scan completed once in 117 seconds without a timeout. Baseline/configuration/repository source hashes remained unchanged. The baseline source-manifest SHA-256 is `f044014e1e5198919e770dace4a030f2e35f951aff6eb6fdf76ad7fd1a497fe9`; its log SHA-256 is `4d11f2dee0ed45babea2c71bf0ab2d0acddddb62900ee4dc7a2220176cc65270`.

The raw proposed-minus-baseline differences are **+3 critical, +716 warnings, and +69 informational findings**. The three added critical flags are exactly three `panic!` guards in the new completed-generation regressions: unexpected native writes during artifact repair, unexpected canonical scanning on the unchanged shortcut, and unexpected native writes during completed admission. These deliberately fail tests on incorrect behavior; none is a production panic. The remaining critical categories have equal baseline/proposed counts and matching existing source samples. Warning/informational differences have not been individually classified, and both strict scans remain RED.

The focused module's one warning is `rust.perf.string-alloc-in-loop` on the `ContentMatch::Different` error path. That path formats one diagnostic and immediately returns; source review found no accumulating hot-path allocation there. Displayed full-scan critical samples include intentional test failure sentinels and unchanged synthetic/security test fixtures. This triage does not waive the RED scan or claim individual review of every aggregate warning.

Local evidence files are `cass-gh496-gate-attempt-5.log`, `cass-gh496-gate-attempt-6.log`, their header-only receipts, `cass-gh496-ubs-standalone-1.log`, `cass-gh496-ubs-baseline-1/strict-ubs.log`, `cass-gh496-ubs-baseline-1/receipt.json`, and the focused production-module scanner log in the execution workspace. Actual compiler/test receipts and raw paired benchmark output remain required before behavioral or performance claims can be made.

The repository gate is the entry point for final validation. In an environment using the permitted local execution path, a representative targeted invocation is:

```bash
CARGO_BUILD_JOBS=1 \
GATE_BUILD_JOBS=1 \
CARGO_INCREMENTAL=0 \
CARGO_PROFILE_DEV_DEBUG=0 \
CARGO_PROFILE_TEST_DEBUG=0 \
RUST_MIN_STACK=134217728 \
RUSTFLAGS='-Z threads=1 -C link-arg=-fuse-ld=lld' \
scripts/gate.sh --local \
  --lib-filter 'gh496 gh494 lexical_rebuild lexical_reconcile' \
  --integration 'e2e_index_resume,gh494_publish_resume,atomic_swap_publish_crash_window,cli_index:killed_between_commit_and_checkpoint' \
  --no-bin
```

The filter argument contains multiple libtest name substrings, combined as alternatives. The final gate should confirm positive coverage for every requested filter. The initial environment used the repository's pinned `nightly-2026-08-31` toolchain, a shared 8 GiB cgroup, no swap, and a read-only cgroup configuration. Record the actual toolchain, CPU availability, profile, memory controls, and receipt used for the accepted run rather than inferring them from those setup observations.

### Paired benchmark design and commands

`gh496_benchmark_interrupted_resume_against_restart` is an ignored benchmark driver. It creates one canonical archive and a real committed staged prefix. After that checkpoint, it appends one message to an existing conversation and adds 10% more conversations. It runs recovery and restart from zero in two fresh child test processes against the same unchanged canonical database, within the same driver invocation.

Each child checks exact final counts, native readability/count, and publication receipt usability before reporting. Its `GH496_BENCH` JSON row contains measured elapsed time, Linux `VmHWM` in KiB when available, exact final counts, the native candidate count observed before recovery, and an arithmetic missing-document count. That last value is final expected documents minus initial candidate documents, not an independently instrumented insertion or retention count. Fixture generation and compilation occur outside the measured worker processes. This compares recovery and restart strategies in the same binary; it is not a comparison between two released CASS versions.

Wall time measures `rebuild_tantivy_from_db`, including lexical rebuilding and publication. It does not measure the entire CLI ingestion, fallback-FTS, or vector-indexing workflow. Worker peak RSS also includes initial candidate admission and post-timing validation. Both competitors use the ordinary merge path unless the inherited `CASS_TEST_SKIP_POST_RUN_LEXICAL_MAINTENANCE` hook or normal resource policy skips it. Accepted benchmark invocations explicitly unset that hook and record actual compiler flags and Cargo profile; these are test-binary measurements.

The generated text contains deterministic natural-language tokens and conversation/message identifiers. It is a synthetic lexical workload, not a copy of the reporter's archive. The normal run uses 200 original conversations with 40 messages each and 1,024 body bytes. The staged prefix covers 150 conversations/6,000 documents; after growth, the expected canonical projection contains 220 conversations/8,801 documents. These are fixture parameters, not measured performance results.

From the repository root, after building the test binary:

```bash
cargo test --lib gh496_benchmark_interrupted_resume_against_restart \
  -- --ignored --nocapture --test-threads=1
```

A larger synthetic comparison with 2,000 original conversations yields a planned 88,001-document final corpus:

```bash
CASS_GH496_BENCH_CONVERSATIONS=2000 \
CASS_GH496_BENCH_MESSAGES=40 \
CASS_GH496_BENCH_BODY_BYTES=1024 \
cargo test --lib gh496_benchmark_interrupted_resume_against_restart \
  -- --ignored --nocapture --test-threads=1
```

To compare the same workload under smaller production batch/merge settings:

```bash
CASS_GH496_BENCH_CONVERSATIONS=2000 \
CASS_GH496_BENCH_MESSAGES=40 \
CASS_GH496_BENCH_BODY_BYTES=1024 \
CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGES=256 \
CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES=256 \
CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGE_BYTES=2097152 \
CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGE_BYTES=2097152 \
CASS_TANTIVY_REBUILD_PIPELINE_MAX_MESSAGE_BYTES_IN_FLIGHT=8388608 \
CASS_LEXICAL_MERGE_MAX_OUTPUT_BYTES=33554432 \
cargo test --lib gh496_benchmark_interrupted_resume_against_restart \
  -- --ignored --nocapture --test-threads=1
```

These settings constrain batches and merge outputs. They are not an enforced process RSS limit and do not reproduce the reporter's systemd `MemoryHigh`/`MemoryMax` configuration. If a writable cgroup or comparable process-memory constraint becomes available, record the exact external limit separately and run both competitors within it.

### Real measurements

**No recovery/restart wall-time or runtime RSS measurements were obtained.** The test binary could not be built in this execution environment. Compiler RSS and static-scanner durations above are not indexing benchmarks. The following entries describe planned fixture sizes only:

| Planned paired workload | Expected final conversations/docs | Intended staged prefix | Runtime evidence |
| --- | --- | --- | --- |
| Small synthetic | 220 / 8,801 | 6,000 documents | NOT RUN; wall time and RSS unmeasured. |
| Larger synthetic | 2,200 / 88,001 | 60,000 documents | NOT RUN; wall time and RSS unmeasured. |
| Larger corpus with smaller batch/merge settings | 2,200 / 88,001 | 60,000 documents | NOT RUN; wall time and RSS unmeasured. |

For a future successful pair, retain both raw worker JSON records, observed candidate counts, elapsed time, process RSS, environment limits, and all slower or failed outcomes. Do not replace the missing measurements with the intended prefix counts.

The driver runs recovery before restart and does not flush the operating-system page cache. The source is shared and may already be cached by fixture setup or the preceding child. Single paired results are observations for that workload/order, not a statistical performance guarantee. `VmHWM` describes one worker process; it is not the cgroup's aggregate memory peak, page-cache charge, or swap consumption.

The source-level expectation is fewer document additions: `D - R` versus `D`. That does not imply a proportional elapsed-time or RSS improvement because both paths read canonical source, and recovery also verifies native witnesses. Any timing ratio must be calculated from completed paired measurements. No 8.4-million-message benchmark, production systemd-limit reproduction, or production-scale memory improvement is established by the fixtures or source analysis in this report.

## Owner-directed main-branch landing record

The owner authorized both commits directly on `main` through GitHub, using the subjects and file groups below. This direction was given after the outstanding validation results were disclosed. The implementation commit includes the actual validation status in its full GitHub message; the descriptions below summarize the changes. The documentation commit's identity is available in this file's Git history, avoiding a self-referential commit hash inside its own contents.

| Commit | Authorized subject | GitHub commit SHA |
| --- | --- | --- |
| 1 | `fix(index): resume lexical rebuilds safely across archive growth` | [`9aa9065ff3fc1f1d608dc88a261576e6f3849b44`](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/9aa9065ff3fc1f1d608dc88a261576e6f3849b44) |
| 2 | `docs(index): document lexical recovery and validation evidence` | The commit introducing this report; see [file history](https://github.com/Dicklesworthstone/coding_agent_session_search/commits/main/docs/perf/issue-496-lexical-resume.md). |

### Commit 1

Subject:

```text
fix(index): resume lexical rebuilds safely across archive growth
```

Change summary:

```text
Preserve interrupted staged Quill generations when canonical archives grow.
Audit all retained document identities and native content witnesses against
one bounded canonical snapshot, add missing documents, and refuse changed
or orphaned retained projections.

Persist archive/configuration bindings and completed source epochs, retain
incompatible candidates, durably checkpoint bounded replay batches, and
recover terminal candidates whose first-publication placeholder disappeared.
Keep prior publication authority separate from rebuild progress.

Refuse unverified legacy live-prefix cursors, recheck completed admission,
repair paired completion evidence only after a full audit, and durably
checkpoint the fresh final batch before folding its native segments.

Add real-engine recovery, corruption, mutation, publication-fault and
completion regressions plus a paired resume-versus-restart benchmark.

Refs #496; coding_agent_session_search-2l1b0.46 (remains open)
```

Exact file group:

- `src/indexer/mod.rs`
- `src/indexer/lexical_resume.rs`
- `src/indexer/lexical_reconcile.rs`
- `src/indexer/lexical_reconcile/canary.rs`
- `src/indexer/lexical_reconcile/canary/exact.rs`
- `src/indexer/lexical_reconcile/snapshot.rs`
- `src/indexer/gh494_tests.rs`
- `src/indexer/gh496_tests.rs`
- `src/indexer/gh496_completed_tests.rs`
- `tests/cli_index.rs`

### Commit 2

Subject:

```text
docs(index): document lexical recovery and validation evidence
```

Change summary:

```text
Document lexical recovery admission, complete canonical verification,
checkpoint/publication durability, configuration and source-generation
guards, and the boundary for uncoordinated canonical writers.

Record validation outcomes and reproducible benchmark commands, keeping
observed results separate from unexecuted scale and memory claims.
Link the recovery report from the indexing documentation.

Refs #496
```

Exact file group:

- `README.md`
- `docs/perf/issue-496-lexical-resume.md`

The landing proceeds at the owner's explicit direction with compiler/runtime validation unfinished and the strict scanner result still RED. The recorded failures and NOT RUN entries remain outstanding; this authorization does not establish that the repository's quality gates or scanner findings have been resolved. Follow-up work must run the complete gate and paired comparisons on a build host with sufficient available memory, address resulting implementation findings, and resolve or explicitly review the scanner findings under repository policy. Behavioral and performance claims still require actual source-bound execution evidence. No pull request is part of this landing.
