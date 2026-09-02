#!/usr/bin/env bash
# Documentation validation script for cass.
#
# Validates:
# - Link validity in markdown files
# - Required sections in README
# - CLI help text consistency
# - Example code validity
#
# Usage:
#   ./scripts/validate_docs.sh           # Run all validations
#   ./scripts/validate_docs.sh --links   # Only check links
#   ./scripts/validate_docs.sh --readme  # Only check README
#   ./scripts/validate_docs.sh --help    # Only check CLI help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RCH_BIN="${RCH_BIN:-rch}"
RCH_TARGET_DIR="${RCH_TARGET_DIR:-${TMPDIR:-/tmp}/rch_target_cass_validate_docs}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Counters
ERRORS=0
WARNINGS=0
CHECKS=0

# =============================================================================
# Helper Functions
# =============================================================================

log_pass() {
    ((CHECKS += 1))
    echo -e "${GREEN}✓${NC} $1"
}

log_fail() {
    ((CHECKS += 1))
    ((ERRORS += 1))
    echo -e "${RED}✗${NC} $1"
}

log_warn() {
    ((WARNINGS += 1))
    echo -e "${YELLOW}!${NC} $1"
}

log_info() {
    echo -e "  $1"
}

ensure_rch() {
    if ! command -v "$RCH_BIN" &> /dev/null; then
        log_fail "rch binary not found; validate_docs cargo work must be offloaded"
        return 1
    fi
}

run_cargo() {
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$RCH_TARGET_DIR" cargo "$@"
}

section() {
    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo " $1"
    echo "═══════════════════════════════════════════════════════════════"
}

# =============================================================================
# Link Validation
# =============================================================================

check_links() {
    section "Link Validation"

    local md_files
    md_files=$(find . -name "*.md" -not -path "./target/*" -not -path "./.git/*" 2>/dev/null || true)

    if [[ -z "$md_files" ]]; then
        log_warn "No markdown files found"
        return
    fi

    local file
    for file in $md_files; do
        log_info "Checking $file..."

        # Check for broken internal links (relative paths)
        local links
        links=$(grep -oE '\[([^]]+)\]\(([^)]+)\)' "$file" 2>/dev/null | grep -v 'http' | grep -v 'mailto' || true)

        while IFS= read -r link; do
            [[ -z "$link" ]] && continue

            # Extract the path from the link
            local path
            path=$(echo "$link" | sed -E 's/.*\]\(([^)#]+).*/\1/')

            # Skip anchors and empty paths
            [[ -z "$path" || "$path" == "#"* ]] && continue

            # Resolve relative to file directory
            local dir
            dir=$(dirname "$file")
            local full_path="$dir/$path"

            if [[ ! -e "$full_path" && ! -e "$path" ]]; then
                log_fail "Broken link in $file: $path"
            fi
        done <<< "$links"

        # Check for valid URL patterns in external links
        local urls
        urls=$(grep -oE 'https?://[^)"\s>]+' "$file" 2>/dev/null || true)

        while IFS= read -r url; do
            [[ -z "$url" ]] && continue

            # Basic URL format validation
            if ! echo "$url" | grep -qE '^https?://[a-zA-Z0-9]'; then
                log_fail "Malformed URL in $file: $url"
            fi
        done <<< "$urls"
    done

    log_pass "Link validation complete"
}

# =============================================================================
# README Validation
# =============================================================================

check_readme() {
    section "README Validation"

    local readme="README.md"

    if [[ ! -f "$readme" ]]; then
        log_fail "README.md not found"
        return
    fi

    log_info "Checking required sections..."

    # Check for key sections
    local sections=("installation" "usage" "features" "license")

    for sec in "${sections[@]}"; do
        if grep -qi "## .*$sec\|# .*$sec" "$readme"; then
            log_pass "README has $sec section"
        else
            log_warn "README may be missing $sec section"
        fi
    done

    # Check for examples
    if grep -q '```' "$readme"; then
        log_pass "README contains code examples"
    else
        log_warn "README has no code examples"
    fi

    # Check for badges (optional)
    if grep -qE '!\[.*\]\(https?://' "$readme"; then
        log_pass "README has badges/images"
    else
        log_info "README has no badges (optional)"
    fi

    # Check file isn't empty or too short
    local lines
    lines=$(wc -l < "$readme")
    if [[ "$lines" -lt 20 ]]; then
        log_warn "README seems short ($lines lines)"
    else
        log_pass "README has adequate content ($lines lines)"
    fi
}

# =============================================================================
# CLI Help Validation
# =============================================================================

check_help() {
    section "CLI Help Validation"

    # Check if binary exists
    local binary="$RCH_TARGET_DIR/release/cass"
    if [[ ! -x "$binary" ]]; then
        binary="$RCH_TARGET_DIR/debug/cass"
    fi
    if [[ ! -x "$binary" ]]; then
        binary="target/release/cass"
    fi
    if [[ ! -x "$binary" ]]; then
        binary="target/debug/cass"
    fi

    if [[ ! -x "$binary" ]]; then
        log_warn "cass binary not found, building through rch..."
        ensure_rch || return
        run_cargo build --quiet --bin cass 2>/dev/null || {
            log_fail "Could not build cass binary"
            return
        }
        binary="$RCH_TARGET_DIR/debug/cass"
        if [[ ! -x "$binary" ]]; then
            log_fail "Built cass binary not found at $binary"
            return
        fi
    fi

    log_info "Using binary: $binary"

    # Test --help
    if "$binary" --help &>/dev/null; then
        log_pass "--help flag works"
    else
        log_fail "--help flag failed"
    fi

    # Test -h
    if "$binary" -h &>/dev/null; then
        log_pass "-h flag works"
    else
        log_fail "-h flag failed"
    fi

    # Test --version
    local version_output
    version_output=$("$binary" --version 2>&1 || true)
    if echo "$version_output" | grep -qE '[0-9]+\.[0-9]+\.[0-9]+'; then
        log_pass "--version shows version number"
    else
        log_fail "--version doesn't show version number"
    fi

    # Test subcommand help
    local subcommands=("search" "index" "export" "tui" "health")
    for cmd in "${subcommands[@]}"; do
        if "$binary" "$cmd" --help &>/dev/null; then
            log_pass "Subcommand '$cmd' has help"
        else
            log_warn "Subcommand '$cmd' help unavailable"
        fi
    done

    # Check help mentions key features
    local help_output
    help_output=$("$binary" --help 2>&1 || true)

    if echo "$help_output" | grep -qi "search"; then
        log_pass "Help mentions search"
    else
        log_warn "Help doesn't mention search"
    fi

    if echo "$help_output" | grep -qi "index"; then
        log_pass "Help mentions index"
    else
        log_warn "Help doesn't mention index"
    fi
}

# =============================================================================
# Security Doc Validation
# =============================================================================

check_security() {
    section "Security Documentation"

    local security="SECURITY.md"

    if [[ ! -f "$security" ]]; then
        log_warn "SECURITY.md not found (may be generated at publish time)"
        return
    fi

    log_info "Checking security documentation..."

    # Check for key security concepts
    local concepts=("encrypt" "argon" "aes" "password" "key")

    for concept in "${concepts[@]}"; do
        if grep -qi "$concept" "$security"; then
            log_pass "Security doc mentions $concept"
        else
            log_warn "Security doc may not cover $concept"
        fi
    done
}

# =============================================================================
# Example Code Validation
# =============================================================================

# =============================================================================
# README ↔ code truth checks (reality check 2026-09-01, WS-A.9)
#
# The README is the vision document; these checks make its concrete claims
# executable: every key binding in a README key table must have a matching arm
# in the TUI key map, every `cass` flag the README shows must exist in
# `cass introspect --json`, and every env var in the README env table must be
# one `cass robot-docs env` knows. `README_PATH` overrides the file under test
# (used to plant a negative); `CASS_BIN` overrides the binary.
# =============================================================================

resolve_cass_binary() {
    local candidate
    for candidate in "${CASS_BIN:-}" "$RCH_TARGET_DIR/release/cass" "$RCH_TARGET_DIR/debug/cass" \
        "target/release/cass" "target/debug/cass"; do
        if [[ -n "$candidate" && -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    if command -v cass &> /dev/null; then
        command -v cass
        return 0
    fi
    return 1
}

# Translate one README key token (`Alt+Shift+W`, `F4`, `Ctrl+Del`, `Esc`, `?`)
# into an extended regex over the TUI key map source, or print nothing when the
# token is not a key (prose in the key column).
key_token_pattern() {
    local token="$1" mods="" base
    local ctrl=0 alt=0 shift=0
    base="$token"
    while [[ "$base" == *+* && ${#base} -gt 1 ]]; do
        case "${base%%+*}" in
            Ctrl|ctrl) ctrl=1 ;;
            Alt|alt|Meta|Opt|Option) alt=1 ;;
            Shift|shift) shift=1 ;;
            *) break ;;
        esac
        base="${base#*+}"
    done
    local code=""
    case "$base" in
        F[0-9]|F1[0-2]) code="KeyCode::F\\(${base#F}\\)" ;;
        Esc|Escape) code="KeyCode::Esc" ;;
        Enter|Return) code="KeyCode::Enter" ;;
        Tab) if [[ $shift == 1 ]]; then code="KeyCode::BackTab"; shift=0; else code="KeyCode::Tab"; fi ;;
        BackTab) code="KeyCode::BackTab" ;;
        Del|Delete) code="KeyCode::Delete" ;;
        Backspace) code="KeyCode::Backspace" ;;
        Up|Down|Left|Right|Home|End) code="KeyCode::${base}" ;;
        PageUp|PgUp) code="KeyCode::PageUp" ;;
        PageDown|PgDn) code="KeyCode::PageDown" ;;
        Space) code="Char\\(' '\\)" ;;
        [1-9]) # README writes digit ranges as `Alt+1`..`Alt+9`; the map has one
               # range arm: `KeyCode::Char(c @ '1'..='9') if <modifier>`.
            code="(Char\\('${base}'\\)|Char\\(c @ '1'\\.\\.='9'\\))" ;;
        ?) # single printable character: either case is accepted, and either
           # of the two forms the TUI uses — a `KeyCode::Char('x')` arm, or a
           # typed-character re-dispatch `text == "x"` (the detail pane).
            local lower upper escaped
            lower=$(printf '%s' "$base" | tr '[:upper:]' '[:lower:]')
            upper=$(printf '%s' "$base" | tr '[:lower:]' '[:upper:]')
            case "$base" in
                [A-Za-z]) code="(Char\\('(${lower}|${upper})'\\)|text == \"(${lower}|${upper})\")" ;;
                "'") code="Char\\('\\\\''\\)" ;;
                *)
                    escaped=$(printf '%s' "$base" | sed 's/[][\\.*^$/?+(){}|]/\\&/g')
                    code="(Char\\('${escaped}'\\)|text == \"${escaped}\")"
                    ;;
            esac
            ;;
        *) return 0 ;;
    esac
    [[ $ctrl == 1 ]] && mods="${mods}(?=.*ctrl)"
    [[ $alt == 1 ]] && mods="${mods}(?=.*alt)"
    [[ $shift == 1 ]] && mods="${mods}(?=.*shift)"
    # PCRE: the arm line must mention the code and every modifier guard.
    echo "^(?=.*${code})${mods}"
}

check_keys() {
    section "README key bindings ↔ TUI key map"
    local readme="${README_PATH:-README.md}" keymap="src/ui/app.rs"
    local missing=0 checked=0 token pattern cell
    while IFS= read -r cell; do
        # Split multi-key cells: `F3` / `Alt+G`
        while IFS= read -r token; do
            token="${token#\`}"; token="${token%\`}"
            [[ -z "$token" ]] && continue
            pattern=$(key_token_pattern "$token")
            [[ -z "$pattern" ]] && continue
            checked=$((checked + 1))
            if ! grep -P -q "$pattern" "$keymap"; then
                log_fail "README key \`$token\` has no matching arm in $keymap"
                missing=$((missing + 1))
            fi
        done < <(printf '%s\n' "$cell" | grep -o -E '`[^`]+`')
    done < <(grep -E '^\| `[^|]+` *\|' "$readme" | sed -E 's/^\| ([^|]*)\|.*/\1/' | grep -E '`(F[0-9]+|Esc|Enter|Tab|Del|Delete|Backspace|Up|Down|Left|Right|Home|End|Page(Up|Down)|Space|Ctrl|Alt|Shift|.)`|\+' )
    if [[ $missing -eq 0 ]]; then
        log_pass "all $checked README key bindings resolve to a TUI key-map arm"
    fi
}

check_flags() {
    section "README cass flags ↔ cass introspect"
    local readme="${README_PATH:-README.md}" binary known
    if ! binary=$(resolve_cass_binary); then
        log_warn "cass binary not found (set CASS_BIN); skipping flag check"
        return
    fi
    # `cass introspect --json` lists top-level commands only (nested
    # subcommands such as `sources sync` are not described there — a contract
    # gap noted in the reality check), so the truth source for a README
    # invocation is clap itself: `cass <command path> --help`. Help output is
    # collected once per command path.
    local global_flags
    global_flags=$("$binary" --help 2>/dev/null | grep -o -E -- '--[a-z][a-z0-9-]+' | sort -u)
    if [[ -z "$global_flags" ]]; then
        log_fail "cass --help printed no flags from $binary"
        return
    fi
    declare -A help_cache=()
    flags_for_path() {
        local path="$1"
        if [[ -z "${help_cache[$path]+x}" ]]; then
            # shellcheck disable=SC2086
            help_cache[$path]=$("$binary" $path --help 2>/dev/null | grep -o -E -- '--[a-z][a-z0-9-]+' | sort -u)
        fi
        printf '%s\n' "${help_cache[$path]}"
    }
    local missing=0 checked=0 line path token flag
    # README lines that deliberately show WRONG spellings (the auto-correction
    # tables) are not claims about the contract. A line may hold several
    # invocations; each `cass …` segment is judged on its own.
    while IFS= read -r line; do
        path=""
        local first=1
        for token in $(printf '%s' "$line"); do
            if [[ $first == 1 ]]; then first=0; continue; fi   # the literal `cass`
            case "$token" in
                -*|\"*|\'*|\`*|\||'#'*) break ;;
                *)
                    if [[ "$token" =~ ^[a-z][a-z0-9-]*$ ]]; then
                        path="${path:+$path }$token"
                    else
                        break
                    fi
                    ;;
            esac
        done
        while IFS= read -r flag; do
            [[ -z "$flag" ]] && continue
            case "$flag" in --help|--version) continue ;; esac
            checked=$((checked + 1))
            if printf '%s\n' "$global_flags" | grep -q -x -- "$flag"; then
                continue
            fi
            if [[ -z "$path" ]]; then
                # `cass --robot` in prose names a flag without its command;
                # not checkable against one --help, so it is a warning.
                log_warn "README mentions \`cass $flag\` without a command; cannot check it against --help"
                continue
            fi
            if flags_for_path "$path" | grep -q -x -- "$flag"; then
                continue
            fi
            log_fail "README shows \`cass $path $flag\` but \`cass $path --help\` does not list $flag"
            missing=$((missing + 1))
        done < <(printf '%s\n' "$line" | grep -o -E -- '--[a-z][a-z0-9-]+' | sort -u)
    done < <(grep -E '(^|[^a-z])cass ' "$readme" \
        | grep -v -i -E 'typo|alias|corrected|auto-correct|Levenshtein|wrong|converted|become|normalized|promoted|route' \
        | sed -E 's/(^|[^a-z])cass /\ncass /g' | grep -E '^cass ' \
        | sed -E 's/`.*$//')   # an inline code span ends at the next backtick
    if [[ $missing -eq 0 ]]; then
        log_pass "all $checked README cass flag usages are accepted by the matching \`cass … --help\`"
    fi
}

check_env() {
    section "README env vars ↔ code"
    local readme="${README_PATH:-README.md}" binary robot_known=""
    # Truth source: the variable must be read somewhere in src/ (dotenvy::var,
    # env::var, or a named constant). `cass robot-docs env` is a curated subset
    # for agents, so a README variable it omits is only a warning.
    if binary=$(resolve_cass_binary); then
        robot_known=$("$binary" robot-docs env 2>/dev/null | grep -o -E '^\s+[A-Z][A-Z0-9_]+' | tr -d ' ' | sort -u)
    fi
    # Connector root overrides (`CASS_<AGENT>_DATA_ROOT`) are read by the
    # franken_agent_detection crate, not by src/; look in its checkout or the
    # cargo registry copy when present.
    local -a env_roots=(src)
    local fad fad_found=0
    for fad in /data/projects/franken_agent_detection/src \
        "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/franken_agent_detection-*/src; do
        if [[ -d "$fad" ]]; then
            env_roots+=("$fad")
            fad_found=1
        fi
    done
    local missing=0 checked=0 unlisted=0 var
    while IFS= read -r var; do
        checked=$((checked + 1))
        if ! grep -r -q -F -- "\"$var\"" "${env_roots[@]}" --include='*.rs'; then
            if [[ $fad_found == 0 && "$var" == CASS_*_DATA_ROOT ]]; then
                # Connector root overrides live in the detection crate; without
                # its source on this host the claim cannot be checked.
                log_warn "README env var \`$var\` is a connector root override; the agent-detection crate source is not available here to confirm it"
                continue
            fi
            log_fail "README env table documents \`$var\` but nothing in src/ (or the agent-detection crate) reads \"$var\""
            missing=$((missing + 1))
        elif [[ -n "$robot_known" ]] && ! printf '%s\n' "$robot_known" | grep -q -x -- "$var"; then
            unlisted=$((unlisted + 1))
        fi
    done < <(grep -E '^\| `CASS_[A-Z0-9_]+`' "$readme" | grep -o -E 'CASS_[A-Z0-9_]+' | sort -u)
    if [[ $missing -eq 0 ]]; then
        log_pass "all $checked README env vars are read by the code"
    fi
    if [[ $unlisted -gt 0 ]]; then
        log_warn "$unlisted README env vars are not in the curated \`cass robot-docs env\` list"
    fi
}

check_examples() {
    section "Example Code Validation"

    # Extract code blocks from README
    local readme="README.md"

    if [[ ! -f "$readme" ]]; then
        log_warn "README.md not found"
        return
    fi

    # Check for shell examples
    if grep -qE '```(bash|sh|shell)' "$readme"; then
        log_pass "README has shell examples"
    else
        log_info "No shell examples in README"
    fi

    # Check for Rust examples
    if grep -qE '```rust' "$readme"; then
        log_pass "README has Rust examples"
    else
        log_info "No Rust examples in README"
    fi

    # Validate cargo commands mentioned work
    local cargo_cmds
    cargo_cmds=$(grep -oE 'cargo (build|test|run|install|bench)[^`]*' "$readme" 2>/dev/null | head -5 || true)

    if [[ -n "$cargo_cmds" ]]; then
        log_info "Found cargo commands in README"
        while IFS= read -r cmd; do
            [[ -z "$cmd" ]] && continue
            log_info "  - $cmd"
        done <<< "$cargo_cmds"
    fi
}

# =============================================================================
# Cargo Doc Validation
# =============================================================================

check_cargo_docs() {
    section "Cargo Documentation"

    log_info "Building documentation..."

    ensure_rch || return

    if run_cargo doc --no-deps --quiet 2>/dev/null; then
        log_pass "cargo doc builds successfully"
    else
        log_fail "cargo doc has errors"
    fi

    # Check for documentation warnings
    local doc_output
    doc_output=$(run_cargo doc --no-deps 2>&1 || true)

    local missing_docs
    missing_docs=$(echo "$doc_output" | grep -c "missing documentation" || true)

    if [[ "$missing_docs" -gt 0 ]]; then
        log_warn "$missing_docs items missing documentation"
    else
        log_pass "No missing documentation warnings"
    fi
}

# =============================================================================
# Main
# =============================================================================

main() {
    echo "╔═══════════════════════════════════════════════════════════════╗"
    echo "║           CASS Documentation Validation                       ║"
    echo "╚═══════════════════════════════════════════════════════════════╝"

    cd "$PROJECT_ROOT"

    case "${1:-all}" in
        --links)
            check_links
            ;;
        --readme)
            check_readme
            ;;
        --help)
            check_help
            ;;
        --security)
            check_security
            ;;
        --examples)
            check_examples
            ;;
        --cargo)
            check_cargo_docs
            ;;
        --keys)
            check_keys
            ;;
        --flags)
            check_flags
            ;;
        --env)
            check_env
            ;;
        --truth)
            check_keys
            check_flags
            check_env
            ;;
        all|*)
            check_readme
            check_links
            check_help
            check_security
            check_examples
            check_cargo_docs
            check_keys
            check_flags
            check_env
            ;;
    esac

    # Summary
    section "Summary"
    echo ""
    echo "  Checks:   $CHECKS"
    echo "  Passed:   $((CHECKS - ERRORS))"
    echo "  Errors:   $ERRORS"
    echo "  Warnings: $WARNINGS"
    echo ""

    if [[ "$ERRORS" -gt 0 ]]; then
        echo -e "${RED}Documentation validation failed with $ERRORS error(s)${NC}"
        exit 1
    elif [[ "$WARNINGS" -gt 0 ]]; then
        echo -e "${YELLOW}Documentation validation passed with $WARNINGS warning(s)${NC}"
        exit 0
    else
        echo -e "${GREEN}Documentation validation passed!${NC}"
        exit 0
    fi
}

main "$@"
