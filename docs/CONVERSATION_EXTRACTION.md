# Recover a conversation without restoring the database

`cass archive extract-conversation` recovers one canonical conversation row
and **every message row belonging to it** from an existing logical backup.
It does not open a database, search index, model, or provider source path.
This is useful when the original provider files are gone or the canonical
archive cannot be opened and restoring a whole database is not practical.

First obtain a conversation ID and the exact backup's `content_sha256` using
`cass archive search` or `cass archive verify`. IDs are scoped to that digest;
an ID from a different snapshot must not be reused without checking it.

Set `BACKUP_DIGEST` to that exact digest before the extraction command.

```sh
cass archive search history.jsonl --contains 'recovery design' --include-private
cass archive extract-conversation history.jsonl \
  --conversation-id 7 \
  --content-sha256 "$BACKUP_DIGEST" \
  --output recovered-conversation.jsonl \
  --include-private
```

The output path must be new and its parent directory must already exist. An
existing file, directory, or symlink is never replaced. On Unix the new file
is private (mode 0600). A private temporary file is discarded on failure.
The complete source, including unrelated records, completion digest and EOF,
is verified before publication. The staged output is synchronized, reread
through its original descriptor, and checked against its computed byte count
and SHA-256 before no-clobber publication and parent-directory synchronization.

## Exactly what is recovered

The output retains the complete `conversations` descriptor and selected row,
then the complete `messages` descriptor and every matching row in original
primary-key order. Message `idx` values are retained; they are not renumbered.
No preview limit, maximum-message count, or text truncation is applied.
`content`, `extra_json`, `extra_bin`, NULLs, integer values and exact REAL bit
encodings all retain their stored values. Additional columns are preserved,
not silently omitted. A selected conversation with no messages is valid; a
missing conversation, missing messages table, noninteger identity, or ambiguous
primary-key layout is refused.

This is a **typed transcript excerpt**, not a full logical archive, not a
provider-native session file, and not a database restoration input. Related
agent/workspace rows and other canonical tables are not copied; their IDs in
the conversation remain the original IDs. The output does not claim that those
external references can be resolved independently. Use ordinary `archive
import` for complete canonical restoration.

## Output format

The first JSONL record has `type: "conversation_extract"`,
`format: "cass.conversation_extract"`, and `schema_version: 1`. It carries the
original `source_header`, expected `source_content_sha256`, conversation ID,
`row_transport: "cass.logical_archive.v2"`, and `restorable_as_archive: false`.

Following records use the logical-archive typed `table` and `row` definitions.
A large row uses the existing v2 `row_start`, numbered `row_chunk`, `row_end`
transport with its row checksum. Each physical frame fits within 8 MiB; the
complete canonical encoded row remains bounded by 256 MiB. Decode continuation
groups before interpreting message fields. TEXT is exact UTF-8; BLOB cell
values use canonical standard base64, not paths to external files.

The last record has `type: "extraction_completion"`, conversation and message
row counts, source content digest, and the byte length and SHA-256 of all output
bytes **before** that final record. The JSON receipt on stdout also provides
`output_bytes` and `output_sha256` for the **entire** published file, including
the final record. These are integrity checksums, not signatures or proof of
who authored the history. No private message text appears in the receipt.

Memory scales with one bounded decoded row plus bounded encoding/I/O scratch,
not with the conversation's message count. This is not a whole-process RSS
ceiling. Recovery must scan the complete backup once; it does not offer indexed
random access. A completed extraction proves the logical source and copied
bytes, not the structural integrity of any database (`database_integrity_checked`
is false).
