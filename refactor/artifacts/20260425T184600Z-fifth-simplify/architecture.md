# Fifth Loop Architecture Notes

## Purpose

`cass` is a Rust 2024 CLI/TUI that indexes local and remote coding-agent
conversation histories, normalizes them into a shared model, persists them to
SQLite via frankensqlite, and serves human/TUI plus robot JSON search surfaces.

## Main Layers

- Entry/runtime: `src/main.rs` handles fatal-error formatting, robot-mode error
  envelopes, AVX/runtime setup, and delegates parsed commands into `src/lib.rs`.
- CLI contract: `src/lib.rs` defines clap commands, aliases, robot docs,
  structured error kinds, command execution, health/status/doctor surfaces,
  search/export/timeline/sources/models/pages/analytics dispatch, and many
  CLI contract tests.
- Connectors: `src/connectors/*` wraps `franken_agent_detection` and per-agent
  adapters that emit normalized conversations/messages/snippets.
- Storage: `src/storage/sqlite.rs` is the durable archive layer. Per repo rules,
  new SQLite work must use frankensqlite/compat APIs rather than new rusqlite.
- Indexing: `src/indexer/*` scans connectors, writes canonical storage,
  updates semantic data, and rebuilds/publishes lexical assets with atomic
  swap/backup semantics.
- Search: `src/search/*` owns lexical/vector/semantic readiness, policy,
  query execution, model management, reranking, and daemon client integration.
- Presentation: `src/ui/*` implements the FrankenTUI app; `src/pages/*` and
  `src/pages_assets/*` generate browser/export pages; `src/html_export/*`
  renders self-contained encrypted or plain HTML session exports.
- Operations: `src/sources/*` handles remote source setup/probing/sync and path
  mappings; `src/analytics/*` computes dashboard/bucketing/reporting data.

## Data Flow

```
CLI/TUI command
  -> command dispatch in src/lib.rs
  -> connector discovery / source sync / session scan
  -> normalized Conversation/Message/Snippet model
  -> frankensqlite archive as source of truth
  -> derived lexical/vector/semantic search assets
  -> robot JSON, TUI views, exports, pages, analytics
```

## Refactor Safety Boundaries

- Public robot JSON, error envelopes, exit-code kind strings, and golden docs
  are stable user contracts.
- Search indexes are derived assets; refactors must not add deletion-oriented
  repair flows or manual index removal recipes.
- Connector parsing preserves source provenance, timestamps, roles, and tool
  flattening semantics.
- HTML export and key-management changes need byte/string contract tests
  because files are portable artifacts.
- Any simplification crossing async, I/O, database, or search-ranking boundaries
  is high risk and should be avoided in this loop unless thoroughly pinned.

