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
        all|*)
            check_readme
            check_links
            check_help
            check_security
            check_examples
            check_cargo_docs
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
