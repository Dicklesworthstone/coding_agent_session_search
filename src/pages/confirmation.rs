//! Safety confirmation flow for pages export.
//!
//! Implements a multi-step confirmation flow that ensures users explicitly
//! acknowledge the implications of publishing encrypted content to a public site.
//!
//! # Confirmation Steps
//!
//! 1. **SecretScanAcknowledgment** - If secrets detected, user must type "I understand the risks"
//! 2. **ContentReview** - User confirms they have reviewed the content summary
//! 3. **PublicPublishingWarning** - User types the target domain to confirm
//! 4. **PasswordStrengthWarning** - If password entropy < 60 bits, user chooses action
//! 5. **RecoveryKeyBackup** - User types the last word of the recovery key
//! 6. **FinalConfirmation** - User presses Enter twice

use crate::pages::summary::{PrePublishSummary, ScanReportSummary};
use std::collections::HashSet;

/// Minimum password entropy in bits for full strength.
pub const MIN_STRONG_PASSWORD_BITS: f64 = 60.0;

const SECRET_ACK_PHRASE: &str = "I understand the risks";
const SECRET_ACK_PHRASE_NORMALIZED: &str = "i understand the risks";

/// Confirmation step identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfirmationStep {
    /// Acknowledge detected secrets (only shown if secrets found).
    SecretScanAcknowledgment,
    /// Confirm review of content summary.
    ContentReview,
    /// Acknowledge public publishing implications.
    PublicPublishingWarning,
    /// Acknowledge weak password (only shown if entropy < threshold).
    PasswordStrengthWarning,
    /// Confirm recovery key backup.
    RecoveryKeyBackup,
    /// Final double-enter confirmation.
    FinalConfirmation,
}

impl ConfirmationStep {
    /// Get a human-readable label for the step.
    pub fn label(self) -> &'static str {
        match self {
            ConfirmationStep::SecretScanAcknowledgment => "Secret Scan Acknowledgment",
            ConfirmationStep::ContentReview => "Content Review",
            ConfirmationStep::PublicPublishingWarning => "Public Publishing Warning",
            ConfirmationStep::PasswordStrengthWarning => "Password Strength Warning",
            ConfirmationStep::RecoveryKeyBackup => "Recovery Key Backup",
            ConfirmationStep::FinalConfirmation => "Final Confirmation",
        }
    }
}

/// Result of a confirmation step validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepValidation {
    /// Step passed validation.
    Passed,
    /// Step failed validation with error message.
    Failed(String),
}

/// Result of processing user input for a confirmation step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationResult {
    /// Continue with current step (awaiting more input).
    Continue,
    /// Step completed, move to next.
    StepCompleted,
    /// All steps completed, ready to proceed.
    Confirmed,
    /// User aborted the flow.
    Aborted,
    /// Skip this step (not applicable).
    Skip,
}

/// Password strength action selected by user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordStrengthAction {
    /// Set a stronger password.
    SetStronger,
    /// Proceed with current password (acknowledged weak).
    ProceedAnyway,
    /// Abort the export.
    Abort,
}

/// Configuration for the confirmation flow.
#[derive(Debug, Clone)]
pub struct ConfirmationConfig {
    /// Whether secrets were detected.
    pub has_secrets: bool,
    /// Whether there are critical secrets.
    pub has_critical_secrets: bool,
    /// Number of secret findings.
    pub secret_count: usize,
    /// Target domain for publishing (e.g., "username.github.io").
    pub target_domain: Option<String>,
    /// Whether publishing to a remote target (GitHub/Cloudflare Pages).
    pub is_remote_publish: bool,
    /// Password entropy in bits.
    pub password_entropy_bits: f64,
    /// Whether recovery key was generated.
    pub has_recovery_key: bool,
    /// The recovery key phrase (for validation).
    pub recovery_key_phrase: Option<String>,
    /// Content summary.
    pub summary: PrePublishSummary,
}

impl Default for ConfirmationConfig {
    fn default() -> Self {
        Self {
            has_secrets: false,
            has_critical_secrets: false,
            secret_count: 0,
            target_domain: None,
            is_remote_publish: false,
            password_entropy_bits: 0.0,
            has_recovery_key: false,
            recovery_key_phrase: None,
            summary: PrePublishSummary {
                total_conversations: 0,
                total_messages: 0,
                total_characters: 0,
                estimated_size_bytes: 0,
                earliest_timestamp: None,
                latest_timestamp: None,
                date_histogram: Vec::new(),
                workspaces: Vec::new(),
                agents: Vec::new(),
                secret_scan: ScanReportSummary::default(),
                encryption_config: None,
                key_slots: Vec::new(),
                generated_at: chrono::Utc::now(),
            },
        }
    }
}

/// Manages the multi-step confirmation flow.
pub struct ConfirmationFlow {
    /// Current step in the flow.
    current_step: ConfirmationStep,
    /// Set of completed steps.
    completed_steps: HashSet<ConfirmationStep>,
    /// Configuration for this flow.
    config: ConfirmationConfig,
    /// Number of Enter presses for final confirmation.
    final_enter_count: u8,
    /// Password strength action if chosen.
    password_action: Option<PasswordStrengthAction>,
}

impl ConfirmationFlow {
    /// Create a new confirmation flow with the given configuration.
    pub fn new(config: ConfirmationConfig) -> Self {
        let first_step = Self::determine_first_step(&config);
        Self {
            current_step: first_step,
            completed_steps: HashSet::new(),
            config,
            final_enter_count: 0,
            password_action: None,
        }
    }

    /// Get the current step.
    pub fn current_step(&self) -> ConfirmationStep {
        self.current_step
    }

    /// Get the configuration.
    pub fn config(&self) -> &ConfirmationConfig {
        &self.config
    }

    /// Get the password action if one was chosen.
    pub fn password_action(&self) -> Option<PasswordStrengthAction> {
        self.password_action
    }

    /// Determine the first applicable step based on configuration.
    fn determine_first_step(config: &ConfirmationConfig) -> ConfirmationStep {
        if config.has_secrets {
            ConfirmationStep::SecretScanAcknowledgment
        } else {
            ConfirmationStep::ContentReview
        }
    }

    /// Check if the current step should be skipped.
    pub fn should_skip_current(&self) -> bool {
        match self.current_step {
            ConfirmationStep::SecretScanAcknowledgment => !self.config.has_secrets,
            ConfirmationStep::PublicPublishingWarning => !self.config.is_remote_publish,
            ConfirmationStep::PasswordStrengthWarning => {
                self.config.password_entropy_bits >= MIN_STRONG_PASSWORD_BITS
            }
            ConfirmationStep::RecoveryKeyBackup => !self.config.has_recovery_key,
            _ => false,
        }
    }

    /// Validate input for the secret scan acknowledgment step.
    pub fn validate_secret_ack(&self, input: &str) -> StepValidation {
        let normalized = input.trim().to_lowercase();
        if normalized == SECRET_ACK_PHRASE_NORMALIZED {
            StepValidation::Passed
        } else {
            StepValidation::Failed(format!("Please type exactly: \"{SECRET_ACK_PHRASE}\""))
        }
    }

    /// Validate input for the content review step.
    pub fn validate_content_review(&self, input: &str) -> StepValidation {
        let normalized = input.trim().to_lowercase();
        if normalized == "y" || normalized == "yes" {
            StepValidation::Passed
        } else if normalized == "r" {
            StepValidation::Failed("Return to summary".to_string())
        } else {
            StepValidation::Failed("Press Y to confirm or R to return to summary".to_string())
        }
    }

    /// Validate input for the public publishing warning step.
    pub fn validate_public_warning(&self, input: &str) -> StepValidation {
        let Some(domain) = &self.config.target_domain else {
            return StepValidation::Passed;
        };

        let expected = format!("publish to {}", domain);
        let normalized = input.trim().to_lowercase();

        if normalized == expected.to_lowercase() {
            StepValidation::Passed
        } else {
            StepValidation::Failed(format!("Please type exactly: \"publish to {}\"", domain))
        }
    }

    /// Parse password strength action from input.
    pub fn parse_password_action(&self, input: &str) -> Option<PasswordStrengthAction> {
        match input.trim().to_lowercase().as_str() {
            "s" => Some(PasswordStrengthAction::SetStronger),
            "p" => Some(PasswordStrengthAction::ProceedAnyway),
            "a" => Some(PasswordStrengthAction::Abort),
            _ => None,
        }
    }

    /// Validate input for the recovery key backup step.
    pub fn validate_recovery_key(&self, input: &str) -> StepValidation {
        let Some(phrase) = &self.config.recovery_key_phrase else {
            return StepValidation::Passed;
        };

        // Get the last word from the recovery phrase
        let last_word = phrase
            .split('-')
            .next_back()
            .or_else(|| phrase.split_whitespace().next_back())
            .unwrap_or("");

        let normalized = input.trim().to_lowercase();
        if normalized == last_word.to_lowercase() {
            StepValidation::Passed
        } else {
            StepValidation::Failed(
                "Incorrect. Please type the LAST word of the recovery key.".to_string(),
            )
        }
    }

    /// Process an Enter keypress for final confirmation.
    /// Returns true if both Enter presses have been received.
    pub fn process_final_enter(&mut self) -> bool {
        self.final_enter_count += 1;
        self.final_enter_count >= 2
    }

    /// Reset the final Enter counter (e.g., if user typed something else).
    pub fn reset_final_enter(&mut self) {
        self.final_enter_count = 0;
    }

    /// Get the number of Enter presses received for final confirmation.
    pub fn final_enter_count(&self) -> u8 {
        self.final_enter_count
    }

    /// Mark the current step as completed and advance to the next.
    pub fn complete_current_step(&mut self) {
        self.completed_steps.insert(self.current_step);
        self.advance_to_next_step();
    }

    /// Advance to the next applicable step.
    fn advance_to_next_step(&mut self) {
        let next = match self.current_step {
            ConfirmationStep::SecretScanAcknowledgment => ConfirmationStep::ContentReview,
            ConfirmationStep::ContentReview => {
                if self.config.is_remote_publish {
                    ConfirmationStep::PublicPublishingWarning
                } else if self.config.password_entropy_bits < MIN_STRONG_PASSWORD_BITS {
                    ConfirmationStep::PasswordStrengthWarning
                } else if self.config.has_recovery_key {
                    ConfirmationStep::RecoveryKeyBackup
                } else {
                    ConfirmationStep::FinalConfirmation
                }
            }
            ConfirmationStep::PublicPublishingWarning => {
                if self.config.password_entropy_bits < MIN_STRONG_PASSWORD_BITS {
                    ConfirmationStep::PasswordStrengthWarning
                } else if self.config.has_recovery_key {
                    ConfirmationStep::RecoveryKeyBackup
                } else {
                    ConfirmationStep::FinalConfirmation
                }
            }
            ConfirmationStep::PasswordStrengthWarning => {
                if self.config.has_recovery_key {
                    ConfirmationStep::RecoveryKeyBackup
                } else {
                    ConfirmationStep::FinalConfirmation
                }
            }
            ConfirmationStep::RecoveryKeyBackup => ConfirmationStep::FinalConfirmation,
            ConfirmationStep::FinalConfirmation => ConfirmationStep::FinalConfirmation,
        };

        self.current_step = next;

        // Skip steps that don't apply
        if self.should_skip_current() && self.current_step != ConfirmationStep::FinalConfirmation {
            self.advance_to_next_step();
        }
    }

    /// Check if all required steps are completed.
    pub fn is_complete(&self) -> bool {
        self.completed_steps
            .contains(&ConfirmationStep::FinalConfirmation)
    }

    /// Set the password strength action.
    pub fn set_password_action(&mut self, action: PasswordStrengthAction) {
        self.password_action = Some(action);
    }

    /// Get the list of completed steps for display.
    pub fn completed_steps_summary(&self) -> Vec<(ConfirmationStep, &'static str)> {
        let mut steps = Vec::new();

        if self.config.has_secrets
            && self
                .completed_steps
                .contains(&ConfirmationStep::SecretScanAcknowledgment)
        {
            steps.push((
                ConfirmationStep::SecretScanAcknowledgment,
                "Secrets acknowledged",
            ));
        }

        if self
            .completed_steps
            .contains(&ConfirmationStep::ContentReview)
        {
            steps.push((ConfirmationStep::ContentReview, "Content reviewed"));
        }

        if self.config.is_remote_publish
            && self
                .completed_steps
                .contains(&ConfirmationStep::PublicPublishingWarning)
        {
            steps.push((
                ConfirmationStep::PublicPublishingWarning,
                "Public URL confirmed",
            ));
        }

        if self.config.password_entropy_bits < MIN_STRONG_PASSWORD_BITS
            && self
                .completed_steps
                .contains(&ConfirmationStep::PasswordStrengthWarning)
        {
            let label = match self.password_action {
                Some(PasswordStrengthAction::ProceedAnyway) => "Password warning acknowledged",
                _ => "Password strength confirmed",
            };
            steps.push((ConfirmationStep::PasswordStrengthWarning, label));
        }

        if self.config.has_recovery_key
            && self
                .completed_steps
                .contains(&ConfirmationStep::RecoveryKeyBackup)
        {
            steps.push((ConfirmationStep::RecoveryKeyBackup, "Recovery key saved"));
        }

        steps
    }
}

/// Calculate password entropy using character class analysis.
///
/// This is a simple estimate based on character classes:
/// - Lowercase letters: 26 characters (log2(26) ≈ 4.7 bits each)
/// - Uppercase letters: 26 characters (log2(26) ≈ 4.7 bits each)
/// - Digits: 10 characters (log2(10) ≈ 3.3 bits each)
/// - Symbols: ~32 characters (log2(32) = 5 bits each)
///
/// Total entropy = length × log2(pool_size)
pub fn estimate_password_entropy(password: &str) -> f64 {
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
    let length = password.chars().count() as f64;

    bits_per_char * length
}

/// Get a human-readable password strength label.
pub fn password_strength_label(entropy_bits: f64) -> &'static str {
    if entropy_bits >= 80.0 {
        "Very Strong"
    } else if entropy_bits >= 60.0 {
        "Strong"
    } else if entropy_bits >= 40.0 {
        "Fair"
    } else if entropy_bits >= 20.0 {
        "Weak"
    } else {
        "Very Weak"
    }
}

/// Get the number of required steps for the given configuration.
pub fn count_required_steps(config: &ConfirmationConfig) -> usize {
    let mut count = 2; // ContentReview and FinalConfirmation are always required

    if config.has_secrets {
        count += 1;
    }
    if config.is_remote_publish {
        count += 1;
    }
    if config.password_entropy_bits < MIN_STRONG_PASSWORD_BITS {
        count += 1;
    }
    if config.has_recovery_key {
        count += 1;
    }

    count
}

/// Required phrase for unencrypted export acknowledgment.
pub const UNENCRYPTED_ACK_PHRASE: &str = "I UNDERSTAND AND ACCEPT THE RISKS";

/// Exit code for unconfirmed unencrypted export.
pub const EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED: i32 = 3;

const UNENCRYPTED_BLOCKED_ERROR_KIND: &str = "unencrypted_blocked";
const UNENCRYPTED_BLOCKED_MESSAGE: &str = "Unencrypted exports are not allowed in robot mode";
const UNENCRYPTED_BLOCKED_SUGGESTION: &str =
    "Use --i-understand-unencrypted-risks flag if you really need this";

/// Result of unencrypted export confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnencryptedConfirmResult {
    /// User confirmed with correct phrase.
    Confirmed,
    /// User cancelled (wrong phrase or explicit cancel).
    Cancelled,
    /// Blocked in robot mode (no override flag).
    RobotModeBlocked,
}

/// Validate unencrypted export acknowledgment phrase.
///
/// Requires exact match (case-insensitive) of "I UNDERSTAND AND ACCEPT THE RISKS".
pub fn validate_unencrypted_ack(input: &str) -> StepValidation {
    let normalized = input.trim().to_uppercase();
    if normalized == UNENCRYPTED_ACK_PHRASE {
        StepValidation::Passed
    } else {
        StepValidation::Failed(format!(
            "Please type exactly: \"{}\"",
            UNENCRYPTED_ACK_PHRASE
        ))
    }
}

/// Check if robot mode allows unencrypted export.
///
/// In robot/JSON mode, unencrypted exports are blocked unless
/// `--i-understand-unencrypted-risks` flag is provided.
pub fn check_robot_mode_unencrypted(
    is_robot_mode: bool,
    has_override_flag: bool,
) -> UnencryptedConfirmResult {
    if is_robot_mode && !has_override_flag {
        UnencryptedConfirmResult::RobotModeBlocked
    } else {
        UnencryptedConfirmResult::Confirmed
    }
}

/// Generate the error JSON for blocked robot mode unencrypted export.
pub fn robot_mode_blocked_error() -> serde_json::Value {
    serde_json::json!({
        "error": UNENCRYPTED_BLOCKED_ERROR_KIND,
        "message": UNENCRYPTED_BLOCKED_MESSAGE,
        "suggestion": UNENCRYPTED_BLOCKED_SUGGESTION,
        "exit_code": EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED
    })
}

/// Format warning messages for unencrypted export.
pub fn unencrypted_warning_lines() -> Vec<&'static str> {
    vec![
        "You are about to export WITHOUT ENCRYPTION.",
        "",
        "This means:",
        "  • All conversation content will be publicly readable",
        "  • Anyone with the URL can view your data",
        "  • Search engines may index your content",
        "  • There is NO way to restrict access later",
        "",
        "This is IRREVERSIBLE once deployed.",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_basic_config() -> ConfirmationConfig {
        ConfirmationConfig {
            has_secrets: false,
            has_critical_secrets: false,
            secret_count: 0,
            target_domain: None,
            is_remote_publish: false,
            password_entropy_bits: 80.0,
            has_recovery_key: false,
            recovery_key_phrase: None,
            ..Default::default()
        }
    }

    fn basic_flow_with(configure: impl FnOnce(&mut ConfirmationConfig)) -> ConfirmationFlow {
        let mut config = make_basic_config();
        configure(&mut config);
        ConfirmationFlow::new(config)
    }

    #[test]
    fn test_basic_flow_no_secrets() {
        let config = make_basic_config();
        let flow = ConfirmationFlow::new(config);

        // Should start at ContentReview (no secrets)
        assert_eq!(flow.current_step(), ConfirmationStep::ContentReview);
    }

    #[test]
    fn test_flow_with_secrets() {
        let mut config = make_basic_config();
        config.has_secrets = true;

        let flow = ConfirmationFlow::new(config);

        // Should start at SecretScanAcknowledgment
        assert_eq!(
            flow.current_step(),
            ConfirmationStep::SecretScanAcknowledgment
        );
    }

    #[test]
    fn test_secret_ack_validation() {
        let flow = basic_flow_with(|config| {
            config.has_secrets = true;
        });

        // Wrong phrase
        assert_eq!(
            flow.validate_secret_ack("i understand"),
            StepValidation::Failed("Please type exactly: \"I understand the risks\"".to_string())
        );

        // Correct phrase (case insensitive)
        assert_eq!(
            flow.validate_secret_ack("I UNDERSTAND THE RISKS"),
            StepValidation::Passed
        );
        assert_eq!(
            flow.validate_secret_ack("i understand the risks"),
            StepValidation::Passed
        );
    }

    #[test]
    fn test_public_warning_validation() {
        let flow = basic_flow_with(|config| {
            config.is_remote_publish = true;
            config.target_domain = Some("user.github.io".to_string());
        });

        // Wrong phrase
        assert!(matches!(
            flow.validate_public_warning("publish"),
            StepValidation::Failed(_)
        ));

        // Correct phrase
        assert_eq!(
            flow.validate_public_warning("publish to user.github.io"),
            StepValidation::Passed
        );
    }

    #[test]
    fn test_recovery_key_validation() {
        let flow = basic_flow_with(|config| {
            config.has_recovery_key = true;
            config.recovery_key_phrase = Some("forge-table-river-cloud-dance".to_string());
        });

        // Wrong word
        assert!(matches!(
            flow.validate_recovery_key("river"),
            StepValidation::Failed(_)
        ));

        // Correct last word
        assert_eq!(flow.validate_recovery_key("dance"), StepValidation::Passed);
    }

    #[test]
    fn test_final_confirmation_double_enter() {
        let config = make_basic_config();
        let mut flow = ConfirmationFlow::new(config);

        // First Enter
        assert!(!flow.process_final_enter());
        assert_eq!(flow.final_enter_count(), 1);

        // Second Enter
        assert!(flow.process_final_enter());
        assert_eq!(flow.final_enter_count(), 2);
    }

    #[test]
    fn test_step_advancement() {
        let mut config = make_basic_config();
        config.has_secrets = true;
        config.is_remote_publish = true;
        config.target_domain = Some("test.github.io".to_string());
        config.has_recovery_key = true;
        config.recovery_key_phrase = Some("word1-word2-word3".to_string());

        let mut flow = ConfirmationFlow::new(config);

        // Start at SecretScanAcknowledgment
        assert_eq!(
            flow.current_step(),
            ConfirmationStep::SecretScanAcknowledgment
        );

        flow.complete_current_step();
        assert_eq!(flow.current_step(), ConfirmationStep::ContentReview);

        flow.complete_current_step();
        assert_eq!(
            flow.current_step(),
            ConfirmationStep::PublicPublishingWarning
        );

        flow.complete_current_step();
        // Skips PasswordStrengthWarning (entropy >= 60)
        assert_eq!(flow.current_step(), ConfirmationStep::RecoveryKeyBackup);

        flow.complete_current_step();
        assert_eq!(flow.current_step(), ConfirmationStep::FinalConfirmation);
    }

    #[test]
    fn test_password_entropy_estimation() {
        // Empty password
        assert_eq!(estimate_password_entropy(""), 0.0);

        // Simple lowercase
        let entropy = estimate_password_entropy("password");
        assert!(entropy > 30.0 && entropy < 40.0); // ~37.6 bits

        // Mixed case + digits + symbols
        let entropy = estimate_password_entropy("P@ssw0rd!");
        assert!(entropy > 50.0); // Higher due to larger character pool
    }

    #[test]
    fn test_password_strength_label() {
        for (entropy_bits, expected_label) in [
            (10.0, "Very Weak"),
            (30.0, "Weak"),
            (50.0, "Fair"),
            (70.0, "Strong"),
            (90.0, "Very Strong"),
        ] {
            assert_eq!(
                password_strength_label(entropy_bits),
                expected_label,
                "entropy_bits={entropy_bits}"
            );
        }
    }

    #[test]
    fn test_count_required_steps() {
        let config = make_basic_config();
        assert_eq!(count_required_steps(&config), 2); // ContentReview + FinalConfirmation

        let mut config = make_basic_config();
        config.has_secrets = true;
        config.is_remote_publish = true;
        config.password_entropy_bits = 30.0;
        config.has_recovery_key = true;
        assert_eq!(count_required_steps(&config), 6); // All steps
    }

    #[test]
    fn test_content_review_validation() {
        let config = make_basic_config();
        let flow = ConfirmationFlow::new(config);

        assert_eq!(flow.validate_content_review("y"), StepValidation::Passed);
        assert_eq!(flow.validate_content_review("Y"), StepValidation::Passed);
        assert_eq!(flow.validate_content_review("yes"), StepValidation::Passed);
        assert!(matches!(
            flow.validate_content_review("n"),
            StepValidation::Failed(_)
        ));
    }

    #[test]
    fn test_password_action_parsing() {
        let config = make_basic_config();
        let flow = ConfirmationFlow::new(config);

        for (input, expected) in [
            ("s", Some(PasswordStrengthAction::SetStronger)),
            ("P", Some(PasswordStrengthAction::ProceedAnyway)),
            ("a", Some(PasswordStrengthAction::Abort)),
        ] {
            assert_eq!(flow.parse_password_action(input), expected, "input={input}");
        }
        assert_eq!(flow.parse_password_action("x"), None);
    }

    #[test]
    fn test_completed_steps_summary() {
        let mut config = make_basic_config();
        config.has_secrets = true;
        config.is_remote_publish = true;
        config.target_domain = Some("test.github.io".to_string());

        let mut flow = ConfirmationFlow::new(config);

        // Complete secret ack
        flow.complete_current_step();

        // Complete content review
        flow.complete_current_step();

        let summary = flow.completed_steps_summary();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].1, "Secrets acknowledged");
        assert_eq!(summary[1].1, "Content reviewed");
    }

    #[test]
    fn test_unencrypted_ack_validation() {
        // Correct phrase (exact match)
        assert_eq!(
            validate_unencrypted_ack("I UNDERSTAND AND ACCEPT THE RISKS"),
            StepValidation::Passed
        );

        // Correct phrase (case insensitive)
        assert_eq!(
            validate_unencrypted_ack("i understand and accept the risks"),
            StepValidation::Passed
        );

        // Correct phrase with whitespace
        assert_eq!(
            validate_unencrypted_ack("  I UNDERSTAND AND ACCEPT THE RISKS  "),
            StepValidation::Passed
        );

        // Incorrect phrases
        assert!(matches!(
            validate_unencrypted_ack("I understand"),
            StepValidation::Failed(_)
        ));
        assert!(matches!(
            validate_unencrypted_ack("yes"),
            StepValidation::Failed(_)
        ));
        assert!(matches!(
            validate_unencrypted_ack("I ACCEPT THE RISKS"),
            StepValidation::Failed(_)
        ));
    }

    #[test]
    fn test_robot_mode_unencrypted_check() {
        // Not robot mode - always allowed
        assert_eq!(
            check_robot_mode_unencrypted(false, false),
            UnencryptedConfirmResult::Confirmed
        );

        // Robot mode with override flag - allowed
        assert_eq!(
            check_robot_mode_unencrypted(true, true),
            UnencryptedConfirmResult::Confirmed
        );

        // Robot mode without override flag - blocked
        assert_eq!(
            check_robot_mode_unencrypted(true, false),
            UnencryptedConfirmResult::RobotModeBlocked
        );
    }

    #[test]
    fn test_robot_mode_blocked_error() {
        let error = robot_mode_blocked_error();
        assert_eq!(
            error,
            serde_json::json!({
                "error": "unencrypted_blocked",
                "message": "Unencrypted exports are not allowed in robot mode",
                "suggestion": "Use --i-understand-unencrypted-risks flag if you really need this",
                "exit_code": EXIT_CODE_UNENCRYPTED_NOT_CONFIRMED
            })
        );
    }

    #[test]
    fn test_unencrypted_warning_lines() {
        let lines = unencrypted_warning_lines();
        assert!(!lines.is_empty());
        assert!(lines[0].contains("WITHOUT ENCRYPTION"));
    }
}
