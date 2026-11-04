//! Versioned, bounded document context for semantic passage inputs.
//!
//! This transforms embedding inputs only; canonical messages and query text
//! remain untouched. The selected policy must also bind the vector revision.

use std::borrow::Cow;

/// Limit work on provider titles independently of their original size.
pub const TITLE_SCAN_CHARS: usize = 512;
/// Keep a title from consuming the passage's entire embedding budget.
pub const TITLE_CONTEXT_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputContext {
    #[default]
    Body,
    TitlePrefix,
}

impl InputContext {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value.trim() {
            "body" => Ok(Self::Body),
            "title-prefix" => Ok(Self::TitlePrefix),
            _ => Err("CASS_SEMANTIC_INPUT_CONTEXT must be body or title-prefix"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::TitlePrefix => "title-prefix",
        }
    }

    /// Apply context after passage selection, before canonicalization/hash.
    /// `max_chars` is the canonicalizer's input budget, not a tokenizer limit.
    pub fn document<'a>(
        self,
        title: Option<&str>,
        passage: &'a str,
        max_chars: usize,
    ) -> Cow<'a, str> {
        if self == Self::Body {
            return Cow::Borrowed(passage);
        }
        let bounded: String = title
            .unwrap_or_default()
            .chars()
            // SQLite's bounded title projection has the same NUL boundary.
            .take_while(|ch| *ch != '\0')
            .take(TITLE_SCAN_CHARS)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        let title: String = bounded
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(TITLE_CONTEXT_CHARS)
            .collect();
        if title.is_empty() {
            return Cow::Borrowed(passage);
        }
        let mut input = format!("title: {title}\ntext: ");
        let overhead = input.chars().count();
        if overhead >= max_chars {
            return Cow::Borrowed(passage);
        }
        input.extend(passage.chars().take(max_chars - overhead));
        Cow::Owned(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_policy_preserves_the_exact_borrowed_input() {
        let body = "**original**\n body 👋";
        let input = InputContext::Body.document(Some("irrelevant title"), body, 2000);
        assert!(matches!(input, Cow::Borrowed(_)));
        assert_eq!(input, body);
    }

    #[test]
    fn title_disambiguates_identical_followups_without_changing_the_body() {
        let body = "Applied the configuration and confirmed it works.";
        let git = InputContext::TitlePrefix.document(Some("SSH commit signing"), body, 2000);
        let database =
            InputContext::TitlePrefix.document(Some("Postgres password rotation"), body, 2000);
        assert_ne!(git, database);
        assert!(git.ends_with(body) && database.ends_with(body));
        assert!(git.starts_with("title: SSH commit signing\ntext: "));
    }

    #[test]
    fn absent_blank_or_out_of_budget_titles_preserve_the_body() {
        for title in [None, Some(""), Some("\n\t  "), Some("\0ignored")] {
            assert_eq!(
                InputContext::TitlePrefix.document(title, "body", 2000),
                "body"
            );
        }
        assert_eq!(
            InputContext::TitlePrefix.document(Some("title"), "body", 5),
            "body"
        );
    }

    #[test]
    fn unicode_and_huge_instruction_titles_have_a_fixed_budget() -> Result<(), &'static str> {
        let title = format!("  résumé\n\t修復  {}", "x".repeat(100_000));
        let input = InputContext::TitlePrefix
            .document(Some(&title), &"界".repeat(3000), 2000)
            .into_owned();
        assert!(input.starts_with("title: résumé 修復 "));
        assert_eq!(input.chars().count(), 2000);
        let (title, body) = input
            .split_once("\ntext: ")
            .ok_or("missing title/body separator")?;
        assert_eq!(
            title
                .strip_prefix("title: ")
                .ok_or("missing title prefix")?
                .chars()
                .count(),
            TITLE_CONTEXT_CHARS
        );
        assert!(body.chars().count() > 1800);
        Ok(())
    }

    #[test]
    fn controls_and_nul_do_not_create_extra_title_fields() {
        assert_eq!(
            InputContext::TitlePrefix.document(
                Some("first\nsecond\u{1b} third\0hidden"),
                "body",
                2000
            ),
            "title: first second third\ntext: body"
        );
    }

    #[test]
    fn settings_are_explicit_and_invalid_values_are_errors() {
        assert_eq!(InputContext::parse(" body "), Ok(InputContext::Body));
        assert_eq!(
            InputContext::parse("title-prefix"),
            Ok(InputContext::TitlePrefix)
        );
        for value in ["", "true", "titles", "title_prefix"] {
            assert!(InputContext::parse(value).is_err());
        }
    }
}
