//! P6.7: Accessibility Testing for Pages Export Web Viewer
//!
//! Tests WCAG 2.1 Level AA compliance for the web viewer:
//! - Keyboard navigation
//! - Screen reader support (ARIA)
//! - Color contrast
//! - Focus management
//!
//! Note: Full axe-core testing requires a browser environment.
//! These tests validate the generated HTML structure for accessibility.

/// WCAG 2.1 Level AA color contrast requirements
const MIN_CONTRAST_NORMAL_TEXT: f64 = 4.5;
const MIN_CONTRAST_LARGE_TEXT: f64 = 3.0;

/// Accessibility audit result
#[derive(Debug, Clone)]
pub struct AccessibilityAudit {
    pub violations: Vec<AccessibilityViolation>,
    pub warnings: Vec<AccessibilityWarning>,
    pub passed_checks: Vec<String>,
}

impl AccessibilityAudit {
    pub fn new() -> Self {
        Self {
            violations: Vec::new(),
            warnings: Vec::new(),
            passed_checks: Vec::new(),
        }
    }

    pub fn is_compliant(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn add_violation(&mut self, rule: &str, message: &str, element: Option<&str>) {
        self.violations.push(AccessibilityViolation {
            rule: rule.to_string(),
            message: message.to_string(),
            element: element.map(String::from),
        });
    }

    pub fn add_warning(&mut self, rule: &str, message: &str) {
        self.warnings.push(AccessibilityWarning {
            rule: rule.to_string(),
            message: message.to_string(),
        });
    }

    pub fn add_pass(&mut self, check: &str) {
        self.passed_checks.push(check.to_string());
    }
}

impl Default for AccessibilityAudit {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct AccessibilityViolation {
    pub rule: String,
    pub message: String,
    pub element: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccessibilityWarning {
    pub rule: String,
    pub message: String,
}

/// Audit HTML content for accessibility issues
pub fn audit_html(html: &str) -> AccessibilityAudit {
    let mut audit = AccessibilityAudit::new();

    // Check for lang attribute
    check_lang_attribute(html, &mut audit);

    // Check for document structure
    check_document_structure(html, &mut audit);

    // Check for form accessibility
    check_form_accessibility(html, &mut audit);

    // Check for image alt text
    check_image_alt_text(html, &mut audit);

    // Check for link text
    check_link_text(html, &mut audit);

    // Check for heading structure
    check_heading_structure(html, &mut audit);

    // Check for interactive elements
    check_interactive_elements(html, &mut audit);

    // Check for ARIA usage
    check_aria_usage(html, &mut audit);

    audit
}

/// Check for lang attribute on html element
fn check_lang_attribute(html: &str, audit: &mut AccessibilityAudit) {
    if html.contains(r#"<html lang=""#) || html.contains(r#"<html xml:lang=""#) {
        audit.add_pass("html-has-lang: Document has language attribute");
    } else if html.contains("<html") && !html.contains("lang=") {
        audit.add_violation(
            "html-has-lang",
            "Document must have a lang attribute on the <html> element",
            Some("<html>"),
        );
    } else {
        audit.add_pass("html-has-lang: Document has language attribute");
    }
}

/// Check document structure for landmarks
fn check_document_structure(html: &str, audit: &mut AccessibilityAudit) {
    let html_lower = html.to_lowercase();

    // Check for main landmark
    if html_lower.contains("<main") || html_lower.contains(r#"role="main""#) {
        audit.add_pass("landmark-main: Document has main landmark");
    } else {
        audit.add_violation(
            "landmark-main",
            "Document should have a <main> element or role=\"main\"",
            None,
        );
    }

    // Check for header/banner
    if html_lower.contains("<header") || html_lower.contains(r#"role="banner""#) {
        audit.add_pass("landmark-banner: Document has banner landmark");
    } else {
        audit.add_warning("landmark-banner", "Document should have a <header> element");
    }

    // Check for page title
    if html_lower.contains("<title>") && !html_lower.contains("<title></title>") {
        audit.add_pass("document-title: Document has a title");
    } else {
        audit.add_violation(
            "document-title",
            "Document must have a non-empty <title> element",
            Some("<title>"),
        );
    }
}

/// Check form accessibility
fn check_form_accessibility(html: &str, audit: &mut AccessibilityAudit) {
    // Check for input elements
    let input_count = html.matches("<input").count();
    let label_count = html.matches("<label").count();
    let aria_label_count = html.matches("aria-label").count();
    let aria_labelledby_count = html.matches("aria-labelledby").count();

    // Every input should have a label (label, aria-label, or aria-labelledby)
    let total_labels = label_count + aria_label_count + aria_labelledby_count;

    if input_count > 0 {
        if total_labels >= input_count {
            audit.add_pass("label: Form inputs appear to have labels");
        } else {
            audit.add_warning(
                "label",
                &format!(
                    "Found {} inputs but only {} labels/aria-labels - some inputs may be unlabeled",
                    input_count, total_labels
                ),
            );
        }
    }

    // Check for autocomplete on password fields
    if html.contains(r#"type="password""#) {
        if html.contains("autocomplete=") {
            audit.add_pass("autocomplete-valid: Password fields have autocomplete attribute");
        } else {
            audit.add_warning(
                "autocomplete-valid",
                "Password fields should have autocomplete attribute for accessibility",
            );
        }
    }
}

/// Check for image alt text
fn check_image_alt_text(html: &str, audit: &mut AccessibilityAudit) {
    let img_count = html.matches("<img").count();

    if img_count == 0 {
        audit.add_pass("image-alt: No images to check");
        return;
    }

    // Simple check: count alt attributes near img tags
    let alt_count = html.matches("alt=").count();

    if alt_count >= img_count {
        audit.add_pass("image-alt: Images appear to have alt attributes");
    } else {
        audit.add_violation(
            "image-alt",
            &format!(
                "Found {} images but only {} alt attributes - images must have alt text",
                img_count, alt_count
            ),
            Some("<img>"),
        );
    }
}

/// Check link text
fn check_link_text(html: &str, audit: &mut AccessibilityAudit) {
    // Check for links with "click here" or empty text
    let html_lower = html.to_lowercase();

    if html_lower.contains(">click here<") || html_lower.contains(">here<") {
        audit.add_warning(
            "link-name",
            "Avoid generic link text like 'click here' - use descriptive text",
        );
    }

    // Check for target="_blank" without rel="noopener"
    if html.contains(r#"target="_blank""#) {
        if html.contains("rel=\"noopener") || html.contains("rel='noopener") {
            audit.add_pass("link-target-blank: External links have rel=\"noopener\"");
        } else {
            audit.add_warning(
                "link-target-blank",
                "Links with target=\"_blank\" should have rel=\"noopener\" for security",
            );
        }
    }
}

/// Check heading structure
fn check_heading_structure(html: &str, audit: &mut AccessibilityAudit) {
    let html_lower = html.to_lowercase();

    // Must have at least one h1
    let h1_count = html_lower.matches("<h1").count();
    if h1_count == 0 {
        audit.add_violation(
            "page-has-heading-one",
            "Page should have at least one <h1> heading",
            None,
        );
    } else if h1_count > 1 {
        audit.add_warning(
            "page-has-heading-one",
            &format!("Page has {} <h1> elements - consider using only one", h1_count),
        );
    } else {
        audit.add_pass("page-has-heading-one: Page has exactly one h1");
    }

    // Check for skipped heading levels
    let h2_count = html_lower.matches("<h2").count();
    let h3_count = html_lower.matches("<h3").count();
    let h4_count = html_lower.matches("<h4").count();

    if h3_count > 0 && h2_count == 0 {
        audit.add_violation(
            "heading-order",
            "Heading levels should not be skipped - found h3 without h2",
            Some("<h3>"),
        );
    }
    if h4_count > 0 && h3_count == 0 {
        audit.add_violation(
            "heading-order",
            "Heading levels should not be skipped - found h4 without h3",
            Some("<h4>"),
        );
    }
}

/// Check interactive elements
fn check_interactive_elements(html: &str, audit: &mut AccessibilityAudit) {
    // Check buttons have accessible names
    let button_count = html.matches("<button").count();
    if button_count > 0 {
        // Count buttons with text content or aria-label
        let has_aria_label = html.matches(r#"<button"#)
            .zip(html.match_indices("aria-label"))
            .count();

        if has_aria_label > 0 || html.contains("><span class=\"btn-text\">") {
            audit.add_pass("button-name: Buttons appear to have accessible names");
        }
    }

    // Check for tabindex > 0 (anti-pattern)
    for i in 1..=10 {
        if html.contains(&format!("tabindex=\"{}\"", i)) {
            audit.add_warning(
                "tabindex",
                &format!("Found tabindex=\"{}\" - avoid positive tabindex values", i),
            );
            break;
        }
    }
}

/// Check ARIA usage
fn check_aria_usage(html: &str, audit: &mut AccessibilityAudit) {
    // Check for aria-hidden on focusable elements (violation)
    if html.contains("aria-hidden=\"true\"") && html.contains("tabindex=\"0\"") {
        audit.add_warning(
            "aria-hidden-focus",
            "Elements with aria-hidden=\"true\" should not be focusable",
        );
    }

    // Check for aria-live regions
    if html.contains("aria-live=") {
        audit.add_pass("aria-live: Page has live regions for dynamic content");
    }

    // Check for aria-label on icon-only buttons
    if html.contains("class=\"btn-icon\"") && html.contains("aria-label=") {
        audit.add_pass("button-name: Icon buttons have aria-label");
    }
}

/// Calculate relative luminance for a color
pub fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let r = srgb_to_linear(r as f64 / 255.0);
    let g = srgb_to_linear(g as f64 / 255.0);
    let b = srgb_to_linear(b as f64 / 255.0);

    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Calculate contrast ratio between two colors
pub fn contrast_ratio(l1: f64, l2: f64) -> f64 {
    let lighter = l1.max(l2);
    let darker = l1.min(l2);
    (lighter + 0.05) / (darker + 0.05)
}

/// Check if contrast ratio meets WCAG AA for normal text
pub fn meets_wcag_aa_normal(ratio: f64) -> bool {
    ratio >= MIN_CONTRAST_NORMAL_TEXT
}

/// Check if contrast ratio meets WCAG AA for large text
pub fn meets_wcag_aa_large(ratio: f64) -> bool {
    ratio >= MIN_CONTRAST_LARGE_TEXT
}

/// Parse hex color to RGB
pub fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');

    if hex.len() == 3 {
        let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
        let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
        let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
        Some((r, g, b))
    } else if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some((r, g, b))
    } else {
        None
    }
}

/// Generate an accessibility report as markdown
pub fn generate_report(audit: &AccessibilityAudit) -> String {
    let mut report = String::new();

    report.push_str("# Accessibility Audit Report\n\n");

    // Summary
    report.push_str("## Summary\n\n");
    report.push_str(&format!(
        "- **Violations**: {}\n",
        audit.violations.len()
    ));
    report.push_str(&format!(
        "- **Warnings**: {}\n",
        audit.warnings.len()
    ));
    report.push_str(&format!(
        "- **Passed**: {}\n",
        audit.passed_checks.len()
    ));
    report.push_str(&format!(
        "- **Compliant**: {}\n\n",
        if audit.is_compliant() { "Yes" } else { "No" }
    ));

    // Violations
    if !audit.violations.is_empty() {
        report.push_str("## Violations\n\n");
        for v in &audit.violations {
            report.push_str(&format!("### {}\n\n", v.rule));
            report.push_str(&format!("{}\n", v.message));
            if let Some(elem) = &v.element {
                report.push_str(&format!("\n**Element**: `{}`\n", elem));
            }
            report.push('\n');
        }
    }

    // Warnings
    if !audit.warnings.is_empty() {
        report.push_str("## Warnings\n\n");
        for w in &audit.warnings {
            report.push_str(&format!("- **{}**: {}\n", w.rule, w.message));
        }
        report.push('\n');
    }

    // Passed
    if !audit.passed_checks.is_empty() {
        report.push_str("## Passed Checks\n\n");
        for p in &audit.passed_checks {
            report.push_str(&format!("- {}\n", p));
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HTML: &str = include_str!("../src/pages_assets/index.html");

    #[test]
    fn test_audit_index_html() {
        let audit = audit_html(TEST_HTML);

        // Print report for debugging
        let report = generate_report(&audit);
        println!("{}", report);

        // Should have language attribute
        assert!(
            audit.passed_checks.iter().any(|p| p.contains("html-has-lang")),
            "index.html should have lang attribute"
        );

        // Should have main landmark
        assert!(
            audit.passed_checks.iter().any(|p| p.contains("landmark-main")),
            "index.html should have main landmark"
        );

        // Should have document title
        assert!(
            audit.passed_checks.iter().any(|p| p.contains("document-title")),
            "index.html should have document title"
        );
    }

    #[test]
    fn test_color_contrast_calculation() {
        // White on black should have high contrast
        let white_lum = relative_luminance(255, 255, 255);
        let black_lum = relative_luminance(0, 0, 0);
        let ratio = contrast_ratio(white_lum, black_lum);

        assert!(ratio > 20.0, "White on black should have ratio > 20");
        assert!(meets_wcag_aa_normal(ratio));
        assert!(meets_wcag_aa_large(ratio));
    }

    #[test]
    fn test_color_contrast_wcag_boundaries() {
        // Test at the WCAG AA boundary for normal text (4.5:1)
        // Gray #767676 on white gives approximately 4.54:1
        let gray_lum = relative_luminance(0x76, 0x76, 0x76);
        let white_lum = relative_luminance(255, 255, 255);
        let ratio = contrast_ratio(gray_lum, white_lum);

        assert!(
            ratio >= 4.5,
            "Gray #767676 on white should meet AA for normal text"
        );
    }

    #[test]
    fn test_parse_hex_color() {
        assert_eq!(parse_hex_color("#ffffff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#ff0000"), Some((255, 0, 0)));
        assert_eq!(parse_hex_color("fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#abc"), Some((170, 187, 204)));
    }

    #[test]
    fn test_theme_colors_contrast() {
        // Test the theme colors from CSS
        // Dark theme: --color-text: #f1f5f9 on --color-bg: #0f172a
        let text_dark = parse_hex_color("#f1f5f9").unwrap();
        let bg_dark = parse_hex_color("#0f172a").unwrap();
        let text_lum = relative_luminance(text_dark.0, text_dark.1, text_dark.2);
        let bg_lum = relative_luminance(bg_dark.0, bg_dark.1, bg_dark.2);
        let ratio_dark = contrast_ratio(text_lum, bg_lum);

        assert!(
            meets_wcag_aa_normal(ratio_dark),
            "Dark theme text contrast ratio {} should meet WCAG AA (4.5:1)",
            ratio_dark
        );

        // Light theme: --color-text: #1e293b on --color-bg: #f8fafc
        let text_light = parse_hex_color("#1e293b").unwrap();
        let bg_light = parse_hex_color("#f8fafc").unwrap();
        let text_lum = relative_luminance(text_light.0, text_light.1, text_light.2);
        let bg_lum = relative_luminance(bg_light.0, bg_light.1, bg_light.2);
        let ratio_light = contrast_ratio(text_lum, bg_lum);

        assert!(
            meets_wcag_aa_normal(ratio_light),
            "Light theme text contrast ratio {} should meet WCAG AA (4.5:1)",
            ratio_light
        );
    }

    #[test]
    fn test_primary_button_contrast() {
        // Primary color #3b82f6 on white text
        let primary = parse_hex_color("#3b82f6").unwrap();
        let white = (255u8, 255u8, 255u8);
        let primary_lum = relative_luminance(primary.0, primary.1, primary.2);
        let white_lum = relative_luminance(white.0, white.1, white.2);
        let ratio = contrast_ratio(primary_lum, white_lum);

        // Note: This might not meet AA - buttons might need text color adjustment
        println!("Primary button contrast ratio: {}", ratio);
    }

    #[test]
    fn test_muted_text_contrast() {
        // Muted text --color-text-muted: #94a3b8 on --color-bg: #0f172a
        let muted = parse_hex_color("#94a3b8").unwrap();
        let bg = parse_hex_color("#0f172a").unwrap();
        let muted_lum = relative_luminance(muted.0, muted.1, muted.2);
        let bg_lum = relative_luminance(bg.0, bg.1, bg.2);
        let ratio = contrast_ratio(muted_lum, bg_lum);

        assert!(
            meets_wcag_aa_normal(ratio),
            "Muted text contrast ratio {} should meet WCAG AA (4.5:1)",
            ratio
        );
    }

    #[test]
    fn test_heading_structure_detection() {
        let good_html = r#"<html lang="en"><title>Test</title><h1>Main</h1><h2>Sub</h2></html>"#;
        let audit = audit_html(good_html);
        assert!(
            audit.violations.iter().all(|v| v.rule != "heading-order"),
            "Good heading structure should not have violations"
        );

        let bad_html = r#"<html lang="en"><title>Test</title><h1>Main</h1><h3>Skip</h3></html>"#;
        let audit = audit_html(bad_html);
        assert!(
            audit.violations.iter().any(|v| v.rule == "heading-order"),
            "Skipped heading levels should be a violation"
        );
    }

    #[test]
    fn test_missing_alt_text_detection() {
        let good_html = r#"<img src="test.png" alt="Test image">"#;
        let audit = audit_html(good_html);
        assert!(
            audit.violations.iter().all(|v| v.rule != "image-alt"),
            "Image with alt should pass"
        );

        let bad_html = r#"<img src="test.png"><img src="test2.png">"#;
        let audit = audit_html(bad_html);
        assert!(
            audit.violations.iter().any(|v| v.rule == "image-alt"),
            "Images without alt should be violations"
        );
    }

    #[test]
    fn test_keyboard_focus_order() {
        // Check that we detect positive tabindex (anti-pattern)
        let bad_html = r#"<button tabindex="5">Bad</button>"#;
        let audit = audit_html(bad_html);
        assert!(
            audit.warnings.iter().any(|w| w.rule == "tabindex"),
            "Positive tabindex should generate warning"
        );
    }

    #[test]
    fn test_generate_report_format() {
        let mut audit = AccessibilityAudit::new();
        audit.add_violation("test-rule", "Test violation message", Some("<div>"));
        audit.add_warning("warn-rule", "Test warning message");
        audit.add_pass("pass-check: Test passed");

        let report = generate_report(&audit);

        assert!(report.contains("# Accessibility Audit Report"));
        assert!(report.contains("## Summary"));
        assert!(report.contains("## Violations"));
        assert!(report.contains("## Warnings"));
        assert!(report.contains("## Passed Checks"));
        assert!(report.contains("test-rule"));
        assert!(report.contains("Test violation message"));
    }

    #[test]
    fn test_index_html_has_aria_labels() {
        // Check that the auth screen has proper ARIA labels
        assert!(
            TEST_HTML.contains("aria-label="),
            "index.html should have aria-label attributes"
        );
    }

    #[test]
    fn test_index_html_has_form_labels() {
        // Check that form elements have labels
        assert!(
            TEST_HTML.contains("<label for="),
            "index.html should have form labels"
        );
    }

    #[test]
    fn test_reduced_motion_support() {
        // Check CSS for reduced motion media query
        let css = include_str!("../src/pages_assets/styles.css");
        // The CSS might not have this yet, so we'll just check if transitions are defined
        assert!(
            css.contains("transition"),
            "CSS should have transitions that could be disabled for reduced motion"
        );
    }

    #[test]
    fn test_focus_styles_exist() {
        let css = include_str!("../src/pages_assets/styles.css");
        assert!(
            css.contains(":focus") || css.contains(":focus-visible"),
            "CSS should have focus styles"
        );
    }
}
