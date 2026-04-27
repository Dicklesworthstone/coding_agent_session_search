# PLAN_TO_PORT_INSTALL_SCRIPTS_TO_RUST.md

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
