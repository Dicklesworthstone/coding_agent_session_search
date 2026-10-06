# GH501: local identity receipt admission — uncompiled source candidate

This directory retains the GH501 implementation candidate in source control.
It is **not wired into CASS's production build**, not applied to frankensearch,
and not a completed fix. CASS still pins frankensearch 0.7.1 / Quill 0.4.0,
which does not expose the candidate admission API. No dependency pin, lockfile,
registry checksum, build guard, or production integrity policy is changed here.

Native Rust compilation, native regression execution, end-to-end CASS integration,
and performance measurements remain outstanding. Passing the Python installer
tests is not evidence that the Rust code compiles or that search is faster.

## Implementation

`frankensearch/` contains the proposed Quill receipt store, read-only admission
path and native regression sources. The companion CASS bridge and checked source
installer are kept beside them so the changes can be reviewed and qualified
against complete checkouts before publication and adoption.

The proposed Quill API is `KeeperSnapshot::open_with_local_receipts`, selected
for read-only open and refresh by `read_open_receipt_directory`. Ordinary
`KeeperSnapshot::open`, writers, recovery, doctor and maintenance remain strict.
Only a previously successful **file-prefix hash** is reused. Container and
MANIFEST validation and ordinary lazy section checks are not bypassed.

Receipts bind schema, MANIFEST segment ID, length and expected XXH3 to the mapped
file descriptor's device, inode, length, nanosecond mtime and nanosecond ctime.
The descriptor is checked during admission and after segment binding. New proof
state is persisted only after complete snapshot admission succeeds.

The CASS selector proposed by the companion bridge is:

```sh
CASS_LEXICAL_VERIFY=full
CASS_LEXICAL_VERIFY=local-receipts CASS_LEXICAL_RECEIPT_DIR=/absolute/private/cache
```

These are **candidate controls, not supported controls in the current CASS
binary**. Full verification is the default and unconditional override. The
receipt directory must already exist, be owner-controlled mode 0700, and be
outside the index. Unusable proof storage falls back to full verification.

## Integrity boundary

This is an explicitly weaker, optional policy, not proof that current bytes equal
previously verified bytes. Metadata-preserving media corruption and stores through
an already dirty shared writable mapping are not detected by identity matching.
Lazy checks still protect sections accessed; a strict open is needed for a fresh
full-file scan. Published memory-mapped segments must remain immutable under either
policy. No concurrent-mutation or truncation safety is claimed.

Reuse is restricted to known Linux-local filesystems. Other platforms and unknown,
network, FUSE and overlay filesystems verify in full. Reboot, producer/package
version and format changes invalidate the book. Newly changed files wait 60 seconds;
receipts expire after one hour, and hits do not renew them. Expiry uses wall-clock
seconds: a rollback that stays after verification time can extend effective TTL.
Future-dated receipts are rejected.

Receipt books are bounded to 1 MiB and checksummed with SHA-256. This checksum
detects damaged cache state, **not forgery by the trusted owner**. Descriptor-relative,
no-follow/nonblocking reads and atomic replacement operate in a retained private
directory; receipt files must be owner-only, mode 0600, single-link regular files.
Missing, malformed, expired or mismatched proof state only costs full verification.

## Qualification and release boundary

Before enabling this in CASS:

1. Apply the source candidate to complete, current frankensearch checkouts and
   review every transformation. The installer refuses missing/ambiguous anchors;
   its small fixture tests do not prove compatibility with a current checkout.
2. Format, compile, run the native receipt and keeper regressions, the complete
   Quill library suite and clippy, without weakening existing gates.
3. Publish the qualified engine normally, update CASS's official dependency and
   lockfile, then apply and qualify the search-only bridge. Do not introduce a
   path/git override or fabricate a registry version to bypass publication.
4. Measure representative fresh-process and retained-reader workloads. Report
   child CPU separately from wall time. No speedup is claimed by this directory.

The original source anchors were inspected against frankensearch
`33a4ab9dfcad7b45c75b4aa8cb55661ae813bce4`; the complete keeper/index sources were
not materialized in that editing environment. The source remains uncompiled.

The native regression sources assert actual prefix-hash counts, strict bypass,
replacement, truncation, same-length rewriting, restored mtime, damaged proof
state, and preservation of lazy section failures beneath a valid prefix witness.
Positive Linux integration cases require writable supported `/dev/shm`; they do
not silently skip the receipt-hit assertion. Mappings are dropped before mutation.

## Evidence

The source-candidate preparation and commit retry both ran the seven Python
installer fixture tests successfully. A separate Linux filesystem probe confirmed
identity changes for rewriting, restored mtime, truncation and pathname replacement.
Those are filesystem assumptions and Python results, not native Quill qualification.

GH501 remains unresolved until the engine, published dependency and CASS integration
are qualified and actual admission work/performance is measured.
