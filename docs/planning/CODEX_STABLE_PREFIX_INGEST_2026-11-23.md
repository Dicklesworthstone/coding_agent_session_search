# Codex stable-prefix ingestion: implementation and qualification

## Scope and source identity

This implementation addresses actively growing plain Codex JSONL rollouts.
The original validated candidate was based on CASS commit
`710435869716073a3b5aa015ad3f9872426e2e4a` and retains the exact Franken Agent
Detection dependency `a0dd013a8c1e374d6a80949637e5e51cbb45550b` (0.3.10).
The dependency pin is unchanged. Integration preserves concurrent upstream
changes, including the raw-mirror admission and sensitive-store guards.

The governing sources are:

- [The comprehensive plan](PLAN_TO_MAKE_CODING_AGENT_SESSION_SEARCH.md) and
  [the current bridge plan](REALITY_CHECK_AND_BRIDGE_PLAN_2026-09-01.md).
- [GH #559](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/559),
  including veridian-swan's still/reopen/held reproduction and the
  November 23 maintainer clarification.
- [The diagnostics correction](https://github.com/Dicklesworthstone/coding_agent_session_search/commit/b871f75a1b3f35b5bc643deb2e0833f7f41f1783)
  and the subsequent idle-held-writer change `7d1f7f213`.
- [GH #560](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/560),
  whose existing inactive abandoned-tail behavior is preserved.
- The checked-in Beads entries `coding_agent_session_search-gh559-source-diagnostics`,
  `coding_agent_session_search-gh560-inactive-codex-tail`, and
  `coding_agent_session_search-watch-stable-prefix-ingest-v6ysi`.
- The pinned FAD
  [Codex reader](https://github.com/Dicklesworthstone/franken_agent_detection/blob/a0dd013a8c1e374d6a80949637e5e51cbb45550b/src/connectors/codex/reader.rs),
  source-boundary hooks, CASS enrichment and source-budget guards,
  `build_support/source_ingest_contract.rs`, source-ledger matching,
  startup persistence, watch publication debt, and raw-mirror policy.

The broader bead also requests other providers, compressed-frame policy,
rewrite reconciliation, and populated real-watch latency measurements. This
candidate does not close that bead or assert a sub-ten-second latency result.

## Admission and parser contract

The unchanged FAD reader already limits a read to its initial file length,
but also rejects a changed final size or mtime. Merely bypassing CASS's active
source filter therefore cannot implement a growing-file prefix. FAD's public
source interface accepts paths, not an injected bounded reader.

The adapter uses a private immutable parser view:

1. Apply original-source path exclusions and the host admission predicate.
   Recheck exclusions after the predicate, before copying any source bytes.
2. Open a regular Unix `rollout-*.jsonl` source read-only with `O_NOFOLLOW`
   and `O_NONBLOCK`. Retain the descriptor, device and inode. Advisory-lock
   evidence refuses admission.
3. Fix admission length **N** from the opened file. Enforce the existing Codex
   per-source read budget; do not follow subsequent appends to extend N.
4. Copy at most N bytes using a 64 KiB buffer. Remember the last two LF
   boundaries and their BLAKE3 states. **B** initially ends at the last LF;
   the active user-prompt frontier described below may retain the preceding
   boundary instead. Truncate only the private copy to B; never alter the
   original rollout.
5. Make the empty staging directory mode `0700` before creating transcript
   bytes. The copy starts at `0600` and becomes `0400` before parsing.
6. Run the exact pinned FAD parser and existing CASS enrichment on that same
   view. Restore the original source path, scoped session ID, scan root,
   origin and platform before host delivery. The staging name is not an
   archive identity.
7. Verify the named file and retained descriptor still refer to the captured
   device/inode, have not shrunk below N, remain unlocked, and contain the
   same B bytes. Rehash before and after delivery. Revalidate again after
   asynchronous queuing or watch spooling, before canonical publication.

Appends beyond N are permitted. Same-size edits, rewrite-plus-growth,
truncation/regrowth with changed admitted bytes, pathname replacement,
symlink substitution and advisory locks fail closed. Restoring size and
mtime does not bypass the content/object proof.

A live final record without LF is excluded even when its JSON is valid.
Incomplete JSON and torn UTF-8 in that excluded suffix wait for a later
pass. Malformed bytes within the admitted complete-record region remain
subject to the existing parser and enrichment rules. A zero-message prefix
with an unfinished suffix retains an `UnexpectedEof` retry; a valid complete
header-only or blank file fabricates neither a conversation nor completion.

### Existing inactive EOF and abandoned-tail behavior

The original `source_can_recover_incomplete_tail` predicate remains the
authority for inactive EOF recovery. When it permits a non-LF EOF, the view
contains the entire admitted N bytes and retains the source's mtime so the
pinned parser applies its existing policy. That mode has a distinct
`inactive_eof` receipt flag and requires strict length, mtime and inactivity
stability throughout. It cannot be downgraded into permission to consume an
active unterminated record.

This also handles a previously published live prefix whose writer dies with
an unfinished suffix. Prior overlap is still verified before the existing
abandoned-tail recovery runs. The source itself remains unchanged.

## Stable message projection across appends

The pinned parser coalesces neighboring user-prompt event and response
records. It keeps the response side, which may have different whitespace or
a slightly different timestamp. Publishing a trailing event before its
response arrives can therefore invalidate an otherwise unchanged byte
prefix.

For an active source, admission conservatively withholds the last complete
physical line when its last native-emitting record is such a user event.
Ignored or enrichment-only records on that same recovered line do not close
FAD's native-message adjacency. Blank or malformed intervening physical lines
do affect its physical adjacency. A zero-byte frontier retains retry debt
instead of falling through to a parser route that would publish the pending
event. The existing inactivity predicate can finalize the pending record.

CASS enrichment also needs append-stable message indices. An explicit
thread-bound guard identifies the actual private parser snapshot. Within that
scope, enrichment locates the pinned parser's rows by their source occurrences
and sorts by those occurrences instead of timestamps. Each original primary
row must consume exactly one classified occurrence. Discarded paired events
consume none. Exact raw evidence is used when available; compacted rows use
the pinned native identity tuple with one-to-one occurrence queues. Known
different raw provenance cannot be replaced by a broad content/time match.
An unclassified or unmatched primary row fails closed.

Equivalent event/primary representations retain their existing equivalence
predicate, but newly enriched rows do not replenish the matching inventory.
Repeated same-stream messages therefore remain distinct. Strict, compressed,
archive and non-Unix paths retain their existing ordering.

Neither the initial observed length nor the committed boundary may regress
behind a previously admitted receipt. This also protects previously admitted
bytes that did not produce a canonical message. Raw recapture uses the
already certified B exactly and does not apply a fresh prompt frontier.

The existing inactivity contract remains meaningful: after an inactive event
has been finalized, a later matching response can still require reconciliation
because its normalized projection differs. The adapter refuses that rewrite;
it does not alter historical canonical rows to make a new parse fit.

## Receipts, duplicate prevention and future discovery

`StablePrefixReceipt` is a versioned, typed record containing:

| Field | Meaning |
| --- | --- |
| `source_path`, `device`, `inode` | Original named file and captured physical object |
| `admitted_bytes` | N, the fixed read ceiling |
| `committed_bytes` | B, the admitted complete-record boundary, or strict inactive EOF |
| `blake3` | Digest of exactly the admitted B bytes |
| `partial` | This receipt cannot authorize whole-source reuse or coverage |
| `message_count`, `messages_blake3` | Previously admitted normalized-message overlap |
| `inactive_eof` | Existing strict inactive EOF/recovery mode |

A thread-bound scoped guard grants delivery authority only during the real
parser callback. Serializable packet metadata alone does not grant active
admission or mint a source completion. Completion has its own guard phase.
Generic FAD consumers receive unknown size/mtime for partial completions,
preventing equality with an ordinary full-file metadata fingerprint.

The typed receipt lives in the existing `source_ingest_v1` observation and
inherits its source/root/origin key and producer contract. No new database
table, competing publication ledger or dependency pin is introduced. The
existing build contract recursively hashes the changed producer source.

The parser reprocesses a bounded prefix on each pass. It does not seek from B
with synthetic parser state. Before extending a stored prefix, the adapter
verifies both its original byte hash and the normalized-message projection
already represented by the receipt. The projection digest includes index,
role, author, timestamp and content. This avoids silently trusting the
archive's append-only message-index conflict handling when a later parse
reorders or changes earlier messages. Canonical persistence then validates
the actual stored overlap and admits new message indices exactly once.

### Canonical overlap, upgrades and repeated messages

An older archive can contain messages without a prefix receipt. A new source
hash alone cannot establish that those rows match the admitted projection.
The storage guard therefore runs after resolving the actual conversation ID,
inside the existing FrankenSQLite transaction, on normalized and redacted
messages. It compares role, author, timestamp and content at each occupied
incoming index. Divergence fails with an explicit reconciliation error; the
error does not include transcript content.

Two indexed boundary queries reject a canonical tail outside the full
receipt's message count. That count remains the source boundary even when
the host delivers a smaller ingestion slice. Each incoming-index comparison
returns a scalar equality result, avoiding an unbounded second copy of
historical transcript bodies. The guard also sees earlier writes in the
same transaction, so a conflicting later slice rolls the batch back.

For prefix packets, equal occupied indices are replay no-ops and missing
indices use strict insertion. Different indices remain distinct even when
text, role, author and timestamp are identical or timestamps are absent.
Generic content/timestamp replay shortcuts cannot suppress those legitimate
records. The guard covers single-conversation, batched, profiled and
duplicate-insert recovery paths; unrelated packets retain existing behavior.

Partial receipts never satisfy whole-source reuse, complete-source recovery
coverage or provider/root watermark advancement. Existing active-source
deferral and lexical replay debt keep them discoverable even without another
filesystem event. Ordinary nonstreaming configuration still routes actual
source-boundary-capable Codex connectors through the durable bounded sender.

### Publication ordering and crash recovery

Startup holds the Codex conversation until final source validation and its
typed completion. A missing completion drops that provisional packet rather
than flushing it as an ordinary batch at the next source boundary. Watch
likewise spools provisionally and filters uncertified packets before canonical
publication. Receipt-capacity exhaustion defers a prefix before delivery.

Before any completed prefix can modify canonical rows, including split/OOM
fallback, the host writes an invalidated receipt containing the new overlap
proof. The invalidation is not a publication certificate. A crash can leave
proof and replay debt, but cannot leave an obsolete reusable EOF certificate
or discard the overlap needed to detect divergent replay.

Startup replaces that marker with the source completion in the existing final
canonical transaction. Watch replaces it after the corresponding lexical
commit. Ordinary lexical rebuild/replay mechanisms remain responsible for
derived-index recovery. Full/rebuild modes disable reuse while retaining Codex
overlap evidence whenever canonical rows survive.

Before persistence, canonical packet metadata is aligned with the final
revalidated receipt. A later append cannot leave `partial=false` in that
metadata while the publication ledger correctly records a partial prefix.

The bounded watch receipt cache distinguishes omitted entries from genuine
absence. A Codex miss in an incomplete cache receives one indexed exact-key
lookup. SQL limits returned text to 256 KiB by UTF-8 byte length before
materializing it; oversized or nontext values remain unusable evidence.
A genuinely absent key may proceed, while an omitted existing receipt still
protects its source. The storage mutex is released before source hashing or
parsing. Real lookup errors remain terminal.

An unusable or changed source receipt defers that source and records a bounded,
typed admission failure. Healthy siblings may still publish. Existing parser,
consumer and storage failures retain their error identity; a failed scan is
not silently reported as complete.

## Raw-mirror and privacy behavior

Discovery-time whole-file raw capture is deferred for plain Codex JSONL.
After normal workspace and source gates, prefix capture reconstructs and
verifies exactly B bytes from the original object. It uses the existing raw
retention policy, chunk storage, verification and publication machinery.
The original path and origin remain the manifest identity.

The captured source length and content digest must equal the parser receipt.
Prefix captures bypass the whole-source metadata cache so B bytes cannot be
cached under a later N-byte fingerprint. Existing content-addressed chunks
remain reusable. A failed prefix capture never falls back to mirroring the
later whole file. Raw capture being disabled or paused retains its existing
optional behavior; it does not manufacture a successful raw capture.

Integration retains the upstream pre-lock source snapshot guard for ordinary
raw capture. A typed prefix can allow suffix appends while waiting for the
raw-store lock, but must revalidate its original object and admitted bytes.
Every typed-prefix capture bypasses whole-source caching, including one whose
receipt described settled EOF before a later append.

The existing raw manifest verifies reconstructed captured bytes, not the
current original file's EOF. Its `source_size_bytes` is B. Its source mtime is
an observation hint, not the identity of the admitted generation; the typed
canonical/ledger receipt supplies N, B, device/inode and digest. The raw
manifest schema and checksum contract are unchanged.

`CASS_RAW_MIRROR=off`, original-path exclusions, sensitive physical ZCode
store fences and retention budgets still apply. No environment exclusion is
unset for the temporary view. The pinned parser can therefore also reject a
staging location excluded by an operator; that is a fail-closed limitation.

## Regression matrix

| Area | Populated coverage added or retained |
| --- | --- |
| Reporter arms | Completed appends; writer repeatedly reopening; writer retaining its writable descriptor |
| Boundedness | Appends at every copy/hash heartbeat; N never grows; LF at 64 KiB boundaries; large unfinished suffix |
| Record endings | CRLF; incomplete JSON; torn UTF-8; valid final JSON without LF; later completion exactly once |
| Object/content races | Truncation, regrowth, replacement, symlink, restored mtime, same-size edits and rewrite-plus-append |
| Publication | Consumer failures, missing completion, queued mutation, late-append downgrade, durable partial receipts and replay |
| Deduplication | Stable session IDs, actual canonical consumer replay, normalized-prefix divergence refusal, legacy-row conflicts, identical records at distinct indices, sliced-transaction rollback |
| Policy | Private staging modes, original provenance, path exclusions, ZCode physical ownership and advisory locks |
| Raw mirror | Exact B-byte readback, original manifest identity, cache separation, forged view, post-copy original mutation |
| Recovery | Existing #560 inactive tail; transition from a published live prefix to abandoned-tail recovery |

`tests/codex_stable_prefix.rs` coordinates real writer threads through parser
progress callbacks so the concurrent appends are deterministic. The separate
`scripts/tests/codex_growing_repro.py` contains the free-running 20 MB + 0.5 MB
CLI experiment with 20 ms writers. Its search marker is appended after the
first index and before the live second index, avoiding an old-marker false
positive. It then checks the last append after the writer stops. The optional
inactivity-aging qualification mode is reported explicitly; it is not the
default reporter-timing experiment.

Existing source-failure tests still exercise errors before any complete
record is available, alongside new positive complete-prefix cases. Settled
EOF controls explicitly establish inactivity and retain their original
message assertions. Strict snapshot guards remain directly tested; they
were not relaxed to make the new path pass.

## Validation record

All execution used the repository-authorized DSR route, with the pinned
`nightly-2026-08-31` toolchain and unchanged dependency versions. The evidence
bundle includes exact commands, source/configuration/log hashes, terminal
statuses and the unsuccessful intermediate attempts.

| Lane | Executed result and scope |
| --- | --- |
| Unchanged baseline Codex contract | 218 passed, 1 ignored; strict Clippy passed. The aggregate command failed on three pre-existing rustfmt hunks. |
| Frozen original candidate: Codex contract | 238 passed, 0 failed, 1 ignored; strict Clippy with `-D warnings` and rustfmt checks passed. Includes the six new public stable-prefix tests with real coordinated writer threads and the exact pinned FAD parser. |
| Frozen original candidate: source-ledger matcher | 13 passed, 0 failed, 1 benchmark ignored. Executes the production observation matcher, not host publication or storage. |
| Python harness generation | 12 passed. Reporter script syntax and `--help` also passed; these do not execute its CLI experiment. |
| Full production library check | Both attempts stopped in unchanged Asupersync 0.5.0 before CASS compiled: allocation failure/SIGABRT at 4 GiB and 6 GiB per-process virtual-memory limits; Cargo returned 101. |

The original candidate's focused receipt is
`cass-prefix-final-focused/20261123T235219-QnrAMyOx/receipt.json`.
Its 41 focused inputs were byte-identical to the original frozen all-source
candidate. Those results do not qualify later follow-up edits.
The last full-check receipt is
`cass-prefix-full-check/20261123T235449-xGDRUIn1/receipt.json`.
The first interrupted baseline build lost its environment without a terminal
result; it is not counted as a pass or a test failure.

The focused Codex lane compiles the production Codex adapter and writer
probe with the real pinned parser, while omitting unrelated FAD SQLite and
cryptography features. It cannot establish that the complete CASS library
type-checks. In that original candidate, thirteen added host/watch/cache tests, eight
raw-mirror test entrypoints (including an isolated child helper), and six
canonical storage tests received source-level review but were not executed.
Later additions include twelve semantic-frontier/source-order regressions,
bounded-cache and source-admission tests, and a lock-wait raw-capture
regression; those additions also remain unexecuted. The real CLI
still/reopen/held experiment also remains unexecuted because no qualified
full CLI binary was built.

This is an implementation candidate with focused runtime evidence, not full
integration qualification. No release, real-watch latency, cross-platform
qualification, or broader-bead closure is claimed.

## Follow-up validation and integration

A second diagnostic snapshot reproduced two progress defects in the original
candidate: a later paired user response could replace an earlier event prompt,
and timestamp sorting could move an appended enrichment record ahead of
already published messages. Its three new tests failed while the 238 existing
native tests passed. Those failures motivate the semantic-frontier and
source-order changes; they are not post-fix success evidence.

The fuller build was retried with a 32 GiB virtual-address allowance, a
16 MiB compiler stack, one Cargo job and three observed Asupersync compiler
threads. Actual runtime-test stack configuration stayed at 128 MiB. A
100 ms watchdog preserved a 2 GiB available-memory reserve and a 5 GiB
aggregate RSS ceiling. It stopped the build with SIGTERM after 70.448 seconds,
before CASS compiled, when available memory reached 2,096,540 KiB. Peak
process-tree RSS was 5,239,540 KiB and peak VAS was 7,836,360 KiB. The DSR
exit was 125. This shows why the earlier VAS limits were premature, while
also establishing a real resident-memory limit for the available host.

The real pinned UBS v5.4.4 gate stage executed with genuine ast-grep 0.51.3,
verified runner/contract/module assets, and a successful Rust AST probe.
It scanned 27 files and reported 86 critical findings, 22,542 warnings
and 5,421 informational findings: **STAGE=ubs EXIT=1**. The gate is red.
Sampled findings include test panic macros and existing constants/helpers;
they have not all been classified as false positives. A new reporter-script
subprocess warning was fixed with a 15-second timeout and has not been
rescanned.

The execution environment later became unavailable while no compiler or
scanner was running; the cause is unknown. Follow-up changes and integration with upstream raw-policy, native web-action,
streaming-fingerprint and nonblocking source-admission fixes therefore have
source-level review only. No later rustfmt, Clippy, UBS, production compilation or runtime pass is
claimed. The original patch was recovered from its durable artifact and its
276,282-byte content matched SHA256
`97ec4c70edb985ffa8a8de623c8fd3e8dcc89419e9f594ee5957b399c87be923`.

The original evidence bundle preserves its original receipts and logs.
Later run metadata was recovered separately; their raw logs were in the
unavailable workspace and are not asserted to exist in that earlier bundle.
[The accompanying validation record](CODEX_STABLE_PREFIX_VALIDATION_2026-11-24.json)
distinguishes executed snapshots, failures, source-only changes and unavailable
qualification.

## Deliberate boundaries

- Stable-prefix identity is implemented for regular plain Unix Codex JSONL.
  Non-Unix, compressed JSONL and other formats retain their strict path.
- Advisory locks continue to defer. The broader bead's lock-bypass request
  is not implemented by weakening the existing lock guard.
- Replaced/truncated/rewritten published sources and non-monotonic normalized
  projections require reconciliation; this patch refuses divergent merges.
  It does not invent a general canonical rewrite/deletion protocol.
- An event prompt finalized by the existing inactivity policy can require
  reconciliation if a later paired response changes its published projection.
  A later richer web response that would replace an already published web
  completion likewise fails closed; ordinary call-then-completion appends
  retain the existing web-action handling. These cases do not rewrite history
  or advance an incompatible receipt.
- Existing opt-in layout-migration proof remains stricter than prefix
  admission and may defer a migrated active source.
- Copy and hashing buffers are fixed-size; total bytes read are bounded by
  the existing per-source limit. Full-prefix parsing is O(N) per attempt and
  retains the parser's existing normalized-message memory behavior. This is
  not a whole-process RSS guarantee or incremental parser checkpoint.
- Repeated identity and content verification is not an atomic filesystem
  transaction against a hostile writer that changes and restores bytes
  between observations. It provides verified captured-prefix evidence with
  conservative refusal of observed divergence.
