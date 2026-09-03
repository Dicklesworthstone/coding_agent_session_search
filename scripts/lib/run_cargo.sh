# scripts/lib/run_cargo.sh — shared rch-wrapped cargo invocation helper.
#
# Per `coding_agent_session_search-tafss`. This file is INTENTIONALLY meant to
# be `source`d, not executed. It exposes `run_cargo` as a function and sets
# default values for `RCH_BIN` and `RCH_TARGET_DIR`.
#
# Usage:
#     # at top of a shell script
#     source "$(dirname "$0")/../lib/run_cargo.sh"
#     run_cargo build --release
#     run_cargo test --test foo -- --nocapture
#
# Why this exists:
#   AGENTS.md mandates that all `cargo` invocations route through `rch exec --`
#   to offload compilation to the remote build cluster. Inline `run_cargo()`
#   definitions in individual scripts have drifted (different env defaults,
#   different rch-resolution logic, different stderr handling). This helper
#   centralizes the pattern.

# Resolve RCH_BIN. Prefer ${RCH_BIN}, fall back to `rch` on PATH, then
# the default user-local install location ~/.local/bin/rch.
: "${RCH_BIN:=$(command -v rch || true)}"
if [ -z "$RCH_BIN" ]; then
    if [ -x "$HOME/.local/bin/rch" ]; then
        RCH_BIN="$HOME/.local/bin/rch"
    else
        RCH_BIN=""  # signal that rch is not installed; run_cargo will fail loudly
    fi
fi

# Default target dir: per-script via the calling script's RCH_TARGET_DIR env.
# Falls back to a stable shared dir so different invocations can reuse build
# artifacts when run on the same machine.
: "${RCH_TARGET_DIR:=/tmp/cass-rch-target}"

# run_cargo: thin wrapper around `rch exec -- env CARGO_TARGET_DIR=... cargo "$@"`.
# Logs a structured invocation line to stderr on every call so failures are
# easy to reproduce.
run_cargo() {
    if [ -z "$RCH_BIN" ]; then
        if [ "${RCH_REQUIRE_REMOTE:-}" = "1" ]; then
            printf '[run_cargo] ERROR: rch not found on PATH or in ~/.local/bin/rch\n' >&2
            printf '[run_cargo] hint: install rch first, or set RCH_BIN=/path/to/rch\n' >&2
            return 127
        fi
        printf '[run_cargo] WARN: rch not found; failing open to local cargo (set RCH_REQUIRE_REMOTE=1 to fail closed)\n' >&2
        env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
        return $?
    fi
    printf '[run_cargo] cmd=cargo %s cwd=%s target=%s rch_bin=%s\n' \
        "$*" "$PWD" "$RCH_TARGET_DIR" "$RCH_BIN" >&2
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

# Export so subshells inherit the function (POSIX `export -f` is a bashism;
# this script is bash-targeted, which AGENTS.md already mandates).
export -f run_cargo 2>/dev/null || true
export RCH_BIN RCH_TARGET_DIR
