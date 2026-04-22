//! Text canonicalization for consistent embedding input.
//!
//! Delegates to [`frankensearch::DefaultCanonicalizer`] for the full preprocessing
//! pipeline (NFC normalization, markdown stripping, code block collapsing,
//! whitespace normalization, low-signal filtering, and truncation).
//!
//! This module adds content hashing on top of the shared canonicalization logic.
//!
//! # Example
//!
//! ```ignore
//! use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
//!
//! let raw = "**Hello** world!\n\n```rust\nfn main() {}\n```";
//! let canonical = canonicalize_for_embedding(raw);
//! let hash = content_hash(&canonical);
//! ```

use frankensearch::{Canonicalizer, DefaultCanonicalizer};
use ring::digest::{self, SHA256};

/// Maximum characters to keep after canonicalization.
pub const MAX_EMBED_CHARS: usize = 2000;

/// Maximum lines to keep from the beginning of a code block.
pub const CODE_HEAD_LINES: usize = 20;

/// Maximum lines to keep from the end of a code block.
pub const CODE_TAIL_LINES: usize = 10;

thread_local! {
    /// Per-thread cached canonicalizer. DefaultCanonicalizer is a stateless
    /// POD (three `usize` fields), so the cost of `Default::default()` per
    /// call was pure overhead; caching it also gives a clean injection point
    /// for future input-length short-circuiting.
    static CANONICALIZER: DefaultCanonicalizer = DefaultCanonicalizer::default();
}

/// Low-signal content tokens. Must stay in sync with frankensearch's
/// `LOW_SIGNAL_CONTENT` constant; the slow path falls through to the shared
/// canonicalizer so any drift is caught by `canonicalize_for_embedding_fast_path_matches_slow_path`.
const LOW_SIGNAL_CONTENT: &[&str] = &[
    "ok",
    "done",
    "done.",
    "got it",
    "got it.",
    "understood",
    "understood.",
    "sure",
    "sure.",
    "yes",
    "no",
    "thanks",
    "thanks.",
    "thank you",
    "thank you.",
];

/// Return `Some(canonical)` when `text` can be processed by the cheap
/// whitespace-only fast path, `None` otherwise. The fast path matches the
/// output of the full `DefaultCanonicalizer` pipeline exactly when the input
/// is pure ASCII and contains no markdown discriminators.
///
/// For the dominant tool-output message shape (short plain-ASCII strings
/// without backticks, asterisks, underscores, headers, or link brackets),
/// this skips NFC normalization, markdown line-by-line stripping, and
/// code-block collapse — the expensive parts of the slow path — and just
/// does whitespace collapse + low-signal filter + truncation.
fn canonicalize_fast_path(text: &str) -> Option<String> {
    // Pure-ASCII check implies NFC is a no-op; any non-ASCII byte must
    // flow through the full pipeline because NFC may re-encode composed
    // characters.
    if !text.is_ascii() {
        return None;
    }
    // Any markdown discriminator byte forces the slow path. `]` is excluded
    // because on its own it's harmless; `[` is the real link start token, so
    // looking for `[` alone suffices.
    if text
        .bytes()
        .any(|b| matches!(b, b'`' | b'*' | b'_' | b'#' | b'['))
    {
        return None;
    }

    // Whitespace-collapsed string: split_whitespace + join(' ') produces the
    // same output as the slow path's char-by-char collapse + trim.
    // Pre-size the buffer from the input length — collapsed output is always
    // <= input length for ASCII.
    let mut collapsed = String::with_capacity(text.len());
    let mut first = true;
    for token in text.split_whitespace() {
        if !first {
            collapsed.push(' ');
        }
        collapsed.push_str(token);
        first = false;
    }

    // Low-signal filter: case-insensitive ASCII match against the shared
    // pattern list. `str::eq_ignore_ascii_case` walks both operands byte-by-
    // byte and does the case-fold inline, so we avoid the `to_ascii_lowercase`
    // allocation that the previous version paid on every ack-length input.
    if !collapsed.is_empty() {
        for pattern in LOW_SIGNAL_CONTENT {
            if collapsed.eq_ignore_ascii_case(pattern) {
                return Some(String::new());
            }
        }
    }

    // Truncate to MAX_EMBED_CHARS. Pure-ASCII inputs let us slice by byte
    // index == char index.
    if collapsed.len() > MAX_EMBED_CHARS {
        collapsed.truncate(MAX_EMBED_CHARS);
    }

    Some(collapsed)
}

/// Canonicalize text for embedding.
///
/// Applies the full preprocessing pipeline to produce clean, consistent text
/// suitable for embedding. The output is deterministic: the same visual input
/// always produces the same output.
///
/// Hot-path: when the input is pure ASCII and contains no markdown
/// discriminator bytes, a cheap whitespace-only fast path is used and the
/// full `DefaultCanonicalizer` pipeline is skipped. The fast path is a
/// superset-preserving refinement — for any input where it fires, its output
/// is byte-identical to the slow path.
pub fn canonicalize_for_embedding(text: &str) -> String {
    if let Some(fast) = canonicalize_fast_path(text) {
        return fast;
    }
    CANONICALIZER.with(|c| c.canonicalize(text))
}

/// Compute SHA256 content hash of text.
///
/// The hash is computed on the UTF-8 bytes of the input. For consistent
/// hashing, always canonicalize text first.
pub fn content_hash(text: &str) -> [u8; 32] {
    let digest = digest::digest(&SHA256, text.as_bytes());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

/// Compute SHA256 content hash as hex string.
///
/// Convenience wrapper around [`content_hash`] that returns a hex-encoded string.
pub fn content_hash_hex(text: &str) -> String {
    let hash = content_hash(text);
    hex::encode(hash)
}

fn role_is(role: Option<&str>, expected: &str) -> bool {
    role.is_some_and(|role| role.trim().eq_ignore_ascii_case(expected))
}

fn is_short_acknowledgement(lower: &str) -> bool {
    matches!(
        lower,
        "ok" | "ok."
            | "okay"
            | "okay."
            | "done"
            | "done."
            | "done!"
            | "got it"
            | "got it."
            | "got it!"
            | "ack"
            | "ack."
            | "acknowledged"
            | "acknowledged."
            | "confirmed"
            | "confirmed."
            | "completed"
            | "completed."
            | "complete"
            | "complete."
    )
}

/// Return true when text is a low-value acknowledgement/tool confirmation.
///
/// These messages add little search value and tend to dominate result sets with
/// repeated "done/acknowledged/wrote file" noise.
pub fn is_tool_acknowledgement(role: Option<&str>, text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.len() > 200 {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    if is_short_acknowledgement(&lower) {
        return true;
    }

    let toolish = role_is(role, "tool");
    let short_tool_ack = lower == "no matches found"
        || lower == "no changes made"
        || lower == "no changes"
        || lower == "already up to date"
        || lower == "up to date"
        || lower == "file written";
    if short_tool_ack && (toolish || lower.contains("file") || lower.contains("match")) {
        return true;
    }

    let prefixed_tool_ack = lower.starts_with("successfully wrote to ")
        || lower.starts_with("successfully updated ")
        || lower.starts_with("successfully created ")
        || lower.starts_with("successfully deleted ")
        || lower.starts_with("successfully saved ")
        || lower.starts_with("successfully applied ")
        || lower.starts_with("applied patch")
        || lower.starts_with("patch applied");
    prefixed_tool_ack && (toolish || lower.contains('/') || lower.contains("file"))
}

/// Return true when content looks like an injected prompt/instructions block.
///
/// We keep these messages in storage, but suppress them from normal search
/// results unless the query is clearly asking for prompt/instruction content.
pub fn is_system_prompt_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("# agents.md instructions for ")
        || lower.starts_with("agents.md instructions for ")
        || lower.starts_with("system prompt:")
        || lower.starts_with("developer prompt:")
        || lower.starts_with("developer message:")
        || lower.starts_with("system message:")
        || lower.contains("follow the agents.md instructions")
        || ((lower.starts_with("you are a ") || lower.starts_with("you are an "))
            && (lower.contains("assistant") || lower.contains("coding agent"))
            && (lower.contains("instructions")
                || lower.contains("follow")
                || lower.contains("must")
                || lower.contains("rules")))
}

/// Return true when a query explicitly asks for prompt/instructions content.
pub fn query_requests_system_prompt(query: &str) -> bool {
    let lower = query.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }

    lower.contains("system prompt")
        || lower.contains("developer prompt")
        || lower.contains("system message")
        || lower.contains("developer message")
        || lower.contains("system instructions")
        || lower.contains("developer instructions")
        || lower.contains("agents.md")
        || lower.contains("agents md")
        || lower.contains("claude.md")
        || lower.contains("claude md")
        || lower.contains("prompt text")
        || ((lower.starts_with("you are ") || lower.contains(" you are "))
            && (lower.contains("assistant") || lower.contains("coding agent")))
        || lower.contains("\"you are")
}

/// Noise we can safely skip during indexing.
pub fn is_hard_message_noise(role: Option<&str>, text: &str) -> bool {
    text.trim().is_empty() || is_tool_acknowledgement(role, text)
}

/// Noise we should suppress from search results.
pub fn is_search_noise_text(text: &str, query: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || is_tool_acknowledgement(None, trimmed)
        || (is_system_prompt_text(trimmed) && !query_requests_system_prompt(query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_fast_path_matches_slow_path_for_pure_ascii_inputs() {
        // Every input in this table must either (a) hit the fast path and
        // match the slow path byte-for-byte, or (b) correctly fall through
        // to the slow path because it contains a markdown discriminator or
        // non-ASCII bytes. If the fast path ever diverges, this test catches
        // it before it reaches production.
        let cases = &[
            // Pure-ASCII, no markdown — fast path eligible
            "hello world",
            "  hello   world  ",
            "hello\n\n\nworld\n",
            "line one\nline two\nline three",
            "Thanks!",
            "plain text with punctuation: comma, period. question?",
            "simple-hyphen and plus+signs",
            "parens (like this) are fine",
            // Low-signal acks — fast path must return ""
            "OK",
            "ok",
            "  Done.  ",
            "got it",
            "Thanks",
            "thank you.",
            // Markdown discriminators — fall through to slow path
            "**bold** text",
            "has `inline code`",
            "# A Header",
            "list [link](url)",
            "_italic_ too",
            // Non-ASCII — fall through (NFC must run)
            "café au lait",
            "caf\u{0065}\u{0301}",
            "emoji 👋 mix",
            // Empty / whitespace-only
            "",
            "   ",
            "\n\n\n",
        ];

        for input in cases {
            let slow = CANONICALIZER.with(|c| c.canonicalize(input));
            let combined = canonicalize_for_embedding(input);
            assert_eq!(
                combined, slow,
                "canonicalize_for_embedding({input:?}) diverged from slow path"
            );
        }
    }

    #[test]
    fn canonicalize_fast_path_truncates_to_max_embed_chars() {
        let long_ascii: String = "a ".repeat(MAX_EMBED_CHARS);
        let out = canonicalize_for_embedding(&long_ascii);
        assert!(out.chars().count() <= MAX_EMBED_CHARS);
    }

    #[test]
    fn test_unicode_nfc_normalization() {
        let composed = "caf\u{00E9}";
        let decomposed = "cafe\u{0301}";
        assert_ne!(composed, decomposed);
        let canon_composed = canonicalize_for_embedding(composed);
        let canon_decomposed = canonicalize_for_embedding(decomposed);
        assert_eq!(canon_composed, canon_decomposed);
    }

    #[test]
    fn test_unicode_nfc_hash_stability() {
        let composed = "caf\u{00E9}";
        let decomposed = "cafe\u{0301}";
        let hash1 = content_hash(&canonicalize_for_embedding(composed));
        let hash2 = content_hash(&canonicalize_for_embedding(decomposed));
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_canonicalize_deterministic() {
        let text = "**Hello** _world_!\n\nThis is a [link](http://example.com).";
        let result1 = canonicalize_for_embedding(text);
        let result2 = canonicalize_for_embedding(text);
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_strip_markdown_bold_italic() {
        let text = "**bold** and *italic* and __also bold__";
        let canonical = canonicalize_for_embedding(text);
        assert!(!canonical.contains("**"));
        assert!(!canonical.contains("__"));
        assert!(canonical.contains("bold"));
        assert!(canonical.contains("italic"));
    }

    #[test]
    fn test_strip_markdown_links() {
        let text = "Check out [this link](http://example.com) for more info.";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("this link"));
        assert!(!canonical.contains("http://example.com"));
    }

    #[test]
    fn test_strip_markdown_headers() {
        let text = "# Header 1\n## Header 2\n### Header 3";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("Header 1"));
        assert!(canonical.contains("Header 2"));
        assert!(canonical.contains("Header 3"));
    }

    #[test]
    fn test_code_block_short() {
        let text = "```rust\nfn main() {\n    println!(\"Hello\");\n}\n```";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("[code: rust]"));
        assert!(canonical.contains("fn main()"));
    }

    #[test]
    fn test_code_block_collapse_long() {
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!("line {i}"));
        }
        let code = format!("```python\n{}\n```", lines.join("\n"));
        let canonical = canonicalize_for_embedding(&code);

        assert!(canonical.contains("line 0"));
        assert!(canonical.contains("line 19"));
        assert!(canonical.contains("line 40"));
        assert!(canonical.contains("line 49"));
        assert!(canonical.contains("lines omitted"));
        assert!(!canonical.contains("line 25"));
    }

    #[test]
    fn test_whitespace_normalization() {
        let text = "hello    world\n\n\nwith   multiple   spaces";
        let canonical = canonicalize_for_embedding(text);
        assert!(!canonical.contains("  "));
        assert!(canonical.contains("hello"));
        assert!(canonical.contains("world"));
    }

    #[test]
    fn test_low_signal_filtered() {
        assert_eq!(canonicalize_for_embedding("OK"), "");
        assert_eq!(canonicalize_for_embedding("Done."), "");
        assert_eq!(canonicalize_for_embedding("Got it."), "");
        assert_eq!(canonicalize_for_embedding("Thanks!"), "Thanks!");
    }

    #[test]
    fn test_truncation() {
        let long_text: String = "a".repeat(5000);
        let canonical = canonicalize_for_embedding(&long_text);
        assert_eq!(canonical.chars().count(), 2000);
    }

    #[test]
    fn test_empty_input() {
        assert_eq!(canonicalize_for_embedding(""), "");
    }

    #[test]
    fn test_content_hash_deterministic() {
        let text = "Hello, world!";
        let hash1 = content_hash(text);
        let hash2 = content_hash(text);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_different_for_different_input() {
        let hash1 = content_hash("Hello");
        let hash2 = content_hash("World");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_hex() {
        let hex = content_hash_hex("test");
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_is_tool_acknowledgement_detects_short_replies() {
        assert!(is_tool_acknowledgement(None, "OK"));
        assert!(is_tool_acknowledgement(None, "Acknowledged."));
        assert!(is_tool_acknowledgement(None, "Done!"));
        assert!(!is_tool_acknowledgement(None, "Thanks!"));
    }

    #[test]
    fn test_is_tool_acknowledgement_detects_tool_write_confirmations() {
        assert!(is_tool_acknowledgement(
            Some("tool"),
            "Successfully wrote to /tmp/output.rs"
        ));
        assert!(is_tool_acknowledgement(Some("tool"), "No matches found"));
        assert!(!is_tool_acknowledgement(
            Some("tool"),
            "Compilation failed with an auth refresh error"
        ));
    }

    #[test]
    fn test_is_system_prompt_text_detects_instruction_blocks() {
        assert!(is_system_prompt_text(
            "# AGENTS.md instructions for /repo\n\nFollow these rules carefully."
        ));
        assert!(is_system_prompt_text(
            "You are a coding assistant. You must follow the instructions exactly."
        ));
        assert!(!is_system_prompt_text(
            "You are looking at the auth module."
        ));
    }

    #[test]
    fn test_query_requests_system_prompt_matches_prompt_terms() {
        assert!(query_requests_system_prompt("AGENTS.md instructions"));
        assert!(query_requests_system_prompt("show me the system prompt"));
        assert!(query_requests_system_prompt("you are a coding assistant"));
        assert!(!query_requests_system_prompt("build instructions"));
        assert!(!query_requests_system_prompt("authentication failure"));
    }

    #[test]
    fn test_list_markers_stripped() {
        let text = "1. First item\n2. Second item\n10. Tenth item";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("First item"));
        assert!(canonical.contains("Second item"));
        assert!(canonical.contains("Tenth item"));
    }

    #[test]
    fn test_numbers_not_list_markers_preserved() {
        let text = "3.14159 is pi";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("3.14159"));
    }

    #[test]
    fn test_blockquote() {
        let text = "> This is a quote\n> spanning multiple lines";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("This is a quote"));
    }

    #[test]
    fn test_inline_code() {
        let text = "Use `fn main()` to start.";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("fn main()"));
        assert!(!canonical.contains('`'));
    }

    #[test]
    fn test_emoji_preserved() {
        let text = "Hello 👋 World 🌍";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains('👋'));
        assert!(canonical.contains('🌍'));
    }

    #[test]
    fn test_mixed_content() {
        let text = r#"# Welcome

**Bold** and *italic* text.

```rust
fn hello() {
    println!("Hello!");
}
```

See [docs](http://docs.rs) for more.
"#;
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("Welcome"));
        assert!(!canonical.contains("**"));
        assert!(canonical.contains("Bold"));
        assert!(canonical.contains("[code: rust]"));
        assert!(canonical.contains("docs"));
        assert!(!canonical.contains("http://docs.rs"));
    }

    #[test]
    fn test_unbalanced_link_preserves_content() {
        let text = "Check [link](url( unbalanced. Next sentence.";
        let canonical = canonicalize_for_embedding(text);
        assert!(canonical.contains("Next sentence"));
        assert!(canonical.contains("unbalanced"));
    }
}
