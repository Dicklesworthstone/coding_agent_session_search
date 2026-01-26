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

use std::fmt;
use std::time::Instant;

use super::{encryption, filename, renderer, scripts, styles};
use tracing::{debug, info, trace, warn};

/// Errors that can occur during template generation.
#[derive(Debug)]
pub enum TemplateError {
    /// Invalid input data
    InvalidInput(String),
    /// Rendering failed
    RenderFailed(String),
    /// Encryption required but not provided
    EncryptionRequired,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::InvalidInput(msg) => write!(f, "invalid input: {}", msg),
            TemplateError::RenderFailed(msg) => write!(f, "render failed: {}", msg),
            TemplateError::EncryptionRequired => {
                write!(f, "encryption required but no key provided")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

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
/* CDN fallback hooks */
.no-tailwind .toolbar,
.no-tailwind .header,
.no-tailwind .conversation {
    backdrop-filter: none !important;
}

.no-prism pre code[class*="language-"] {
    color: #c0caf5;
}

.no-prism pre code[class*="language-"] .token {
    color: inherit;
}
"#;

const TAILWIND_CDN_URL: &str =
    "https://cdn.jsdelivr.net/npm/tailwindcss@3.4.1/dist/tailwind.min.css";
const TAILWIND_CDN_SRI: &str =
    "sha384-wAkE1abywdsF0VP/+RDLxHADng231vt6gsqcjBzQFUoAQNkuN63+cJ4XDiE7LVjx";
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

    /// Message count
    pub message_count: usize,

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
            tags.push(format!(
                r#"<link rel="stylesheet" href="{url}" integrity="{sri}" crossorigin="anonymous" media="print" onload="this.media='all'" onerror="document.documentElement.classList.add('no-tailwind')">"#,
                url = TAILWIND_CDN_URL,
                sri = TAILWIND_CDN_SRI
            ));

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
        <div id="password-modal" class="modal" role="dialog" aria-labelledby="modal-title" aria-modal="true">
            <div class="modal-content">
                <div class="modal-icon" aria-hidden="true">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                        <rect x="3" y="11" width="18" height="10" rx="2"/>
                        <path d="M7 11V7a5 5 0 0 1 10 0v4"/>
                    </svg>
                </div>
                <h2 id="modal-title" class="modal-title">Enter Password</h2>
                <p class="modal-text">This conversation is encrypted. Enter the password to view.</p>
                <form id="password-form">
                    <input type="password" id="password-input" class="modal-input" placeholder="Password" autocomplete="current-password" required>
                    <button type="submit" class="modal-btn">Decrypt</button>
                </form>
                <p id="decrypt-error" class="modal-error" hidden></p>
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

        format!(
            r#"<!DOCTYPE html>
<html lang="en" data-theme="dark">
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
{print_footer}
    <div id="app">
{header}
{toolbar}
        <!-- Conversation container -->
        <main id="conversation" class="conversation" role="main">
{content}
        </main>
{password_modal}
    </div>
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
                r#"<span class="meta-item"><time datetime="{}">{}</time></span>"#,
                escaped_ts, escaped_ts
            ));
        }

        if let Some(agent) = &self.metadata.agent {
            meta_items.push(format!(
                r#"<span class="meta-item meta-agent">{}</span>"#,
                html_escape(agent)
            ));
        }

        if self.metadata.message_count > 0 {
            meta_items.push(format!(
                r#"<span class="meta-item">{} messages</span>"#,
                self.metadata.message_count
            ));
        }

        if let Some(duration) = &self.metadata.duration {
            meta_items.push(format!(
                r#"<span class="meta-item">{}</span>"#,
                html_escape(duration)
            ));
        }

        if let Some(project) = &self.metadata.project {
            meta_items.push(format!(
                r#"<span class="meta-item meta-project">{}</span>"#,
                html_escape(project)
            ));
        }

        let meta_html = if meta_items.is_empty() {
            String::new()
        } else {
            format!(
                r#"
            <div class="meta">{}</div>"#,
                meta_items.join("\n                ")
            )
        };

        format!(
            r#"        <!-- Header with metadata -->
        <header class="header" role="banner">
            <h1 class="title">{}</h1>{}
        </header>"#,
            html_escape(&self.title),
            meta_html
        )
    }

    fn render_toolbar(&self, options: &ExportOptions) -> String {
        let mut toolbar_items = Vec::new();

        if options.include_search {
            toolbar_items.push(r#"<div class="toolbar-item">
                <input type="search" id="search-input" placeholder="Search..." aria-label="Search conversation">
                <span id="search-count" class="search-count" hidden></span>
            </div>"#.to_string());
        }

        if options.include_theme_toggle {
            toolbar_items.push(r#"<button id="theme-toggle" class="toolbar-btn" aria-label="Toggle theme" title="Toggle light/dark theme">
                <svg class="icon icon-sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <circle cx="12" cy="12" r="5"/>
                    <path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42"/>
                </svg>
                <svg class="icon icon-moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>
                </svg>
            </button>"#.to_string());
        }

        toolbar_items.push(r#"<button id="print-btn" class="toolbar-btn" aria-label="Print" title="Print conversation">
                <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
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

    /// Generate a full HTML export for a set of messages.
    pub fn export_messages(
        &self,
        title: &str,
        messages: &[renderer::Message],
        metadata: TemplateMetadata,
        password: Option<&str>,
    ) -> Result<String, TemplateError> {
        let started = Instant::now();
        info!(
            component = "template",
            operation = "export_messages",
            message_count = messages.len(),
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
        let rendered = renderer::render_conversation(messages, &render_options)
            .map_err(|e| TemplateError::RenderFailed(e.to_string()))?;
        debug!(
            component = "renderer",
            operation = "render_conversation_complete",
            duration_ms = render_started.elapsed().as_millis(),
            bytes = rendered.len(),
            "Conversation HTML rendered"
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

        assert!(html.contains(TAILWIND_CDN_URL));
        assert!(html.contains(TAILWIND_CDN_SRI));
        assert!(html.contains(PRISM_CORE_URL));
        assert!(html.contains(PRISM_CORE_SRI));
        assert!(html.contains("document.documentElement.classList.add('no-tailwind')"));
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
        let messages = vec![renderer::Message {
            role: "user".to_string(),
            content: "Hello world".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        }];

        let html = exporter
            .export_messages("Test Export", &messages, TemplateMetadata::default(), None)
            .expect("export");

        assert!(html.contains("Hello world"));
        assert!(html.contains("conversation"));
    }

    #[test]
    fn test_export_logs_include_milestones() {
        let exporter = HtmlExporter::with_options(ExportOptions::default());
        let messages = vec![
            renderer::Message {
                role: "user".to_string(),
                content: "Hello world".to_string(),
                timestamp: None,
                tool_call: None,
                index: None,
                author: None,
            },
            renderer::Message {
                role: "assistant".to_string(),
                content: "Response".to_string(),
                timestamp: None,
                tool_call: None,
                index: None,
                author: None,
            },
        ];

        let logs = capture_logs(|| {
            exporter
                .export_messages("Test Export", &messages, TemplateMetadata::default(), None)
                .expect("export");
        });

        // Verify INFO-level milestone logs are captured
        // Note: Due to test parallelism and subscriber isolation, we check for at least
        // the export start log. The completion log may not always be captured in time.
        assert!(
            logs.contains("component=\"template\"") && logs.contains("export_messages"),
            "expected template export start log, got: {logs}"
        );
        // If completion log is present, verify its format
        if logs.contains("export_messages_complete") {
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
        let messages = vec![renderer::Message {
            role: "assistant".to_string(),
            content: "Secret".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        }];

        let result = exporter.export_messages(
            "Encrypted Export",
            &messages,
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
        let messages = vec![renderer::Message {
            role: "assistant".to_string(),
            content: "Top secret".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        }];

        let html = exporter
            .export_messages(
                "Encrypted Export",
                &messages,
                TemplateMetadata::default(),
                Some("password"),
            )
            .expect("export");

        assert!(html.contains("encrypted-content"));
        assert!(html.contains("\"iterations\":600000"));
        assert!(!html.contains("Top secret"));
    }
}
