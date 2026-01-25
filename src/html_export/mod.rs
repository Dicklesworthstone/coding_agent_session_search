//! HTML export module for self-contained session exports.
//!
//! This module generates standalone HTML files from coding agent session transcripts.
//! The exported files are:
//! - **Self-contained**: All critical CSS/JS inlined for offline operation
//! - **Progressive enhancement**: CDN resources enhance but don't break offline view
//! - **Encrypted (optional)**: Web Crypto compatible encryption for sensitive content
//! - **Accessible**: Semantic HTML with proper ARIA attributes
//!
//! # Architecture
//!
//! ```text
//! html_export/
//! ├── mod.rs           # Module facade (this file)
//! ├── template.rs      # Core HTML template generation
//! ├── styles.rs        # CSS (critical inline + Tailwind CDN fallback)
//! ├── scripts.rs       # JS (decryption, search, theme toggle)
//! ├── renderer.rs      # Conversation -> HTML rendering
//! ├── filename.rs      # Smart filename generation
//! └── encryption.rs    # Web Crypto compatible encryption
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use cass::html_export::{HtmlExporter, ExportOptions};
//!
//! let exporter = HtmlExporter::new();
//! let html = exporter.export_session(&session, ExportOptions::default())?;
//! std::fs::write("session.html", html)?;
//! ```

mod encryption;
mod filename;
mod renderer;
mod scripts;
mod styles;
mod template;

// Re-export public API
pub use encryption::{EncryptedContent, EncryptionError, EncryptionParams, encrypt_content};
pub use filename::{FilenameOptions, generate_filename};
pub use renderer::{RenderError, RenderOptions, render_conversation};
pub use scripts::{ScriptBundle, generate_scripts};
pub use styles::{StyleBundle, generate_styles};
pub use template::{ExportOptions, HtmlExporter, HtmlTemplate, TemplateError};

/// Color palette matching TUI theme.rs for visual consistency.
///
/// These CSS custom properties are injected into the HTML template,
/// ensuring exported files match the TUI aesthetics.
pub mod colors {
    /// Deep background - primary canvas color (#1a1b26)
    pub const BG_DEEP: &str = "#1a1b26";

    /// Elevated surface - cards, modals, popups (#24283b)
    pub const BG_SURFACE: &str = "#24283b";

    /// Subtle surface - hover states, selected items (#292e42)
    pub const BG_HIGHLIGHT: &str = "#292e42";

    /// Border color - subtle separators (#3b4261)
    pub const BORDER: &str = "#3b4261";

    /// Border accent - focused/active elements (#7d91c8)
    pub const BORDER_FOCUS: &str = "#7d91c8";

    /// Primary text - headings, important content (#c0caf5)
    pub const TEXT_PRIMARY: &str = "#c0caf5";

    /// Secondary text - body content (#a9b1d6)
    pub const TEXT_SECONDARY: &str = "#a9b1d6";

    /// Muted text - hints, placeholders, timestamps (#696e9e)
    pub const TEXT_MUTED: &str = "#696e9e";

    /// Disabled/inactive text (#444b6a)
    pub const TEXT_DISABLED: &str = "#444b6a";

    /// Primary accent - main actions, links (#7aa2f7)
    pub const ACCENT_PRIMARY: &str = "#7aa2f7";

    /// Secondary accent - complementary highlights (#bb9af7)
    pub const ACCENT_SECONDARY: &str = "#bb9af7";

    /// Tertiary accent - subtle highlights (#7dcfff)
    pub const ACCENT_TERTIARY: &str = "#7dcfff";

    /// User messages - soft sage green (#9ece6a)
    pub const ROLE_USER: &str = "#9ece6a";

    /// Agent/Assistant messages - primary accent (#7aa2f7)
    pub const ROLE_AGENT: &str = "#7aa2f7";

    /// Tool invocations - warm peach (#ff9e64)
    pub const ROLE_TOOL: &str = "#ff9e64";

    /// System messages - soft amber (#e0af68)
    pub const ROLE_SYSTEM: &str = "#e0af68";

    /// Success states (#73daca)
    pub const STATUS_SUCCESS: &str = "#73daca";

    /// Warning states (#e0af68)
    pub const STATUS_WARNING: &str = "#e0af68";

    /// Error states (#f7768e)
    pub const STATUS_ERROR: &str = "#f7768e";

    /// Info states (#7dcfff)
    pub const STATUS_INFO: &str = "#7dcfff";

    /// User message background tint (#1a201e)
    pub const ROLE_USER_BG: &str = "#1a201e";

    /// Agent message background tint (#1a1c24)
    pub const ROLE_AGENT_BG: &str = "#1a1c24";

    /// Tool invocation background tint (#201c1a)
    pub const ROLE_TOOL_BG: &str = "#201c1a";

    /// System message background tint (#201e1a)
    pub const ROLE_SYSTEM_BG: &str = "#201e1a";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_colors_are_valid_hex() {
        // Verify all color constants are valid 7-char hex colors
        let all_colors = [
            // Backgrounds
            colors::BG_DEEP,
            colors::BG_SURFACE,
            colors::BG_HIGHLIGHT,
            // Borders
            colors::BORDER,
            colors::BORDER_FOCUS,
            // Text
            colors::TEXT_PRIMARY,
            colors::TEXT_SECONDARY,
            colors::TEXT_MUTED,
            colors::TEXT_DISABLED,
            // Accents
            colors::ACCENT_PRIMARY,
            colors::ACCENT_SECONDARY,
            colors::ACCENT_TERTIARY,
            // Roles
            colors::ROLE_USER,
            colors::ROLE_AGENT,
            colors::ROLE_TOOL,
            colors::ROLE_SYSTEM,
            // Role backgrounds
            colors::ROLE_USER_BG,
            colors::ROLE_AGENT_BG,
            colors::ROLE_TOOL_BG,
            colors::ROLE_SYSTEM_BG,
            // Status
            colors::STATUS_SUCCESS,
            colors::STATUS_WARNING,
            colors::STATUS_ERROR,
            colors::STATUS_INFO,
        ];

        for color in all_colors {
            assert!(
                color.starts_with('#') && color.len() == 7,
                "Invalid color format: {}",
                color
            );
            // Verify hex chars
            assert!(
                color[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "Invalid hex in color: {}",
                color
            );
        }
    }
}
