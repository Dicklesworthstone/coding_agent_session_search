# Search-hit follow-up addressing (GH #493)

Search's legacy `line_number` is the stored message index plus one
(`messages.idx + 1`), not a physical JSONL file line. Metadata, progress records,
blank or malformed lines, tool records, and provider normalization can make the
coordinates diverge. Stored indexes may also be sparse.

## Follow a search hit

Use `--message-index`, passing search's `line_number` unchanged. Do not subtract
one. Preserve `source_path`, `source_id`, and `conversation_id` from the same hit
and use the same archive database. Check the installed `view --help` and
`expand --help`; do not translate `--message-index` to `--line` on older builds.

```sh
cass --db "$ARCHIVE_DB" search 'authentication error' \
  --mode lexical --no-maintenance --json --limit 5 \
  --fields source_path,source_id,conversation_id,line_number,agent

# Assign these variables from one hit, without mixing records.
cass --db "$ARCHIVE_DB" expand "$SOURCE_PATH" \
  --source "$SOURCE_ID" --conversation-id "$CONVERSATION_ID" \
  --message-index "$LINE_NUMBER" -C 3 --json

cass --db "$ARCHIVE_DB" view "$SOURCE_PATH" \
  --source "$SOURCE_ID" --conversation-id "$CONVERSATION_ID" \
  --message-index "$LINE_NUMBER" -C 3 --json --timeout 2000
```

Minimal and summary field presets retain the source and conversation identity.
Custom field lists must explicitly retain those fields for exact follow-ups.
`remote` and `all` are search filters, not exact follow-up source identifiers. Preserve the source path
verbatim; a conversation ID must match both that path and any supplied source.

## Exactness and provenance

The message-addressed lane opens the canonical archive read-only. A single read
transaction observes both conversation identity and its messages. It rejects
ambiguous paths (including empty conversations sharing a path), identity
mismatches, missing messages, and invalid or duplicate stored indexes. It does
not prefer a local conversation, choose the newest row, select a neighboring
index, reparse the live source, or fall back to raw file lines.

`-C` counts neighboring archived messages, not numeric index distance. `expand`
returns an array; `view` returns an object with a `lines` array. Message records
contain `message_index`, `message_id`, `conversation_id`, `source_id`, and
`coordinate_space: "message_index"`. Their content is the archived message text.
The `line`/`target_line` compatibility fields in this mode denote the declared
message coordinate, not a verified physical-file location. Never cite them as
raw-file lines. `view` additionally reports `content_source: "archive"`.

`is_target: true` means that the exact requested archive identity/index was
selected. It does not mean that a previous search predicate was rerun or that
an old search hit's content hash was checked. Re-run search after archive mutation;
a consistent follow-up snapshot does not make an earlier search part of that
same snapshot.

## Raw-file compatibility

`--line`, `-n`, `--line-number`, and `--line_number` retain their physical-file
meaning for readable local regular files (`expand` requires JSONL). Do not pass
search's `line_number` here. Explicit physical targets never fall back to an
archive or claim remote/conversation identity. Unanchored archive browsing
remains available through `view`; automation should use `--message-index`.

The two coordinates conflict, and `expand` requires one. Physical `expand`
rejects a blank, malformed, or out-of-range target instead of substituting
another JSON record and marking it `is_target`. Physical `view` displays literal
text, including blank lines, but rejects out-of-range targets. A valid explicit
raw-line read still works even when its content is unrelated to a previous query.

Message-mode reads have a default 10,000 ms budget, overridable through
`CASS_VIEW_BUDGET_MS`; `view --timeout` takes precedence. Lookup and output
encoding finish before anything is printed. A timeout produces no successful
target payload. Physical robot `view` may instead return a budget-marked partial
response containing request identity and no target content or unverified totals.

## Bounded follow-up work

Targeted follow-ups enforce separate retained-data limits: **8 MiB per physical
record or canonical role-plus-content pair**, **32 MiB of window text including
roles**, and **4096 retained records**. Limits apply to the actual retained
window, not merely the requested `-C` number. A large context on a small
transcript remains valid. Oversized requests return a non-retryable
`followup-resource-limit` error with no stdout target, rather than truncating a
message and presenting it as complete. Reduce context or inspect the source
with a streaming tool; increasing the timeout does not raise the byte limits.

Physical `view` counts and UTF-8-validates unselected lines without allocating
their contents, even if those lines exceed the record limit. Physical `expand`
with nonzero context must inspect earlier JSONL records to count valid neighbors;
those inspected records also obey the record limit. `-C 0` can discard earlier
physical lines without parsing them. Special files such as FIFOs are refused;
symlinks to regular files remain readable.

Canonical metadata selection caps retained anchors and still validates indices
through the transcript tail. Selected payloads are SQL byte-guarded before
transfer to the host and streamed into the window instead of collected twice.
Byte admission includes multibyte UTF-8 and text after embedded NUL characters;
malformed types, identity mismatches and oversized bodies remain errors.

Workers share the existing request deadline. Physical scanning checks it every
8 KiB; canonical scanning/hydration checks it between returned rows, and output
projection checks before and after encoding. These are cooperative stops, not
forced interruption of a blocked filesystem/database call. The limits bound
retained follow-up data, **not process-wide RSS, database-internal allocations,
or the execution time of an individual engine or kernel operation**. No archive
repair, index maintenance, or source mutation is performed by this lane.
