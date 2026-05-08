# Dependency Upgrade Log

**Date:** 2026-05-08 | **Project:** coding_agent_session_search | **Language:** Rust

## Summary

- **Updated:** 1
- **Skipped:** 0
- **Failed:** 0
- **Needs attention:** 0

## Updates

### franken-agent-detection: 029253c450702a1714fca4fb34ba290f2cc71d87 -> 7e288f493631020a4660443c5ad8fc7d4e49faa7

- **Reason:** Fix Dicklesworthstone/coding_agent_session_search#214 — the Claude Code connector hardcoded `~/.claude/projects` and ignored both `CLAUDE_CONFIG_DIR` and `XDG_CONFIG_HOME`, so caam-isolated profiles (which set both vars per account) and any user with XDG-redirected config were invisible to `cass`.
- **Upstream commit:** `7e288f4 feat(connectors/claude_code): honor CLAUDE_CONFIG_DIR + XDG_CONFIG_HOME` — adds the same env-var precedence chain the codex connector already uses (`CODEX_HOME` -> default `~/.codex`).
- **Breaking changes:** None. The default branch of `projects_root()` is unchanged when neither env var is set; the public crate version is still `0.1.3` and the feature set is unchanged.
- **Update command:** `cargo update -p franken-agent-detection --precise 7e288f493631020a4660443c5ad8fc7d4e49faa7`
- **Build contract:** `build.rs` `expected_rev` for the `franken_agent_detection` `DependencyContract` updated to keep `cargo build`'s `StrictOptIn` validation green.
- **Tests:** `cargo check --lib` clean. The connector's own test suite (`cargo test --features connectors,cursor,chatgpt,opencode,crush --lib 'connectors::claude_code'`) passes 51/51 — the existing `projects_root_returns_claude_projects_path` test was relaxed in the upstream commit to tolerate any of the three valid forms (default / XDG / explicit), mirroring the codex pattern.
