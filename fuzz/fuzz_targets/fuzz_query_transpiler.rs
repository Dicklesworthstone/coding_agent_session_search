//! Fuzz target for the FTS5 query transpiler (bead
//! `coding_agent_session_search-ugp09`).
//!
//! `fuzz_query_parser.rs` already exercises `QueryExplanation::analyze`
//! — the static introspection path. This target covers the distinct
//! user-facing path: `transpile_to_fts5` in `src/search/query.rs`,
//! which rewrites a raw boolean-query string into an FTS5-compatible
//! expression. The transpiler runs on every `cass search <query>`
//! invocation (commit c91ea038 split the sqlite FTS5 fallback into
//! rank + hydrate phases, both of which call into this path).
//!
//! Scope rationale (copy of the bead's "filed LOW" note): fuzzing
//! the full `SearchClient::search` path would require a stable index
//! on disk + embedder model per invocation, violating the
//! >1000 exec/s rule for a useful fuzz run. `transpile_to_fts5` is
//! the tightest pure-function slice of that path that covers the
//! quote handling, boolean operator ordering, AND/OR/NOT composition,
//! and wildcard/phrase edge cases an adversarial user can drive.
//!
//! Invariants enforced by the harness:
//!   1. Totality: `fuzz_transpile_to_fts5` returns `Some(_)` or
//!      `None`; it must never panic on arbitrary UTF-8 input.
//!   2. Empty-balance: if the transpiled `Some(s)` contains
//!      parentheses, they must be balanced — FTS5 rejects unbalanced
//!      parens at query time, so the transpiler producing them would
//!      surface as a downstream query error instead of a clean
//!      "unsupported, fall back" signal.
//!   3. No null byte injection: `Some(s)` output must not contain
//!      `'\0'` (an FTS5 query parser edge case that produced
//!      hard-to-diagnose errors before bead al19b hardened the
//!      error-kind vocabulary).

#![no_main]

use coding_agent_search::search::query::fuzz_transpile_to_fts5;
use libfuzzer_sys::fuzz_target;

const MAX_QUERY_BYTES: usize = 64 * 1024;

fn bounded_str(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fuzz_target!(|raw: &str| {
    let query = bounded_str(raw, MAX_QUERY_BYTES);
    let Some(transpiled) = fuzz_transpile_to_fts5(query) else {
        // `None` ⇒ the transpiler cleanly rejected an unsupported
        // form (leading wildcards, `OR NOT`, bare leading NOT, etc.).
        // That is the happy "fall back to lexical" signal — no
        // further invariants to check.
        return;
    };

    // Parenthesis balance. The transpiler wraps OR groups with `(...)`
    // (see the `format!("({})", group.join(" OR "))` paths), so a
    // runaway pending_or_group or a bug in the AND/NOT interleave
    // could emit an unbalanced `(` or `)`. FTS5 rejects unbalanced
    // parens at query time with an opaque error — catching that here
    // keeps the boundary clean.
    // `saturating_sub` floors at 0, so an early `)` would silently
    // leave `depth = 0` and pass the original `assert!(depth >= 0)`
    // (which is `0 >= 0` ⇒ true). Use `checked_sub` so a stray
    // closing paren before any opener trips the assertion as
    // intended; mirror with `checked_add` so a runaway `(` storm
    // panics on overflow rather than silently saturating.
    let mut depth: u32 = 0;
    for ch in transpiled.chars() {
        match ch {
            '(' => {
                depth = depth.checked_add(1).expect(
                    "transpiled paren depth overflowed u32 — pathological input",
                );
            }
            ')' => {
                depth = depth.checked_sub(1).unwrap_or_else(|| {
                    panic!(
                        "unbalanced closing paren in transpiled query: {transpiled:?} \
                         (from raw: {query:?})"
                    )
                });
            }
            _ => {}
        }
    }
    assert_eq!(
        depth, 0,
        "unbalanced parens — {depth} unclosed `(` at end of transpiled query: \
         {transpiled:?} (from raw: {query:?})"
    );

    // Null byte injection guard — FTS5 parses queries as C strings
    // in some back-ends, so an embedded NUL would truncate or error.
    assert!(
        !transpiled.contains('\0'),
        "null byte in transpiled query: {transpiled:?} (from raw: {query:?})"
    );
});
