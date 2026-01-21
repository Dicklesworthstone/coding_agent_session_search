//! Password strength validation and visual feedback.
//!
//! Provides real-time password strength validation with consistent behavior
//! between CLI (Rust) and browser (JavaScript) implementations.
//!
//! # Strength Levels
//!
//! | Level | Entropy | Requirements |
//! |-------|---------|--------------|
//! | Weak | <20 bits | Missing multiple requirements |
//! | Fair | 20-40 bits | Missing some requirements |
//! | Good | 40-60 bits | Most requirements met |
//! | Strong | ≥60 bits | All requirements met, 12+ chars |

use console::{style, Term};
use std::io::Write;

/// Password strength levels with associated colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordStrength {
    Weak,
    Fair,
    Good,
    Strong,
}

impl PasswordStrength {
    /// Get the ANSI color name for this strength level.
    pub fn color(&self) -> &'static str {
        match self {
            Self::Weak => "red",
            Self::Fair => "yellow",
            Self::Good => "blue",
            Self::Strong => "green",
        }
    }

    /// Get a human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Weak => "Weak",
            Self::Fair => "Fair",
            Self::Good => "Good",
            Self::Strong => "Strong",
        }
    }

    /// Get the progress bar representation (4 segments).
    pub fn bar(&self) -> &'static str {
        match self {
            Self::Weak => "[█░░░]",
            Self::Fair => "[██░░]",
            Self::Good => "[███░]",
            Self::Strong => "[████]",
        }
    }

    /// Get the percentage (0-100) for progress bar width.
    pub fn percent(&self) -> u8 {
        match self {
            Self::Weak => 25,
            Self::Fair => 50,
            Self::Good => 75,
            Self::Strong => 100,
        }
    }
}

impl std::fmt::Display for PasswordStrength {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Result of password validation.
#[derive(Debug, Clone)]
pub struct PasswordValidation {
    /// Overall strength level.
    pub strength: PasswordStrength,
    /// Computed entropy score (0-7 based on criteria).
    pub score: u8,
    /// Entropy in bits.
    pub entropy_bits: f64,
    /// List of improvement suggestions.
    pub suggestions: Vec<&'static str>,
    /// Individual requirement checks.
    pub checks: PasswordChecks,
}

/// Individual password requirement checks.
#[derive(Debug, Clone, Copy)]
pub struct PasswordChecks {
    pub has_lowercase: bool,
    pub has_uppercase: bool,
    pub has_digit: bool,
    pub has_special: bool,
    pub length: usize,
    pub meets_min_length: bool,
}

/// Validate a password and return strength assessment with suggestions.
///
/// # Algorithm
///
/// 1. Check for presence of lowercase, uppercase, digits, and special characters
/// 2. Compute length score: 0 (0-7), 1 (8-11), 2 (12-15), 3 (16+)
/// 3. Sum all criteria to get score (0-7)
/// 4. Map score to strength level
///
/// # Example
///
/// ```
/// use cass::pages::password::validate_password;
///
/// let result = validate_password("MySecureP@ssw0rd!");
/// assert_eq!(result.strength, PasswordStrength::Strong);
/// assert!(result.suggestions.is_empty());
/// ```
pub fn validate_password(password: &str) -> PasswordValidation {
    let length = password.len();
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    let has_special = password.chars().any(|c| !c.is_alphanumeric());

    // Length scoring (0-3 points)
    let length_score: u8 = match length {
        0..=7 => 0,
        8..=11 => 1,
        12..=15 => 2,
        _ => 3,
    };

    // Total score (0-7)
    let score = length_score
        + has_upper as u8
        + has_lower as u8
        + has_digit as u8
        + has_special as u8;

    // Collect improvement suggestions
    let mut suggestions = Vec::new();
    if length < 12 {
        suggestions.push("Use at least 12 characters");
    }
    if !has_upper {
        suggestions.push("Add uppercase letters");
    }
    if !has_lower {
        suggestions.push("Add lowercase letters");
    }
    if !has_digit {
        suggestions.push("Add numbers");
    }
    if !has_special {
        suggestions.push("Add special characters (!@#$%^&*)");
    }

    // Map score to strength
    let strength = match score {
        0..=2 => PasswordStrength::Weak,
        3..=4 => PasswordStrength::Fair,
        5..=6 => PasswordStrength::Good,
        _ => PasswordStrength::Strong,
    };

    // Calculate entropy bits for compatibility with confirmation.rs
    let entropy_bits = estimate_entropy(password);

    PasswordValidation {
        strength,
        score,
        entropy_bits,
        suggestions,
        checks: PasswordChecks {
            has_lowercase: has_lower,
            has_uppercase: has_upper,
            has_digit,
            has_special,
            length,
            meets_min_length: length >= 12,
        },
    }
}

/// Calculate password entropy using character class analysis.
///
/// This mirrors the algorithm in `confirmation.rs::estimate_password_entropy`
/// for consistency.
fn estimate_entropy(password: &str) -> f64 {
    if password.is_empty() {
        return 0.0;
    }

    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    let has_special = password.chars().any(|c| !c.is_alphanumeric());

    let mut pool_size = 0u32;
    if has_lower {
        pool_size += 26;
    }
    if has_upper {
        pool_size += 26;
    }
    if has_digit {
        pool_size += 10;
    }
    if has_special {
        pool_size += 32;
    }

    if pool_size == 0 {
        pool_size = 26; // Assume lowercase if nothing else
    }

    let bits_per_char = (pool_size as f64).log2();
    let length = password.len() as f64;

    bits_per_char * length
}

/// Display password strength in the terminal with colored progress bar.
///
/// Clears the current line and writes:
/// ```text
/// Strength: [████] Strong
///   • Add special characters (!@#$%^&*)
/// ```
pub fn display_strength(term: &mut Term, validation: &PasswordValidation) -> std::io::Result<()> {
    let strength = &validation.strength;

    // Choose color based on strength
    let colored_bar = match strength {
        PasswordStrength::Weak => style(strength.bar()).red(),
        PasswordStrength::Fair => style(strength.bar()).yellow(),
        PasswordStrength::Good => style(strength.bar()).blue(),
        PasswordStrength::Strong => style(strength.bar()).green(),
    };

    let colored_label = match strength {
        PasswordStrength::Weak => style(strength.label()).red().bold(),
        PasswordStrength::Fair => style(strength.label()).yellow().bold(),
        PasswordStrength::Good => style(strength.label()).blue().bold(),
        PasswordStrength::Strong => style(strength.label()).green().bold(),
    };

    // Clear line and write strength indicator
    term.clear_line()?;
    write!(term, "Strength: {} {}", colored_bar, colored_label)?;

    // Show suggestions if any
    if !validation.suggestions.is_empty() {
        writeln!(term)?;
        for suggestion in &validation.suggestions {
            writeln!(term, "  {} {}", style("•").dim(), style(suggestion).dim())?;
        }
    }

    term.flush()?;
    Ok(())
}

/// Format password strength as a simple inline indicator.
///
/// Returns a string like "[████] Strong" with ANSI colors.
pub fn format_strength_inline(validation: &PasswordValidation) -> String {
    let strength = &validation.strength;

    let bar = match strength {
        PasswordStrength::Weak => style(strength.bar()).red(),
        PasswordStrength::Fair => style(strength.bar()).yellow(),
        PasswordStrength::Good => style(strength.bar()).blue(),
        PasswordStrength::Strong => style(strength.bar()).green(),
    };

    let label = match strength {
        PasswordStrength::Weak => style(strength.label()).red().bold(),
        PasswordStrength::Fair => style(strength.label()).yellow().bold(),
        PasswordStrength::Good => style(strength.label()).blue().bold(),
        PasswordStrength::Strong => style(strength.label()).green().bold(),
    };

    format!("{} {}", bar, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_password() {
        let result = validate_password("");
        assert_eq!(result.strength, PasswordStrength::Weak);
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_weak_password() {
        let result = validate_password("password");
        assert_eq!(result.strength, PasswordStrength::Weak);
        assert!(result.suggestions.contains(&"Add uppercase letters"));
        assert!(result.suggestions.contains(&"Add numbers"));
        assert!(result.suggestions.contains(&"Add special characters (!@#$%^&*)"));
    }

    #[test]
    fn test_fair_password() {
        let result = validate_password("Password1");
        assert_eq!(result.strength, PasswordStrength::Fair);
    }

    #[test]
    fn test_good_password() {
        let result = validate_password("Password1!");
        assert_eq!(result.strength, PasswordStrength::Good);
    }

    #[test]
    fn test_strong_password() {
        let result = validate_password("MySecureP@ssw0rd!");
        assert_eq!(result.strength, PasswordStrength::Strong);
        assert!(result.suggestions.is_empty());
    }

    #[test]
    fn test_long_lowercase_only() {
        // Long but only lowercase - should be fair due to length
        let result = validate_password("averylongpasswordwithnothingelse");
        assert!(matches!(
            result.strength,
            PasswordStrength::Fair | PasswordStrength::Good
        ));
    }

    #[test]
    fn test_strength_bar_rendering() {
        assert_eq!(PasswordStrength::Weak.bar(), "[█░░░]");
        assert_eq!(PasswordStrength::Fair.bar(), "[██░░]");
        assert_eq!(PasswordStrength::Good.bar(), "[███░]");
        assert_eq!(PasswordStrength::Strong.bar(), "[████]");
    }

    #[test]
    fn test_strength_percent() {
        assert_eq!(PasswordStrength::Weak.percent(), 25);
        assert_eq!(PasswordStrength::Fair.percent(), 50);
        assert_eq!(PasswordStrength::Good.percent(), 75);
        assert_eq!(PasswordStrength::Strong.percent(), 100);
    }

    #[test]
    fn test_checks_populated() {
        let result = validate_password("Test123!");
        assert!(result.checks.has_lowercase);
        assert!(result.checks.has_uppercase);
        assert!(result.checks.has_digit);
        assert!(result.checks.has_special);
        assert_eq!(result.checks.length, 8);
        assert!(!result.checks.meets_min_length);
    }

    #[test]
    fn test_entropy_calculation() {
        // All character classes: pool_size = 26+26+10+32 = 94
        // log2(94) ≈ 6.55 bits per char
        // 16 chars → ~105 bits
        let result = validate_password("MySecureP@ssw0rd");
        assert!(result.entropy_bits > 80.0);
    }

    #[test]
    fn test_unicode_password() {
        // Unicode characters should be handled (treated as special)
        let result = validate_password("Pässwörd123!");
        assert!(result.checks.has_special); // ä and ö are special
        assert!(result.checks.has_uppercase);
        assert!(result.checks.has_digit);
    }
}
