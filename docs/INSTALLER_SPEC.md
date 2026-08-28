# Installer Spec (UBS-style) for coding-agent-search

## Goals
- One-line curl|bash / pwsh that installs coding-agent-search safely.
- Default: non-interactive install with PATH guidance; prebuilt artifacts require a checksum.
- Easy mode: fully non-interactive with safe defaults.
- Works on Linux/macOS; PowerShell path for Windows.
- When source bootstrap is needed, the installer requests the exact toolchain and components pinned by the release's `rust-toolchain.toml`.
- Uses only tar.gz/zip + sha256; optional minisign later.
- Never deletes user files; cleanup is limited to installer-owned temporary files and locks.

## UX
- Colorful logging (✓/✗/→/⚠); quiet flag to silence info.
- Lock file to prevent concurrent runs; temp workdir cleaned on exit.
- DEST default: ~/.local/bin (user) or --system for /usr/local/bin.
- PATH guidance; easy mode appends PATH to writable existing `.zshrc`/`.bashrc` files.
- Self-test flag `--verify` runs `cass --version`; `--quickstart` runs `cass index --full`.

## Inputs
- Flags: --easy-mode, --dest DIR, --system, --quiet, --verify, --quickstart, --version vX, --artifact-url, --checksum, --checksum-url, --from-source.
- Env: ARTIFACT_URL, CHECKSUM, CHECKSUM_URL (override), OWNER, REPO, FALLBACK_VERSION, DEST, and RUSTUP_INIT_SKIP (power users).

## Safety invariants
- Always verify prebuilt artifacts; fail closed if their checksum is missing or unreadable.
- If rustup or the pinned toolchain is required: prompt in normal mode; proceed silently in easy mode.
- Do not rm existing files; overwrite only target binary via install(1) with 0755.
- Exit non-zero on any verification failure.

## Flow (bash)
1) Resolve a writable temporary root and acquire the installer lock.
2) Resolve artifact URL: default GitHub release `cass-${OS}-${ARCH}.tar.gz`; allow an explicit override.
3) Fetch the artifact to a private temp dir; fetch its checksum (or use an override); verify via sha256sum, shasum, or openssl.
4) For source builds, clone the requested release first, bootstrap rustup without a default toolchain if needed, then install the exact toolchain and components declared by that checkout's `rust-toolchain.toml`.
5) Validate archive paths and entry types, extract to the temp dir, install the binary to DEST, and provide or apply PATH guidance.
6) Self-test if --verify; run the default full index if --quickstart.
7) Print next steps + how to run TUI/headless.

## Flow (PowerShell)
- Mirrors the prebuilt-artifact path: download zip, require a checksum, honor ArtifactUrl/Checksum overrides, and provide PATH guidance. PowerShell does not build from source.

## Open items
- Minisign integration (fail-closed when pubkey provided).
- Watch-mode e2e quickstart optional.
