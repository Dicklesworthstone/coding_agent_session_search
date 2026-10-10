# Extract complete message fields from a logical backup

`cass archive extract` recovers a single message field without restoring the
whole database or building a search index. Unlike `archive view`'s 64 KiB
terminal-response window, extraction writes the complete selected value to a
new private file. It reads version 1 and version 2 backups, including continued
large rows within the existing 256 MiB encoded-row limit.

First locate a message with `cass archive search`, or inspect the backup with
`cass archive verify`. Use the returned `content_sha256` to bind the numeric
message ID to that exact snapshot:

```sh
cass archive extract /backups/history.jsonl \
  --message-id 7 --content-sha256 "$BACKUP_DIGEST" \
  --output /recovery/message.txt --include-private
```

The default field is `content`. `--field extra-json` writes the exact stored
`extra_json` text, preserving whitespace and newlines rather than reformatting
JSON. `--field extra-bin` writes the exact decoded `extra_bin` BLOB:

```sh
cass archive extract /backups/history.jsonl \
  --message-id 7 --content-sha256 "$BACKUP_DIGEST" \
  --field extra-bin --output /recovery/message.bin --include-private
```

`extra_bin` is the stored binary representation, which may be compressed or
provider-specific. Extraction does not decompress it, infer its format, execute
it, or treat it as a filesystem path. NULL is not an empty value: selecting a
NULL field fails without publishing a file. An actual empty text/BLOB value
produces an empty file. Text remains UTF-8 with exact NUL and newline bytes; no
extra newline, ellipsis or truncation marker is added.

## Publication and privacy

`--include-private` is mandatory. Payload bytes go only to the explicitly named
file, not stdout; stdout contains one JSON receipt. Parent directories must
already exist. Output files, symlinks, directories and the original input are
never replaced. A persistent adjacent `.logical-archive.lock` file coordinates
competing publishers, as it does for archive export/import.

Extraction reads one pinned regular backup file. It validates every logical
record, ordering, count, continuation checksum, completion digest and EOF,
including all rows after the selected message. A matching row or valid prefix
is not enough. The selected value is written into a private temporary file,
then flushed, synchronized and read back for byte-count/SHA-256 verification.
Only then is the new file published without clobbering another destination.
Unix outputs have no group/other permissions. An ordinary failure before
publication removes the temporary plaintext. A killed process can leave its
private staging file, like other archive commands; an error synchronizing the
parent after publication can leave a complete output without a success receipt.

The receipt distinguishes the backup's `content_sha256` from the extracted
file's `output_sha256`, and reports `output_bytes`, the actual wire version,
message ID, stored field name, encoding, and `destination_status: "created"`.
It never includes body text or binary values. `integrity_verified: true` means
wire-integrity verification, not an audit of database relationships:
`database_integrity_checked`, `database_opened`, and `provider_files_opened` are
all false. This is a field extraction, not a restored database or conversation.

Work is linear in backup size. The shared decoder still materializes one
bounded logical row. Extraction adds fixed-size I/O and base64 decode buffers,
not a complete decoded BLOB copy. These are algorithmic bounds, not a measured
whole-process memory or latency guarantee. The existing `archive view` response
limits and all export/import behavior remain unchanged.
