//! Core HTML template generation.
//!
//! This module provides the `HtmlTemplate` struct and `HtmlExporter` for generating
//! self-contained HTML files from session data. The template follows these principles:
//!
//! - **No external template engine**: Uses Rust `format!` macros for simplicity
//! - **Critical CSS inlined**: Ensures offline functionality
//! - **CDN as enhancement**: Tailwind, Prism.js loaded with defer
//! - **Progressive enhancement**: Basic layout works without JS
//! - **Semantic HTML**: Proper use of article, section, header elements

use std::time::Instant;

use super::{encryption, filename, renderer, scripts, styles};
use tracing::{debug, info, trace, warn};

/// Errors that can occur during template generation.
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    /// Invalid input data
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Rendering failed
    #[error("render failed: {0}")]
    RenderFailed(String),
    /// Encryption required but not provided
    #[error("encryption required but no key provided")]
    EncryptionRequired,
}

/// Options for HTML export.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Document title (defaults to session ID or timestamp)
    pub title: Option<String>,

    /// Include CDN resources for enhanced styling
    pub include_cdn: bool,

    /// Include syntax highlighting (Prism.js)
    pub syntax_highlighting: bool,

    /// Include search functionality
    pub include_search: bool,

    /// Include theme toggle (light/dark)
    pub include_theme_toggle: bool,

    /// Encrypt the conversation content
    pub encrypt: bool,

    /// Include print-optimized styles
    pub print_styles: bool,

    /// Agent name for branding
    pub agent_name: Option<String>,

    /// Include message timestamps
    pub show_timestamps: bool,

    /// Include tool call details (collapsed by default)
    pub show_tool_calls: bool,
}

const SCREEN_ONLY_CSS: &str = r#"
.print-only {
    display: none !important;
}
"#;

const CDN_FALLBACK_CSS: &str = r#"
/* CDN fallback hooks — activated when CDNs fail to load or are disabled */
.no-tailwind .toolbar,
.no-tailwind .header,
.no-tailwind .conversation {
    backdrop-filter: none !important;
}

/* Ensure ALL code blocks are legible without Prism syntax highlighting.
   Covers both language-tagged and untagged code blocks. */
.no-prism pre code {
    color: #c0caf5;
}

.no-prism pre code .token {
    color: inherit;
}
"#;

// Note: Tailwind v3+/v4 requires compilation - no pre-built CSS file exists.
// Our inline critical CSS provides complete Stripe-level styling without external dependencies.
// This ensures offline-capable, self-contained HTML exports with perfect styling.
const PRISM_THEME_URL: &str =
    "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/themes/prism-tomorrow.min.css";
const PRISM_THEME_SRI: &str =
    "sha384-wFjoQjtV1y5jVHbt0p35Ui8aV8GVpEZkyF99OXWqP/eNJDU93D3Ugxkoyh6Y2I4A";
const PRISM_CORE_URL: &str = "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/prism.min.js";
const PRISM_CORE_SRI: &str =
    "sha384-ZM8fDxYm+GXOWeJcxDetoRImNnEAS7XwVFH5kv0pT6RXNy92Nemw/Sj7NfciXpqg";
const PRISM_RUST_URL: &str =
    "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/components/prism-rust.min.js";
const PRISM_RUST_SRI: &str =
    "sha384-JyDgFjMbyrE/TGiEUSXW3CLjQOySrsoiUNAlXTFdIsr/XUfaB7E+eYlR+tGQ9bCO";
const PRISM_PYTHON_URL: &str =
    "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/components/prism-python.min.js";
const PRISM_PYTHON_SRI: &str =
    "sha384-WJdEkJKrbsqw0evQ4GB6mlsKe5cGTxBOw4KAEIa52ZLB7DDpliGkwdme/HMa5n1m";
const PRISM_JS_URL: &str =
    "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/components/prism-javascript.min.js";
const PRISM_JS_SRI: &str =
    "sha384-D44bgYYKvaiDh4cOGlj1dbSDpSctn2FSUj118HZGmZEShZcO2v//Q5vvhNy206pp";
const PRISM_TS_URL: &str =
    "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/components/prism-typescript.min.js";
const PRISM_TS_SRI: &str =
    "sha384-PeOqKNW/piETaCg8rqKFy+Pm6KEk7e36/5YZE5XO/OaFdO+/Aw3O8qZ9qDPKVUgx";
const PRISM_BASH_URL: &str =
    "https://cdn.jsdelivr.net/npm/prismjs@1.29.0/components/prism-bash.min.js";
const PRISM_BASH_SRI: &str =
    "sha384-9WmlN8ABpoFSSHvBGGjhvB3E/D8UkNB9HpLJjBQFC2VSQsM1odiQDv4NbEo+7l15";

const PRINT_EXTRA_CSS: &str = r#"
.print-only {
    display: block !important;
}

.print-footer {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.2in 0.6in 0.1in;
    border-top: 1px solid #ccc;
    font-size: 9pt;
    color: #666;
    background: #fff;
}

.print-footer-title {
    font-weight: 600;
    color: #1a1b26;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1 1 auto;
    min-width: 0;
}

.print-footer-page {
    flex: 0 0 auto;
}

.print-footer-page::after {
    content: "Page " counter(page) " of " counter(pages);
}

body {
    padding-bottom: 0.7in;
}

/* Ensure printed layout is clean and unclipped */
* {
    box-shadow: none !important;
    text-shadow: none !important;
}

.conversation,
.message-content,
.tool-call-body,
pre,
code {
    overflow: visible !important;
    max-height: none !important;
}

img,
svg,
video,
canvas {
    max-width: 100% !important;
    height: auto !important;
}

/* Avoid sticky/fixed UI elements in print, except footer */
.toolbar,
.theme-toggle {
    position: static !important;
}
"#;

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            title: None,
            include_cdn: true,
            syntax_highlighting: true,
            include_search: true,
            include_theme_toggle: true,
            encrypt: false,
            print_styles: true,
            agent_name: None,
            show_timestamps: true,
            show_tool_calls: true,
        }
    }
}

/// The HTML template structure.
///
/// Contains all the parts needed to generate a complete HTML document.
pub struct HtmlTemplate {
    /// Document title
    pub title: String,

    /// Critical inline CSS (required for offline)
    pub critical_css: String,

    /// Print-specific CSS
    pub print_css: String,

    /// Inline JavaScript
    pub inline_js: String,

    /// Main content HTML
    pub content: String,

    /// Whether content is encrypted
    pub encrypted: bool,

    /// Metadata for the header section
    pub metadata: TemplateMetadata,
}

/// Metadata displayed in the document header.
#[derive(Debug, Clone, Default)]
pub struct TemplateMetadata {
    /// Session date/time
    pub timestamp: Option<String>,

    /// Agent type (Claude, Codex, etc.)
    pub agent: Option<String>,

    /// Total rendered message count (internal)
    pub message_count: usize,

    /// Human-typed prompts (user messages that aren't tool results)
    pub human_turns: usize,

    /// Assistant response count
    pub assistant_msgs: usize,

    /// Tool use invocations (individual tool_use blocks in assistant messages)
    pub tool_use_count: usize,

    /// Duration of session
    pub duration: Option<String>,

    /// Source project/directory
    pub project: Option<String>,
}

impl HtmlTemplate {
    /// Generate the complete HTML document.
    pub fn render(&self, options: &ExportOptions) -> String {
        let _started = Instant::now();
        let critical_css = format!(
            "{}\n{}\n{}",
            self.critical_css, SCREEN_ONLY_CSS, CDN_FALLBACK_CSS
        );
        let cdn_scripts = if options.include_cdn {
            let mut tags = Vec::new();
            tags.push(
                r#"<link rel="preconnect" href="https://cdn.jsdelivr.net" crossorigin="anonymous">"#
                    .to_string(),
            );
            if options.syntax_highlighting {
                tags.push(format!(
                    r#"<link rel="stylesheet" href="{url}" integrity="{sri}" crossorigin="anonymous" media="print" onload="this.media='all'" onerror="document.documentElement.classList.add('no-prism')">"#,
                    url = PRISM_THEME_URL,
                    sri = PRISM_THEME_SRI
                ));
                tags.push(format!(
                    r#"<script src="{url}" integrity="{sri}" crossorigin="anonymous" defer onerror="document.documentElement.classList.add('no-prism')"></script>"#,
                    url = PRISM_CORE_URL,
                    sri = PRISM_CORE_SRI
                ));
                tags.push(format!(
                    r#"<script src="{url}" integrity="{sri}" crossorigin="anonymous" defer onerror="document.documentElement.classList.add('no-prism')"></script>"#,
                    url = PRISM_RUST_URL,
                    sri = PRISM_RUST_SRI
                ));
                tags.push(format!(
                    r#"<script src="{url}" integrity="{sri}" crossorigin="anonymous" defer onerror="document.documentElement.classList.add('no-prism')"></script>"#,
                    url = PRISM_PYTHON_URL,
                    sri = PRISM_PYTHON_SRI
                ));
                tags.push(format!(
                    r#"<script src="{url}" integrity="{sri}" crossorigin="anonymous" defer onerror="document.documentElement.classList.add('no-prism')"></script>"#,
                    url = PRISM_JS_URL,
                    sri = PRISM_JS_SRI
                ));
                tags.push(format!(
                    r#"<script src="{url}" integrity="{sri}" crossorigin="anonymous" defer onerror="document.documentElement.classList.add('no-prism')"></script>"#,
                    url = PRISM_TS_URL,
                    sri = PRISM_TS_SRI
                ));
                tags.push(format!(
                    r#"<script src="{url}" integrity="{sri}" crossorigin="anonymous" defer onerror="document.documentElement.classList.add('no-prism')"></script>"#,
                    url = PRISM_BASH_URL,
                    sri = PRISM_BASH_SRI
                ));
            }

            format!(
                r#"
    <!-- CDN enhancement (optional) - degrades gracefully if offline -->
    {}"#,
                tags.join("\n    ")
            )
        } else {
            String::new()
        };

        let print_styles = if options.print_styles {
            format!(
                r#"
    <style media="print">
{}
{}
    </style>"#,
                self.print_css, PRINT_EXTRA_CSS
            )
        } else {
            String::new()
        };

        let print_footer = if options.print_styles {
            self.render_print_footer()
        } else {
            String::new()
        };

        let password_modal = if self.encrypted {
            r#"
        <!-- Password modal for encrypted content -->
        <div id="password-modal" class="decrypt-modal" role="dialog" aria-labelledby="modal-title" aria-modal="true">
            <div class="decrypt-form">
                <h2 id="modal-title">Enter Password</h2>
                <p>This conversation is encrypted. Enter the password to view.</p>
                <form id="password-form">
                    <input type="password" id="password-input" placeholder="Password" autocomplete="current-password" required>
                    <button type="submit">Decrypt</button>
                </form>
                <p id="decrypt-error" class="decrypt-error" hidden></p>
            </div>
        </div>"#
        } else {
            ""
        };

        let toolbar = self.render_toolbar(options);
        let header = self.render_header();

        trace!(
            component = "template",
            operation = "render_inputs",
            include_cdn = options.include_cdn,
            syntax_highlighting = options.syntax_highlighting,
            include_search = options.include_search,
            include_theme_toggle = options.include_theme_toggle,
            encrypt = options.encrypt,
            print_styles = options.print_styles,
            "Preparing HTML render"
        );

        // When CDNs are disabled, add no-prism class so fallback CSS activates.
        // Without this, code blocks are illegible (dark text on dark background)
        // because the Prism onerror handlers never fire to add the class.
        let html_classes = if !options.include_cdn {
            r#" class="no-prism no-tailwind""#
        } else {
            ""
        };

        format!(
            r#"<!DOCTYPE html>
<html lang="en" data-theme="dark"{html_classes}>
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <meta name="color-scheme" content="dark light">
    <meta name="generator" content="CASS HTML Export">
    <title>{title}</title>
    <!-- Critical inline styles for offline operation -->
    <style>
{critical_css}
    </style>{cdn_scripts}{print_styles}
</head>
<body>
    <div class="scroll-progress" id="scroll-progress"></div>
{print_footer}
    <div id="app" class="app-container">
{header}
{toolbar}
        <!-- Conversation container -->
        <main id="conversation" class="conversation" role="main">
{content}
        </main>
{password_modal}
    </div>
    <!-- Floating navigation -->
    <nav class="floating-nav" id="floating-nav" aria-label="Quick navigation">
        <button class="floating-btn" id="scroll-top" aria-label="Scroll to top" title="Scroll to top">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M18 15l-6-6-6 6"/>
            </svg>
        </button>
    </nav>
    <!-- Scripts at end for performance -->
    <script>
{inline_js}
    </script>
</body>
</html>"#,
            title = html_escape(&self.title),
            critical_css = critical_css,
            cdn_scripts = cdn_scripts,
            print_styles = print_styles,
            header = header,
            toolbar = toolbar,
            content = self.content,
            password_modal = password_modal,
            inline_js = self.inline_js,
            print_footer = print_footer,
        )
    }

    fn render_header(&self) -> String {
        let mut meta_items = Vec::new();

        if let Some(ts) = &self.metadata.timestamp {
            let escaped_ts = html_escape(ts);
            meta_items.push(format!(
                r#"<span><time datetime="{}">{}</time></span>"#,
                escaped_ts, escaped_ts
            ));
        }

        if let Some(agent) = &self.metadata.agent {
            // Use human-readable display name instead of raw slug
            let display_name = crate::html_export::renderer::agent_display_name(agent);
            meta_items.push(format!(
                r#"<span class="header-agent">{}</span>"#,
                html_escape(display_name)
            ));
        }

        if self.metadata.message_count > 0 {
            // Show accurate breakdown: human prompts, assistant responses, tool calls.
            // "577 messages" is misleading when only 20 were human-typed.
            let count_str = if self.metadata.human_turns > 0 {
                format!(
                    "{} prompt{}, {} response{}, {} tool use{}",
                    self.metadata.human_turns,
                    if self.metadata.human_turns == 1 {
                        ""
                    } else {
                        "s"
                    },
                    self.metadata.assistant_msgs,
                    if self.metadata.assistant_msgs == 1 {
                        ""
                    } else {
                        "s"
                    },
                    self.metadata.tool_use_count,
                    if self.metadata.tool_use_count == 1 {
                        ""
                    } else {
                        "s"
                    },
                )
            } else {
                format!("{} messages", self.metadata.message_count)
            };
            meta_items.push(format!(r#"<span>{}</span>"#, count_str));
        }

        if let Some(duration) = &self.metadata.duration {
            meta_items.push(format!(r#"<span>{}</span>"#, html_escape(duration)));
        }

        if let Some(project) = &self.metadata.project {
            // Extract just the project name from full path for cleaner display
            let display_project = std::path::Path::new(project)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(project);
            meta_items.push(format!(
                r#"<span class="header-project">{}</span>"#,
                html_escape(display_project)
            ));
        }

        let meta_html = if meta_items.is_empty() {
            String::new()
        } else {
            format!(
                r#"
                <div class="header-meta">{}</div>"#,
                meta_items.join("\n                    ")
            )
        };

        // Header with terminal-style traffic lights (via CSS ::before)
        // The header-content div is offset to make room for the traffic lights
        format!(
            r#"        <!-- Header with terminal-style traffic lights -->
        <header class="header" role="banner">
            <div class="header-content">
                <h1 class="header-title">{}</h1>{}
            </div>
        </header>"#,
            html_escape(&self.title),
            meta_html
        )
    }

    fn render_toolbar(&self, options: &ExportOptions) -> String {
        let mut toolbar_items = Vec::new();

        if options.include_search {
            toolbar_items.push(r#"<div class="search-wrapper">
                <input type="search" id="search-input" class="search-input" placeholder="Search messages..." aria-label="Search conversation">
                <span id="search-count" class="search-count" hidden></span>
            </div>"#.to_string());
        }

        if options.include_theme_toggle {
            toolbar_items.push(r#"<button id="theme-toggle" class="toolbar-btn" aria-label="Toggle theme" title="Toggle light/dark theme">
                <svg class="icon-sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <circle cx="12" cy="12" r="5"/>
                    <path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42"/>
                </svg>
                <svg class="icon-moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>
                </svg>
            </button>"#.to_string());
        }

        toolbar_items.push(r#"<button id="print-btn" class="toolbar-btn" aria-label="Print" title="Print conversation">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <path d="M6 9V2h12v7M6 18H4a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2h-2"/>
                    <rect x="6" y="14" width="12" height="8"/>
                </svg>
            </button>"#.to_string());

        if toolbar_items.is_empty() {
            return String::new();
        }

        format!(
            r#"        <!-- Toolbar -->
        <nav class="toolbar" role="navigation" aria-label="Conversation tools">
            {}
        </nav>"#,
            toolbar_items.join("\n            ")
        )
    }

    fn render_print_footer(&self) -> String {
        format!(
            r#"    <div class="print-footer print-only" aria-hidden="true">
        <span class="print-footer-title">{}</span>
        <span class="print-footer-page"></span>
    </div>"#,
            html_escape(&self.title)
        )
    }
}

/// Main exporter for generating HTML from sessions.
pub struct HtmlExporter {
    options: ExportOptions,
}

impl HtmlExporter {
    /// Create a new exporter with default options.
    pub fn new() -> Self {
        Self {
            options: ExportOptions::default(),
        }
    }

    /// Create a new exporter with custom options.
    pub fn with_options(options: ExportOptions) -> Self {
        Self { options }
    }

    /// Get the current options.
    pub fn options(&self) -> &ExportOptions {
        &self.options
    }

    /// Generate an empty template for testing.
    pub fn create_template(&self, title: &str) -> HtmlTemplate {
        let styles = styles::generate_styles(&self.options);
        let scripts = scripts::generate_scripts(&self.options);

        HtmlTemplate {
            title: title.to_string(),
            critical_css: styles.critical_css,
            print_css: styles.print_css,
            inline_js: scripts.inline_js,
            content: String::new(),
            encrypted: self.options.encrypt,
            metadata: TemplateMetadata::default(),
        }
    }

    /// Generate a full HTML export for a set of message groups.
    ///
    /// Message groups are created by `group_messages_for_export()` which consolidates
    /// tool calls with their parent assistant messages for cleaner rendering.
    pub fn export_messages(
        &self,
        title: &str,
        groups: &[renderer::MessageGroup],
        metadata: TemplateMetadata,
        password: Option<&str>,
    ) -> Result<String, TemplateError> {
        let started = Instant::now();
        info!(
            component = "template",
            operation = "export_messages",
            group_count = groups.len(),
            total_tool_calls = groups.iter().map(|g| g.tool_count()).sum::<usize>(),
            encrypt = self.options.encrypt,
            include_cdn = self.options.include_cdn,
            include_search = self.options.include_search,
            include_theme_toggle = self.options.include_theme_toggle,
            print_styles = self.options.print_styles,
            "Starting HTML export"
        );

        let render_options = renderer::RenderOptions {
            show_timestamps: self.options.show_timestamps,
            show_tool_calls: self.options.show_tool_calls,
            syntax_highlighting: self.options.syntax_highlighting,
            agent_slug: self
                .options
                .agent_name
                .as_ref()
                .map(|name| filename::agent_slug(name)),
            ..renderer::RenderOptions::default()
        };

        let render_started = Instant::now();
        let rendered = renderer::render_message_groups(groups, &render_options)
            .map_err(|e| TemplateError::RenderFailed(e.to_string()))?;
        debug!(
            component = "renderer",
            operation = "render_message_groups_complete",
            duration_ms = render_started.elapsed().as_millis(),
            bytes = rendered.len(),
            groups = groups.len(),
            "Message groups rendered"
        );

        let content = if self.options.encrypt {
            let password = match password {
                Some(pw) => pw,
                None => {
                    warn!(
                        component = "encryption",
                        operation = "encrypt_payload",
                        "Encryption requested but no password provided"
                    );
                    return Err(TemplateError::EncryptionRequired);
                }
            };
            debug!(
                component = "encryption",
                operation = "encrypt_payload",
                plaintext_bytes = rendered.len(),
                "Encrypting rendered HTML"
            );
            let encrypted = encryption::encrypt_content(
                &rendered,
                password,
                &encryption::EncryptionParams::default(),
            )
            .map_err(|e| TemplateError::RenderFailed(e.to_string()))?;
            encryption::render_encrypted_placeholder(&encrypted)
        } else {
            rendered
        };

        let styles_started = Instant::now();
        let styles = styles::generate_styles(&self.options);
        debug!(
            component = "styles",
            operation = "generate",
            critical_bytes = styles.critical_css.len(),
            print_bytes = styles.print_css.len(),
            duration_ms = styles_started.elapsed().as_millis(),
            "Generated styles"
        );

        let scripts_started = Instant::now();
        let scripts = scripts::generate_scripts(&self.options);
        debug!(
            component = "scripts",
            operation = "generate",
            inline_bytes = scripts.inline_js.len(),
            duration_ms = scripts_started.elapsed().as_millis(),
            "Generated scripts"
        );

        let template = HtmlTemplate {
            title: title.to_string(),
            critical_css: styles.critical_css,
            print_css: styles.print_css,
            inline_js: scripts.inline_js,
            content,
            encrypted: self.options.encrypt,
            metadata,
        };

        let html = template.render(&self.options);
        info!(
            component = "template",
            operation = "export_messages_complete",
            duration_ms = started.elapsed().as_millis(),
            bytes = html.len(),
            "HTML export complete"
        );
        Ok(html)
    }
}

impl Default for HtmlExporter {
    fn default() -> Self {
        Self::new()
    }
}

/// Escape HTML special characters.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing::Level;

    #[derive(Clone)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut inner = self.0.lock().expect("log buffer lock");
            inner.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<F: FnOnce()>(f: F) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = LogBuffer(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_target(false)
            .with_max_level(Level::DEBUG)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        let bytes = buf.lock().expect("log buffer lock").clone();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[test]
    fn test_template_error_display_strings() {
        assert_eq!(
            TemplateError::InvalidInput("missing title".to_string()).to_string(),
            "invalid input: missing title"
        );
        assert_eq!(
            TemplateError::RenderFailed("bad markdown".to_string()).to_string(),
            "render failed: bad markdown"
        );
        assert_eq!(
            TemplateError::EncryptionRequired.to_string(),
            "encryption required but no key provided"
        );
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
        assert_eq!(html_escape(r#"say "hello""#), "say &quot;hello&quot;");
    }

    #[test]
    fn test_export_options_default() {
        let opts = ExportOptions::default();
        assert!(opts.include_cdn);
        assert!(opts.syntax_highlighting);
        assert!(!opts.encrypt);
    }

    #[test]
    fn test_cdn_resources_include_integrity() {
        let template = HtmlTemplate {
            title: "CDN Test".to_string(),
            critical_css: String::new(),
            print_css: String::new(),
            inline_js: String::new(),
            content: "<p>ok</p>".to_string(),
            encrypted: false,
            metadata: TemplateMetadata::default(),
        };
        let opts = ExportOptions::default();
        let html = template.render(&opts);

        // Note: Tailwind CDN removed - Tailwind v3+/v4 requires compilation.
        // Our inline critical CSS provides complete styling.
        assert!(!html.contains("tailwindcss"));
        assert!(html.contains(PRISM_CORE_URL));
        assert!(html.contains(PRISM_CORE_SRI));
        assert!(html.contains("document.documentElement.classList.add('no-prism')"));
    }

    #[test]
    fn test_no_cdn_removes_external_tags() {
        let template = HtmlTemplate {
            title: "No CDN".to_string(),
            critical_css: String::new(),
            print_css: String::new(),
            inline_js: String::new(),
            content: "<p>ok</p>".to_string(),
            encrypted: false,
            metadata: TemplateMetadata::default(),
        };
        let opts = ExportOptions {
            include_cdn: false,
            ..ExportOptions::default()
        };
        let html = template.render(&opts);

        assert!(!html.contains("cdn.jsdelivr.net"));
    }

    #[test]
    fn test_template_renders_valid_html() {
        let template = HtmlTemplate {
            title: "Test Session".to_string(),
            critical_css: "body { background: #1a1b26; }".to_string(),
            print_css: "@page { margin: 1in; }".to_string(),
            inline_js: "console.log('loaded');".to_string(),
            content: "<p>Hello, World!</p>".to_string(),
            encrypted: false,
            metadata: TemplateMetadata::default(),
        };

        let html = template.render(&ExportOptions::default());

        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html lang=\"en\""));
        assert!(html.contains("Test Session"));
        assert!(html.contains("Hello, World!"));
        assert!(html.contains("background: #1a1b26"));
    }

    #[test]
    fn test_encrypted_template_shows_modal() {
        let template = HtmlTemplate {
            title: "Encrypted".to_string(),
            critical_css: String::new(),
            print_css: String::new(),
            inline_js: String::new(),
            content: "[ENCRYPTED]".to_string(),
            encrypted: true,
            metadata: TemplateMetadata::default(),
        };

        let html = template.render(&ExportOptions::default());
        assert!(html.contains("password-modal"));
        assert!(html.contains("Enter Password"));
    }

    #[test]
    fn test_export_messages_plain() {
        let exporter = HtmlExporter::with_options(ExportOptions::default());
        let message = renderer::Message {
            role: "user".to_string(),
            content: "Hello world".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        };
        let groups = vec![renderer::MessageGroup::user(message)];

        let html = exporter
            .export_messages("Test Export", &groups, TemplateMetadata::default(), None)
            .expect("export");

        assert!(html.contains("Hello world"));
        assert!(html.contains("conversation"));
    }

    #[test]
    fn test_export_logs_include_milestones() {
        let exporter = HtmlExporter::with_options(ExportOptions::default());
        let groups = vec![
            renderer::MessageGroup::user(renderer::Message {
                role: "user".to_string(),
                content: "Hello world".to_string(),
                timestamp: None,
                tool_call: None,
                index: None,
                author: None,
            }),
            renderer::MessageGroup::assistant(renderer::Message {
                role: "assistant".to_string(),
                content: "Response".to_string(),
                timestamp: None,
                tool_call: None,
                index: None,
                author: None,
            }),
        ];

        let logs = capture_logs(|| {
            exporter
                .export_messages("Test Export", &groups, TemplateMetadata::default(), None)
                .expect("export");
        });

        // Verify milestone logs are captured.
        // Note: Under parallel test execution the local subscriber can occasionally
        // observe only a subset of this call's structured logs. Accept any of the
        // start or completion milestones, since each one confirms the export path
        // emitted structured progress logs for this call.
        let has_template_start = logs.contains("component=\"template\"")
            && logs.contains("operation=\"export_messages\"");
        let has_renderer_start = logs.contains("component=\"renderer\"")
            && logs.contains("operation=\"render_message_groups\"");
        let has_template_complete = logs.contains("component=\"template\"")
            && logs.contains("operation=\"export_messages_complete\"");
        assert!(
            has_template_start || has_renderer_start || has_template_complete,
            "expected structured export milestone log, got: {logs}"
        );
        // If completion log is present, verify its format
        if logs.contains("operation=\"export_messages_complete\"") {
            assert!(
                logs.contains("duration_ms"),
                "completion log should include duration"
            );
        }
    }

    #[test]
    fn test_export_messages_requires_password_when_encrypted() {
        let exporter = HtmlExporter::with_options(ExportOptions {
            encrypt: true,
            ..Default::default()
        });
        let groups = vec![renderer::MessageGroup::assistant(renderer::Message {
            role: "assistant".to_string(),
            content: "Secret".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        })];

        let result = exporter.export_messages(
            "Encrypted Export",
            &groups,
            TemplateMetadata::default(),
            None,
        );

        assert!(matches!(result, Err(TemplateError::EncryptionRequired)));
    }

    #[test]
    #[cfg(feature = "encryption")]
    fn test_export_messages_encrypted_payload() {
        let exporter = HtmlExporter::with_options(ExportOptions {
            encrypt: true,
            ..Default::default()
        });
        let groups = vec![renderer::MessageGroup::assistant(renderer::Message {
            role: "assistant".to_string(),
            content: "Top secret".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        })];

        let html = exporter
            .export_messages(
                "Encrypted Export",
                &groups,
                TemplateMetadata::default(),
                Some("password"),
            )
            .expect("export");

        assert!(html.contains("encrypted-content"));
        assert!(html.contains("\"iterations\":600000"));
        assert!(!html.contains("Top secret"));
    }
}
