# coding-agent-search

Unified TUI for local coding-agent history search (Codex, Claude Code, Gemini CLI, Cline, OpenCode, Amp).

## Toolchain & dependency policy
- Toolchain: pinned to latest Rust nightly via `rust-toolchain.toml` (rustfmt, clippy included).
- Crates: track latest releases with wildcard constraints (`*`). Run `cargo update` regularly to pick up fixes.
- Edition: 2024.

## Env loading
Load `.env` at startup using dotenvy (see `src/main.rs`); do not use `std::env::var` without calling `dotenvy::dotenv().ok()` first.

## Dev commands (nightly)
- `cargo check --all-targets`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --check`

## Install
- Shell (Linux/macOS): `curl -fsSL https://raw.githubusercontent.com/coding-agent-search/coding-agent-search/main/install.sh | sh`  
  Flags: `--version vX.Y.Z`, `--dest DIR`, `--easy-mode`, `OWNER/REPO override`, `--checksum` or `--checksum-url`. The installer *requires* verification: by default it fetches `<tar>.sha256` next to the artifact; override with `--checksum` if you already have it.
- PowerShell (Windows): `irm https://raw.githubusercontent.com/coding-agent-search/coding-agent-search/main/install.ps1 | iex` with the same checksum rules (defaults to `<zip>.sha256`).
- Homebrew: `brew install coding-agent-search` (formula refuses to install while `sha256` is a placeholder—set real SHA before publishing).
- Releases: built via cargo-dist (`.github/workflows/dist.yml`). When publishing, upload the platform archives **and** matching `.sha256` files; keep README links and the Homebrew formula in sync.

## Usage (TUI & indexing)
- Quickstart: `coding-agent-search index --full` (first run) then `coding-agent-search tui`.
- Toggle detailed hotkey legend with `?` (initially shown). Open selected hit in your editor with `o` (uses `$EDITOR` + `$EDITOR_LINE_FLAG`, defaults `vi` and `+` for line jumps).
- Filter hotkeys: `a/w/f/t` to add agent/workspace/time filters, uppercase `A/W/F` to clear each, `x` to clear all; filter pills show their clear keys.
- Indexing full rebuild: `coding-agent-search index --full` truncates SQLite tables and Tantivy, then re-ingests—never deletes source logs.
- Incremental watch: `coding-agent-search index --watch` registers filesystem watchers on all known connector roots. Changes are routed to the relevant connector only, using per-connector mtime high-water marks (`since_ts`) to avoid full rescans.
- Completions/man: `coding-agent-search completions <shell>`, `coding-agent-search man`.

## Structure (scaffold)
- `src/main.rs` – entrypoint wiring tracing + dotenvy
- `src/lib.rs` – library entry
- `src/config/` – configuration layer
- `src/storage/` – SQLite backend
- `src/search/` – Tantivy/FTS
- `src/connectors/` – agent log parsers
- `src/indexer/` – indexing orchestration
- `src/ui/` – Ratatui interface
- `src/model/` – domain types

## Connectors & watch coverage
- Codex CLI: `~/.codex/sessions/**/rollout-*.jsonl`
- Cline: VS Code globalStorage `saoudrizwan.claude-dev` task dirs
- Gemini CLI: `~/.gemini/tmp/**`
- Claude Code: `~/.claude/projects/**` + `.claude` / `.claude.json`
- OpenCode: `.opencode` SQLite DBs (project/local/global)
- Amp: `sourcegraph.amp` globalStorage + `~/.local/share/amp` JSON caches

Watch mode listens on these roots and triggers connector-specific scans; `--full` truncates DB/index before ingest (non-destructive to log files).
