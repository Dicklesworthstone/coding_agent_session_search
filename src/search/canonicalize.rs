//! Text canonicalization for consistent embedding input.
//!
//! This module provides text preprocessing to clean and normalize agent logs
//! before embedding. Canonicalization is **critical for determinism**: the same
//! visual text must always produce the same canonical form, which in turn
//! produces the same content hash.
//!
//! # Processing Pipeline
//!
//! 1. **Unicode NFC normalization** - "café" (decomposed) → "café" (composed)
//! 2. **Markdown stripping** - Remove formatting, keep text content
//! 3. **Code block collapsing** - First 20 + last 10 lines, elide middle
//! 4. **Whitespace normalization** - Collapse runs, trim
//! 5. **Low-signal filtering** - Remove "OK", "Done.", etc.
//! 6. **Truncation** - Limit to MAX_EMBED_CHARS (2000)
//!
//! # Why This Matters
//!
//! Without proper canonicalization:
//! - Same visual text can hash differently (Unicode normalization issues)
//! - Large code blocks waste embedding capacity
//! - Markdown syntax adds noise to semantic similarity
//!
//! # Example
//!
//! ```ignore
//! use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
//!
//! let raw = "**Hello** world!\n\n```rust\nfn main() {}\n```";
//! let canonical = canonicalize_for_embedding(raw);
//! let hash = content_hash(&canonical);
//!
//! assert_eq!(canonical, "Hello world!\n[code: rust]");
//! ```

use ring::digest::{self, SHA256};
use unicode_normalization::UnicodeNormalization;

/// Maximum characters to keep after canonicalization.
pub const MAX_EMBED_CHARS: usize = 2000;

/// Maximum lines to keep from the beginning of a code block.
pub const CODE_HEAD_LINES: usize = 20;

/// Maximum lines to keep from the end of a code block.
pub const CODE_TAIL_LINES: usize = 10;

/// Low-signal content to filter out (exact matches, case-insensitive).
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

static STREAMING_CANONICALIZE_ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    dotenvy::var("CASS_STREAMING_CANONICALIZE")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
});

/// Canonicalize text for embedding.
///
/// Applies the full preprocessing pipeline to produce clean, consistent text
/// suitable for embedding. The output is deterministic: the same visual input
/// always produces the same output.
///
/// # Arguments
///
/// * `text` - Raw text from agent logs
///
/// # Returns
///
/// Canonicalized text, suitable for embedding and hashing.
pub fn canonicalize_for_embedding(text: &str) -> String {
    if *STREAMING_CANONICALIZE_ENABLED {
        canonicalize_for_embedding_streaming(text)
    } else {
        canonicalize_for_embedding_legacy(text)
    }
}

fn canonicalize_for_embedding_legacy(text: &str) -> String {
    // Step 1: Unicode NFC normalization (CRITICAL for hash stability)
    let normalized: String = text.nfc().collect();

    // Step 2: Strip markdown and collapse code blocks
    let stripped = strip_markdown_and_code(&normalized);

    // Step 3: Normalize whitespace
    let whitespace_normalized = normalize_whitespace(&stripped);

    // Step 4: Filter low-signal content
    let filtered = filter_low_signal(&whitespace_normalized);

    // Step 5: Truncate to max length
    truncate_to_chars(&filtered, MAX_EMBED_CHARS)
}

fn canonicalize_for_embedding_streaming(text: &str) -> String {
    // Step 1: Unicode NFC normalization (CRITICAL for hash stability)
    let normalized: String = text.nfc().collect();

    let mut writer = WhitespaceWriter::new(normalized.len());
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_lines: Vec<&str> = Vec::new();

    for line in normalized.lines() {
        if line.starts_with("```") {
            if in_code_block {
                let collapsed = collapse_code_block(&code_block_lang, &code_lines);
                writer.push_text(&collapsed);
                writer.push_text("\n");
                code_lines.clear();
                code_block_lang.clear();
                in_code_block = false;
            } else {
                in_code_block = true;
                code_block_lang = line.trim_start_matches('`').trim().to_string();
            }
        } else if in_code_block {
            code_lines.push(line);
        } else {
            let stripped = strip_markdown_line(line);
            if !stripped.is_empty() {
                writer.push_text(&stripped);
                writer.push_text("\n");
            }
        }
    }

    if in_code_block && !code_lines.is_empty() {
        let collapsed = collapse_code_block(&code_block_lang, &code_lines);
        writer.push_text(&collapsed);
        writer.push_text("\n");
    }

    let mut output = writer.finish();

    // Step 4: Filter low-signal content (exact match after normalization)
    let trimmed_lower = output.trim().to_lowercase();
    for pattern in LOW_SIGNAL_CONTENT {
        if trimmed_lower == *pattern {
            return String::new();
        }
    }

    // Step 5: Truncate to max length
    if output.chars().count() <= MAX_EMBED_CHARS {
        output
    } else {
        output = output.chars().take(MAX_EMBED_CHARS).collect();
        output
    }
}

struct WhitespaceWriter {
    out: String,
    prev_whitespace: bool,
}

impl WhitespaceWriter {
    fn new(capacity: usize) -> Self {
        let cap = capacity.min(MAX_EMBED_CHARS + 100);
        Self {
            out: String::with_capacity(cap),
            prev_whitespace: true,
        }
    }

    fn push_text(&mut self, text: &str) {
        for c in text.chars() {
            if c.is_whitespace() {
                if !self.prev_whitespace {
                    self.out.push(' ');
                    self.prev_whitespace = true;
                }
            } else {
                self.out.push(c);
                self.prev_whitespace = false;
            }
        }
    }

    fn finish(mut self) -> String {
        if self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out
    }
}

/// Compute SHA256 content hash of text.
///
/// The hash is computed on the UTF-8 bytes of the input. For consistent
/// hashing, always canonicalize text first.
///
/// # Returns
///
/// 32-byte SHA256 hash as a fixed-size array.
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
    hex_encode(&hash)
}

/// Encode bytes as lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strip markdown formatting and collapse code blocks.
fn strip_markdown_and_code(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if line.starts_with("```") {
            if in_code_block {
                // End of code block - collapse it
                result.push_str(&collapse_code_block(&code_block_lang, &code_lines));
                result.push('\n');
                code_lines.clear();
                code_block_lang.clear();
                in_code_block = false;
            } else {
                // Start of code block
                in_code_block = true;
                code_block_lang = line.trim_start_matches('`').trim().to_string();
            }
        } else if in_code_block {
            code_lines.push(line);
        } else {
            // Strip markdown from regular text
            let stripped = strip_markdown_line(line);
            if !stripped.is_empty() {
                result.push_str(&stripped);
                result.push('\n');
            }
        }
    }

    // Handle unclosed code block
    if in_code_block && !code_lines.is_empty() {
        result.push_str(&collapse_code_block(&code_block_lang, &code_lines));
        result.push('\n');
    }

    result
}

/// Collapse a code block to first N + last M lines.
fn collapse_code_block(lang: &str, lines: &[&str]) -> String {
    let lang_label = if lang.is_empty() {
        "code".to_string()
    } else {
        format!("code: {lang}")
    };

    if lines.len() <= CODE_HEAD_LINES + CODE_TAIL_LINES {
        // Short enough to keep in full
        format!("[{lang_label}]\n{}", lines.join("\n"))
    } else {
        // Collapse middle
        let head: Vec<_> = lines.iter().take(CODE_HEAD_LINES).copied().collect();
        let tail: Vec<_> = lines
            .iter()
            .skip(lines.len() - CODE_TAIL_LINES)
            .copied()
            .collect();
        let omitted = lines.len() - CODE_HEAD_LINES - CODE_TAIL_LINES;
        format!(
            "[{lang_label}]\n{}\n[... {omitted} lines omitted ...]\n{}",
            head.join("\n"),
            tail.join("\n")
        )
    }
}

/// Strip markdown formatting from a single line.
fn strip_markdown_line(line: &str) -> String {
    let mut result = line.to_string();

    // Remove bold/italic markers
    result = result.replace("**", "");
    result = result.replace("__", "");
    result = result.replace('*', "");
    result = result.replace('_', " "); // Underscore often used in identifiers

    // Remove inline code backticks
    result = result.replace('`', "");

    // Convert links [text](url) to just text
    result = strip_markdown_links(&result);

    // Remove headers (# prefix)
    result = result.trim_start_matches('#').trim_start().to_string();

    // Remove blockquote prefix
    result = result.trim_start_matches('>').trim_start().to_string();

    // Remove list markers (only actual markdown list syntax, not arbitrary numbers)
    result = strip_list_marker(&result);

    result
}

/// Strip markdown list markers from the start of a line.
///
/// Only strips actual list marker patterns:
/// - Unordered: "- ", "+ ", "* " (already handled by * removal above, but - and + need space)
/// - Ordered: "1. ", "2. ", "10. ", etc. (digit(s) followed by dot and space)
///
/// Does NOT strip arbitrary leading digits (e.g., "3.14159" stays intact).
fn strip_list_marker(line: &str) -> String {
    let trimmed = line.trim_start();

    // Check for unordered list markers: "- " or "+ "
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return rest.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("+ ") {
        return rest.to_string();
    }

    // Check for ordered list markers: digits followed by ". "
    // e.g., "1. item", "10. item", "123. item"
    let mut chars = trimmed.chars().peekable();
    let mut digit_count = 0;

    // Count leading digits
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            digit_count += 1;
            chars.next();
        } else {
            break;
        }
    }

    // Must have at least one digit, followed by ". " (dot then space)
    if digit_count > 0 && chars.next() == Some('.') && chars.peek() == Some(&' ') {
        chars.next(); // consume the space
        return chars.collect();
    }

    // Not a list marker, return original
    line.to_string()
}

/// Strip markdown links: [text](url) → text
fn strip_markdown_links(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '[' {
            // Potential link start
            let mut link_text = String::new();
            let mut found_close = false;

            for inner in chars.by_ref() {
                if inner == ']' {
                    found_close = true;
                    break;
                }
                link_text.push(inner);
            }

            if found_close && chars.peek() == Some(&'(') {
                // Potential URL start
                chars.next(); // consume '('
                let mut url_part = String::from("(");
                let mut depth = 1;
                let mut valid_link = false;

                for inner in chars.by_ref() {
                    url_part.push(inner);
                    match inner {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                valid_link = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                if valid_link {
                    // Valid link: [text](url) -> text
                    result.push_str(&link_text);
                } else {
                    // Unbalanced parens or EOF: restore everything
                    // [text](url...
                    result.push('[');
                    result.push_str(&link_text);
                    result.push(']');
                    result.push_str(&url_part);
                }
            } else {
                // Not a proper link (no '(' after ']'), keep original
                result.push('[');
                result.push_str(&link_text);
                if found_close {
                    result.push(']');
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Normalize whitespace: collapse runs, trim.
fn normalize_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_whitespace = true; // Start as true to trim leading

    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_whitespace {
                result.push(' ');
                prev_whitespace = true;
            }
        } else {
            result.push(c);
            prev_whitespace = false;
        }
    }

    // Trim trailing whitespace
    result.trim_end().to_string()
}

/// Filter out low-signal content.
fn filter_low_signal(text: &str) -> String {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    // If entire text is low-signal, return empty
    for pattern in LOW_SIGNAL_CONTENT {
        if lower == *pattern {
            return String::new();
        }
    }

    text.to_string()
}

/// Truncate string to at most N characters, respecting char boundaries.
fn truncate_to_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unicode_nfc_normalization() {
        // "café" in two forms:
        // - NFC (composed): é is one character (U+00E9)
        // - NFD (decomposed): é is e + combining accent (U+0065 U+0301)
        let composed = "caf\u{00E9}"; // café (NFC)
        let decomposed = "cafe\u{0301}"; // café (NFD)

        // They look the same but have different byte representations
        assert_ne!(composed, decomposed);

        // After canonicalization, they should be identical
        let canon_composed = canonicalize_for_embedding(composed);
        let canon_decomposed = canonicalize_for_embedding(decomposed);

        assert_eq!(canon_composed, canon_decomposed);
    }

    #[test]
    fn test_unicode_nfc_hash_stability() {
        let composed = "caf\u{00E9}";
        let decomposed = "cafe\u{0301}";

        // Hashes should be identical after canonicalization
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
        assert!(!canonical.contains('['));
        assert!(!canonical.contains(']'));
    }

    #[test]
    fn test_strip_markdown_headers() {
        let text = "# Header 1\n## Header 2\n### Header 3";
        let canonical = canonicalize_for_embedding(text);

        assert!(!canonical.starts_with('#'));
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
        // Create a code block with more than HEAD + TAIL lines
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!("line {i}"));
        }
        let code = format!("```python\n{}\n```", lines.join("\n"));

        let canonical = canonicalize_for_embedding(&code);

        // Should have head lines
        assert!(canonical.contains("line 0"));
        assert!(canonical.contains("line 19")); // Last of head

        // Should have tail lines
        assert!(canonical.contains("line 40")); // First of tail
        assert!(canonical.contains("line 49")); // Last line

        // Should have omission marker
        assert!(canonical.contains("lines omitted"));

        // Should NOT have middle lines
        assert!(!canonical.contains("line 25"));
    }

    #[test]
    fn test_whitespace_normalization() {
        let text = "hello    world\n\n\nwith   multiple   spaces";
        let canonical = canonicalize_for_embedding(text);

        // Multiple spaces should be collapsed
        assert!(!canonical.contains("  "));

        // But words should be preserved
        assert!(canonical.contains("hello"));
        assert!(canonical.contains("world"));
    }

    #[test]
    fn test_low_signal_filtered() {
        assert_eq!(canonicalize_for_embedding("OK"), "");
        assert_eq!(canonicalize_for_embedding("Done."), "");
        assert_eq!(canonicalize_for_embedding("Got it."), "");
        assert_eq!(canonicalize_for_embedding("Thanks!"), "Thanks!"); // Not exact match
    }

    #[test]
    fn test_truncation() {
        let long_text: String = "a".repeat(5000);
        let canonical = canonicalize_for_embedding(&long_text);

        assert_eq!(canonical.len(), MAX_EMBED_CHARS);
    }

    #[test]
    fn test_empty_input() {
        let canonical = canonicalize_for_embedding("");
        assert_eq!(canonical, "");
    }

    #[test]
    fn test_all_code_input() {
        let text = "```\nsome code\n```";
        let canonical = canonicalize_for_embedding(text);

        assert!(canonical.contains("[code]"));
        assert!(canonical.contains("some code"));
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

        // SHA256 produces 32 bytes = 64 hex chars
        assert_eq!(hex.len(), 64);

        // All characters should be hex
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_nested_markdown_links() {
        let text = "See [link with (parens)](http://example.com/path(1))";
        let canonical = canonicalize_for_embedding(text);

        assert!(canonical.contains("link with (parens)"));
        assert!(!canonical.contains("http"));
    }

    #[test]
    fn test_inline_code() {
        let text = "Use `fn main()` to start.";
        let canonical = canonicalize_for_embedding(text);

        assert!(canonical.contains("fn main()"));
        assert!(!canonical.contains('`'));
    }

    #[test]
    fn test_blockquote() {
        let text = "> This is a quote\n> spanning multiple lines";
        let canonical = canonicalize_for_embedding(text);

        assert!(canonical.contains("This is a quote"));
        assert!(!canonical.starts_with('>'));
    }

    #[test]
    fn test_unicode_combining_characters() {
        // Test various combining character scenarios
        let text_with_combining = "a\u{0301}"; // á as a + combining acute
        let canonical = canonicalize_for_embedding(text_with_combining);

        // Should be NFC normalized
        let expected: String = text_with_combining.nfc().collect();
        assert_eq!(canonical, expected);
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

        // Headers stripped
        assert!(canonical.contains("Welcome"));
        assert!(!canonical.contains('#'));

        // Formatting removed
        assert!(!canonical.contains("**"));
        assert!(canonical.contains("Bold"));

        // Code block present
        assert!(canonical.contains("[code: rust]"));

        // Link text preserved, URL removed
        assert!(canonical.contains("docs"));
        assert!(!canonical.contains("http://docs.rs"));
    }

    #[test]
    fn test_list_markers_stripped() {
        // Ordered list markers should be stripped
        let text = "1. First item\n2. Second item\n10. Tenth item";
        let canonical = canonicalize_for_embedding(text);

        assert!(canonical.contains("First item"));
        assert!(canonical.contains("Second item"));
        assert!(canonical.contains("Tenth item"));
        // The "1. " prefix should be gone
        assert!(!canonical.contains("1. "));
    }

    #[test]
    fn test_numbers_not_list_markers_preserved() {
        // Numbers that aren't list markers should be preserved
        let text = "3.14159 is pi";
        let canonical = canonicalize_for_embedding(text);

        // The number should be intact, not treated as a list marker
        assert!(canonical.contains("3.14159"));
        assert!(canonical.contains("is pi"));
    }

    #[test]
    fn test_strip_markdown_links_unbalanced_swallow() {
        let text = "Check [link](url( unbalanced. Next sentence.";
        let canonical = canonicalize_for_embedding(text);
        
        // Before fix: "Check link"
        // After fix: "Check [link](url( unbalanced. Next sentence."
        // Or if processed via strip_markdown_links directly:
        // "[link](url( unbalanced. Next sentence."
        
        // Note: canonicalize_for_embedding applies whitespace norm and other steps,
        // but it should definitely preserve "Next sentence"
        assert!(canonical.contains("Next sentence"), "Should not swallow content");
        assert!(canonical.contains("unbalanced"), "Should not swallow content");
    }
}
