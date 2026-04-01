# Test Fixtures

This directory contains real fixture data for integration and E2E tests.
Fixtures follow the project's **no-mock policy** — see `TESTING.md`.

## Directory Overview

| Directory | Purpose | Test Scenarios |
|-----------|---------|----------------|
| [`aider/`](#aider) | Aider chat history format | Markdown history parsing |
| [`amp/`](#amp) | Amp thread format | JSON thread parsing |
| [`claude_code_real/`](#claude_code_real) | Real Claude Code sessions | JSONL session parsing |
| [`claude_project/`](#claude_project) | Claude project structure | Project directory discovery |
| [`cli_contract/`](#cli_contract) | CLI API contract fixtures | Version/capability stability |
| [`cline/`](#cline) | Cline task format | Multi-file task parsing |
| [`codex_real/`](#codex_real) | Real Codex CLI sessions | JSONL session parsing |
| [`connectors/`](#connectors) | Connector manifest | Fixture provenance tracking |
| [`gemini/`](#gemini) | Gemini session format | Hash-based directory parsing |
| [`html_export/`](#html_export) | HTML export test data | Edge cases, real sessions |
| [`install/`](#install) | Installation artifacts | Installer script testing |
| [`models/`](#models) | ONNX model fixtures | Embedding/reranker loading |
| [`multi_source/`](#multi_source) | Multi-machine sources | Source sync testing |
| [`opencode_json/`](#opencode_json) | OpenCode JSON format | Message/part/session parsing |
| [`pages_verify/`](#pages_verify) | Page verification | Security/validity checks |
| [`pi_agent/`](#pi_agent) | Pi Agent sessions | Session format parsing |
| [`search_demo_data/`](#search_demo_data) | Pre-indexed search data | Search/query testing |
| [`sources/`](#sources) | Sources subsystem | Probe result fixtures |

---

## Fixture Details

### aider/

**Scenario:** Parsing Aider's markdown-based chat history format.

| File | Description |
|------|-------------|
| `.aider.chat.history.md` | Minimal Aider session with `/add` command and refactoring request |

**Tested by:** `src/connectors/aider.rs`

---

### amp/

**Scenario:** Parsing Amp's JSON thread format.

| File | Description |
|------|-------------|
| `thread-001.json` | Sample thread with user/assistant messages and timestamps |

**Tested by:** `src/connectors/amp.rs`

---

### claude_code_real/

**Scenario:** Parsing real Claude Code JSONL session files.

| Path | Description |
|------|-------------|
| `projects/-test-project/agent-test123.jsonl` | Real session with matrix completion discussion |

**Tested by:** `src/connectors/claude_code.rs`, `tests/e2e_search_index.rs`

---

### claude_project/

**Scenario:** Discovering Claude Code project directories.

| Path | Description |
|------|-------------|
| `projectA/` | Sample project directory structure |

**Tested by:** `src/connectors/claude_code.rs` (project discovery)

---

### cli_contract/

**Scenario:** CLI API contract stability and backwards compatibility.

| File | Description |
|------|-------------|
| `api_version.json` | API version response fixture |
| `capabilities.json` | Feature capabilities (connectors, limits, features) |
| `introspect.json` | Full introspection response for CLI contract testing |

**Tested by:** `tests/e2e_cli_contract.rs`

**Use case:** Verifies CLI JSON output matches expected schema across versions.

---

### cline/

**Scenario:** Parsing Cline's multi-file task format.

| Path | Description |
|------|-------------|
| `task1/api_conversation_history.json` | API conversation history |
| `task1/task_metadata.json` | Task metadata |
| `task1/ui_messages.json` | UI-facing messages |

**Tested by:** `src/connectors/cline.rs`

---

### codex_real/

**Scenario:** Parsing real Codex CLI JSONL sessions.

| Path | Description |
|------|-------------|
| `sessions/2025/11/25/rollout-test.jsonl` | Codex CLI session fixture |

**Tested by:** `src/connectors/codex.rs`, `tests/e2e_search_index.rs`

---

### connectors/

**Scenario:** Provenance tracking for all connector fixtures.

| File | Description |
|------|-------------|
| `MANIFEST.json` | Central manifest with SHA256 checksums, capture dates, redaction policies |

**Use case:** Validates fixture integrity and documents data provenance.

---

### gemini/

**Scenario:** Parsing Gemini's hash-based directory structure.

| Path | Description |
|------|-------------|
| `hash123/` | Sample Gemini session directory |

**Tested by:** `src/connectors/gemini.rs`

---

### html_export/

**Scenario:** HTML export functionality with various edge cases.

| Subdirectory | Description |
|--------------|-------------|
| `edge_cases/` | Boundary conditions for HTML generation |
| `malformed/` | Invalid/corrupted input handling |
| `real_sessions/` | Real session data from 11 different agents |

**Edge case files:**

| File | Scenario |
|------|----------|
| `all_message_types.jsonl` | Every message type in one session |
| `empty_session.jsonl` | Zero messages |
| `large_session.jsonl` | 357KB session (performance test) |
| `single_message.jsonl` | Minimal valid session |
| `unicode_heavy.jsonl` | CJK, emoji, RTL text rendering |

**Real session coverage:**

| Agent | Session |
|-------|---------|
| Aider | `aider_bugfix.jsonl` |
| Amp | `amp_data_pipeline.jsonl` |
| ChatGPT | `chatgpt_react_help.jsonl` |
| Claude Code | `claude_code_auth_fix.jsonl` |
| Cline | `cline_vscode_setup.jsonl` |
| Codex | `codex_api_design.jsonl` |
| Cursor | `cursor_refactoring.jsonl` |
| Factory | `factory_code_generation.jsonl` |
| Gemini | `gemini_debugging.jsonl` |
| OpenCode | `opencode_rust_cli.jsonl` |
| Pi Agent | `pi_agent_personal_assistant.jsonl` |

**Tested by:** `tests/e2e_html_export.rs`, `src/html_export/`

---

### install/

**Scenario:** Testing the installation script with mock artifacts.

| File | Description |
|------|-------------|
| `coding-agent-search` | Mock Linux binary |
| `coding-agent-search.exe` | Mock Windows binary |
| `*.tar.gz`, `*.zip` | Mock release archives |
| `*.sha256` | Checksum files for integrity verification |

**Tested by:** `tests/e2e_install_easy.rs`

---

### models/

**Scenario:** ONNX model loading for semantic search and reranking.

See [`models/README.md`](models/README.md) for full documentation.

| Subdirectory | Description |
|--------------|-------------|
| Root files | Minimal valid ONNX model for unit tests |
| `xenova-paraphrase-minilm-l3-v2-int8/` | Real embedding model (~17MB) |
| `xenova-ms-marco-minilm-l6-v2-int8/` | Real reranker model (~22MB) |

**Tested by:** `src/search/embedder.rs`, `src/search/reranker.rs`, `tests/e2e_semantic.rs`

---

### multi_source/

**Scenario:** Multi-machine source synchronization.

| Subdirectory | Description |
|--------------|-------------|
| `local/` | Local machine sessions |
| `remote_laptop/` | Remote laptop sessions (different paths) |
| `remote_workstation/` | Remote workstation sessions |

**Tested by:** `tests/e2e_multi_source.rs`, `tests/e2e_sources.rs`

---

### opencode_json/

**Scenario:** Parsing OpenCode's JSON format at multiple levels.

| Subdirectory | Description |
|--------------|-------------|
| `message/` | Individual message fixtures |
| `part/` | Message part fixtures |
| `session/proj1/` | Complete session fixtures |

**Tested by:** `src/connectors/opencode.rs`

---

### pages_verify/

**Scenario:** Static page verification for security and correctness.

| Subdirectory | Test Case |
|--------------|-----------|
| `valid/site/` | Correctly structured exported site |
| `missing_required/` | Missing required files (should fail) |
| `missing_required_no_viewer/` | Missing viewer.html (should fail) |
| `secret_leak/` | Accidental credential exposure detection |
| `unencrypted/` | Unencrypted export verification |

**Tested by:** `tests/e2e_pages.rs`

---

### pi_agent/

**Scenario:** Parsing Pi Agent session format.

| Path | Description |
|------|-------------|
| `sessions/` | Pi Agent session fixtures |

**Tested by:** `src/connectors/pi_agent.rs`

---

### search_demo_data/

**Scenario:** Pre-indexed search data for query testing.

| File/Dir | Description |
|----------|-------------|
| `agent_search.db` | Pre-populated SQLite database |
| `index/v1/` | Legacy index format (migration testing) |
| `index/v7/` | Current index format |
| `watch_state.json` | File watcher state fixture |

**Tested by:** `tests/e2e_search_index.rs`, `src/search/query.rs`

---

### sources/

**Scenario:** Sources subsystem probe fixtures.

See [`sources/probe/README.md`](sources/probe/README.md) for full documentation.

| Subdirectory | Description |
|--------------|-------------|
| `probe/` | HostProbeResult JSON fixtures for various states |

**Tested by:** `src/sources/probe.rs`, `tests/e2e_sources.rs`

---

## Adding New Fixtures

1. **Use real data** — Capture from actual agent sessions when possible
2. **Redact sensitive info** — Anonymize usernames, paths, credentials
3. **Add to MANIFEST** — Update `connectors/MANIFEST.json` with SHA256
4. **Document the scenario** — Update this README and add fixture-specific README if complex
5. **Link to tests** — Note which test files exercise the fixture

## Fixture Loading Helpers

Common fixture loading utilities are in `tests/fixture_helpers.rs`:

```rust
use fixture_helpers::{
    load_fixture,                    // Load fixture file as string
    copy_fixture,                    // Copy fixture to temp location
    embedder_fixture_dir,            // Path to embedding model fixtures
    reranker_fixture_dir,            // Path to reranker model fixtures
    verify_model_fixture_checksums,  // Verify ONNX model integrity
    setup_connector_test,            // Create temp test environment
    create_project_dir,              // Create project within test env
    write_session_file,              // Write session file to test env
};
```

## No-Mock Policy

Per `TESTING.md`, this project avoids mocks in favor of real fixtures:

- **Connector tests** use real session files from each agent
- **Database tests** use real SQLite with known data
- **HTTP tests** use embedded servers with fixture responses
- **Model tests** use real (quantized) ONNX models

This approach provides higher confidence that tests reflect production behavior.
