# PLAN_TO_PORT_INSTALL_SCRIPTS_TO_RUST.md

## Scope decision — 2026-09-17

`coding_agent_session_search-2l1b0.27` retires the standalone Rust port as a
current implementation requirement. Keep `install.sh` and `install.ps1` as the
supported bootstrap entry points, governed by [the installer spec](../INSTALLER_SPEC.md).
They can install cass before a Rust toolchain or cass executable exists. A
second installer binary would require another bootstrap and distribution path;
the language change itself does not supply a missing installation capability.
There is no implemented `cass install` or `cass-installer` promised by this plan.

This decision does not certify the existing installers on every platform.
`coding_agent_session_search-2l1b0.25` retains exact-artifact installation,
update, failure-recovery and platform acceptance. Its dependencies include the
DSR release path (`yviq2`), Windows Quill admission (`aegfi`) and Windows exit
failure (`4w0ma`). The tests below are source coverage, not a fresh passing
release receipt; the September 17 dependency graph cannot yet resolve
FrankenSearch 0.6.1.

| Original requirement | Current route and remaining acceptance |
|---|---|
| Latest-version lookup and explicit version | `install.sh::resolve_version` uses the release API, then tags; `--version` bypasses lookup. PowerShell has its own version selection. The proposed redirect-based algorithm is superseded, not claimed implemented. Exact selected-version/platform proof remains in `.25`. |
| OS/architecture artifact selection | Existing shell/PowerShell target selection; `tests/install_scripts.rs` covers GNU floor and artifact policy. Actual supported-target binaries remain `.25` acceptance. |
| SHA256 verification and missing artifacts | Existing checksum and fail-closed download paths; `install_sh_succeeds_with_valid_checksum`, `install_sh_fails_with_bad_checksum`, aggregate-checksum tests and PowerShell counterparts. No source fallback from an explicit artifact override. |
| Extraction and destination install | Existing private extraction and installation paths; traversal, symlink and special-entry rejection tests in `tests/install_scripts.rs`. Platform replacement/recovery proof remains `.25`. |
| Optional PATH update and easy mode | Existing `maybe_add_path` and PowerShell behavior; `tests/e2e_install_easy.rs`. No new Rust-owned profile mutation. |
| Optional version self-test | Existing `--verify`/PowerShell verification; `verify_flag_runs_self_test`, `verify_flag_rejects_a_binary_whose_version_probe_fails`, and `powershell_verify_contract_fails_closed_on_native_command_errors`. |
| Concurrent installation locking | Existing installer lock paths; `concurrent_installs_are_serialized`. Cross-platform lock/recovery acceptance remains `.25`. |
| Rust modules, binary and script-parity suite | Retired implementation mechanism; no new binary or parallel installer framework is scheduled. Preserve the historical proposal below for reference. |
| Keep scripts, artifact names, package-manager flows; no service or telemetry | Retained constraints. This decision changes none of those implementations or release interfaces. |

Reconsider a Rust port only for a concrete unmet capability that cannot be
maintained in the current entry points, with its own bootstrap and platform
acceptance. Closing the scope-decision bead means this choice is documented;
it does not close `.25` or claim a Rust installer was delivered.

## Historical proposal (superseded)

## Goal
Port the current shell/PowerShell installers (`install.sh`, `install.ps1`) to a
single Rust-based installer while preserving behavior and UX. The Rust installer
should be cross-platform and re-usable by the release workflow and docs.

## Legacy Inputs (Spec Sources)
- `install.sh` (bash)
- `install.ps1` (PowerShell)

## Scope (Inclusions)
- Resolve latest version via GitHub API with redirect fallback
- Support explicit version override
- Download correct release artifact per OS/arch
- Verify SHA256 checksum (direct or from `*.sha256` URL)
- Extract and install `cass` binary to destination
- Optional PATH update in “easy mode”
- Optional `--verify` run to print version
- Safe locking to prevent concurrent installs

## Exclusions
- Do NOT remove or modify `install.sh` or `install.ps1`
- Do NOT change release workflow assets or naming
- Do NOT add background services or telemetry
- Do NOT alter package-manager flows (Homebrew/Scoop)

## Output Artifacts
- New Rust installer command or binary
- Conformance tests that assert identical behavior to scripts
- Updated docs pointing to the Rust installer (optional, after parity)

## Phase 1 — Essence Extraction (Spec)
Extract and document exact behaviors, defaults, and edge cases:
- Version resolution flow + fallback version
- Artifact naming and URL construction
- OS/arch detection rules
- Checksum verification rules
- PATH update rules and prompts
- Locking behavior and stale lock recovery

## Phase 2 — Proposed Architecture
- Module layout:
  - `installer::version` (API + redirect lookup)
  - `installer::artifact` (target resolution)
  - `installer::download` (HTTP + checksum)
  - `installer::extract` (tar/zip)
  - `installer::install` (copy + permissions + PATH update)
  - `installer::lock` (cross-platform lock file)
- CLI surface: `cass install` or `cass-installer` with parity flags
- Error handling: structured, actionable messages (no stack traces by default)

## Phase 3 — Implementation
- Implement spec-driven modules in Rust
- Preserve flags and defaults from legacy scripts
- Add platform-specific install paths and PATH mutation rules

## Phase 4 — Conformance + QA
- Fixture-based tests comparing Rust installer behavior to legacy scripts
- Explicit coverage for:
  - Unknown arch fallback
  - Checksum mismatch
  - Missing artifact
  - PATH update (easy mode on/off)
  - Verify flag

## Risks + Mitigations
- Platform differences → use explicit target mapping + tests
- Partial installs → atomic temp directories + rename
- Concurrency → lock file + stale lock recovery

## Done When
- Rust installer passes conformance tests against scripts
- Docs can recommend Rust installer without regressions
- Scripts remain as fallback (no removal)
