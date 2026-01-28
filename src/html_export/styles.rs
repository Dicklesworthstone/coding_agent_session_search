//! CSS generation for HTML export.
//!
//! Terminal Noir design system - matching the reference implementation exactly.

use super::template::ExportOptions;
use tracing::debug;

/// Bundle of CSS styles for the template.
pub struct StyleBundle {
    /// Critical CSS inlined in the document
    pub critical_css: String,

    /// Print-specific CSS
    pub print_css: String,
}

/// Generate all CSS styles for the template.
pub fn generate_styles(options: &ExportOptions) -> StyleBundle {
    let critical_css = generate_critical_css(options);
    let print_css = generate_print_css();
    debug!(
        component = "styles",
        operation = "generate",
        critical_bytes = critical_css.len(),
        print_bytes = print_css.len(),
        "Generated CSS styles"
    );
    StyleBundle {
        critical_css,
        print_css,
    }
}

fn generate_critical_css(options: &ExportOptions) -> String {
    let search_styles = if options.include_search {
        SEARCH_STYLES
    } else {
        ""
    };

    let encryption_styles = if options.encrypt {
        ENCRYPTION_STYLES
    } else {
        ""
    };

    format!(
        "{}\n{}\n{}\n{}",
        CORE_STYLES, COMPONENT_STYLES, search_styles, encryption_styles
    )
}

/// Core design system - Terminal Noir (exact match to reference)
const CORE_STYLES: &str = r#"
/* ============================================
   Agent Flywheel Design System - Terminal Noir
   Exact match to globals.css reference
   ============================================ */

:root {
  --radius: 0.75rem;

  /* Deep space palette - from reference */
  --background: oklch(0.11 0.015 260);
  --foreground: oklch(0.95 0.01 260);

  /* Cards with subtle elevation */
  --card: oklch(0.14 0.02 260);
  --card-foreground: oklch(0.95 0.01 260);

  --popover: oklch(0.13 0.02 260);
  --popover-foreground: oklch(0.95 0.01 260);

  /* Electric cyan primary */
  --primary: oklch(0.75 0.18 195);
  --primary-foreground: oklch(0.13 0.02 260);

  /* Muted backgrounds */
  --secondary: oklch(0.18 0.02 260);
  --secondary-foreground: oklch(0.85 0.01 260);

  --muted: oklch(0.16 0.015 260);
  --muted-foreground: oklch(0.6 0.02 260);

  /* Warm amber accent */
  --accent: oklch(0.78 0.16 75);
  --accent-foreground: oklch(0.13 0.02 260);

  /* Destructive red */
  --destructive: oklch(0.65 0.22 25);

  /* Borders and inputs */
  --border: oklch(0.25 0.02 260);
  --input: oklch(0.2 0.02 260);
  --ring: oklch(0.75 0.18 195);

  /* Custom accent colors */
  --cyan: oklch(0.75 0.18 195);
  --amber: oklch(0.78 0.16 75);
  --magenta: oklch(0.7 0.2 330);
  --green: oklch(0.72 0.19 145);
  --purple: oklch(0.65 0.18 290);
  --red: oklch(0.65 0.22 25);

  /* Typography Scale - Fluid */
  --text-xs: clamp(0.6875rem, 0.65rem + 0.15vw, 0.75rem);
  --text-sm: clamp(0.8125rem, 0.775rem + 0.2vw, 0.875rem);
  --text-base: clamp(1rem, 0.95rem + 0.25vw, 1.125rem);
  --text-lg: clamp(1.125rem, 1.05rem + 0.4vw, 1.375rem);
  --text-xl: clamp(1.375rem, 1.25rem + 0.65vw, 1.75rem);
  --text-2xl: clamp(1.625rem, 1.45rem + 0.9vw, 2.25rem);

  /* Spacing System */
  --space-1: 0.25rem;
  --space-2: 0.5rem;
  --space-3: 0.75rem;
  --space-4: 1rem;
  --space-5: 1.25rem;
  --space-6: 1.5rem;
  --space-8: 2rem;
  --space-10: 2.5rem;
  --space-12: 3rem;
  --space-16: 4rem;

  /* Enhanced Shadow System - from reference */
  --shadow-xs: 0 1px 2px oklch(0 0 0 / 0.08);
  --shadow-sm: 0 2px 4px oklch(0 0 0 / 0.08), 0 1px 2px oklch(0 0 0 / 0.06);
  --shadow-md: 0 4px 8px oklch(0 0 0 / 0.1), 0 2px 4px oklch(0 0 0 / 0.06);
  --shadow-lg: 0 8px 24px oklch(0 0 0 / 0.12), 0 4px 8px oklch(0 0 0 / 0.06);
  --shadow-xl: 0 16px 48px oklch(0 0 0 / 0.16), 0 8px 16px oklch(0 0 0 / 0.08);

  /* Colored glow shadows - from reference */
  --shadow-glow-sm: 0 0 12px oklch(0.75 0.18 195 / 0.2);
  --shadow-glow: 0 0 24px oklch(0.75 0.18 195 / 0.25), 0 0 48px oklch(0.75 0.18 195 / 0.1);
  --shadow-glow-primary: 0 4px 20px oklch(0.75 0.18 195 / 0.35), 0 0 0 1px oklch(0.75 0.18 195 / 0.15);
  --shadow-glow-amber: 0 4px 20px oklch(0.78 0.16 75 / 0.3), 0 0 0 1px oklch(0.78 0.16 75 / 0.15);

  /* Radius system */
  --radius-sm: calc(var(--radius) - 4px);
  --radius-md: calc(var(--radius) - 2px);
  --radius-lg: var(--radius);
  --radius-xl: calc(var(--radius) + 4px);

  /* Transitions */
  --transition-fast: 150ms cubic-bezier(0.4, 0, 0.2, 1);
  --transition-normal: 250ms cubic-bezier(0.4, 0, 0.2, 1);

  /* Touch targets */
  --touch-min: 44px;
}

/* Light mode - from reference */
[data-theme="light"] {
  --background: oklch(0.98 0.005 260);
  --foreground: oklch(0.15 0.02 260);
  --card: oklch(1 0 0);
  --card-foreground: oklch(0.15 0.02 260);
  --popover: oklch(1 0 0);
  --popover-foreground: oklch(0.15 0.02 260);
  --primary: oklch(0.55 0.2 195);
  --primary-foreground: oklch(1 0 0);
  --secondary: oklch(0.94 0.01 260);
  --secondary-foreground: oklch(0.2 0.02 260);
  --muted: oklch(0.94 0.01 260);
  --muted-foreground: oklch(0.45 0.02 260);
  --accent: oklch(0.65 0.18 75);
  --accent-foreground: oklch(0.15 0.02 260);
  --destructive: oklch(0.55 0.25 25);
  --border: oklch(0.9 0.01 260);
  --input: oklch(0.92 0.01 260);
  --ring: oklch(0.55 0.2 195);

  --cyan: oklch(0.55 0.2 195);
  --green: oklch(0.5 0.18 145);
  --amber: oklch(0.6 0.18 75);
}

/* Base reset */
*, *::before, *::after {
  box-sizing: border-box;
  margin: 0;
  padding: 0;
}

html {
  overflow-x: hidden;
  scroll-behavior: smooth;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
}

body {
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
  font-size: var(--text-base);
  line-height: 1.65;
  color: #e8e9ed;
  color: var(--foreground);
  /* Solid dark background - hex fallback first, then oklch if supported */
  background-color: #16161f;
  min-height: 100vh;
  min-height: 100dvh;
  overflow-x: hidden;
  max-width: 100vw;
}

/* Override background with oklch for modern browsers */
@supports (background: oklch(0.11 0.015 260)) {
  body {
    background-color: oklch(0.11 0.015 260);
  }
}

/* Hero background overlay - subtle ambient glow */
body::before {
  content: '';
  position: fixed;
  inset: 0;
  pointer-events: none;
  z-index: -1;
  background:
    radial-gradient(ellipse at 30% 20%, rgba(70, 180, 220, 0.12) 0%, transparent 40%),
    radial-gradient(ellipse at 70% 80%, rgba(200, 100, 180, 0.08) 0%, transparent 40%),
    radial-gradient(ellipse at 90% 30%, rgba(220, 180, 80, 0.06) 0%, transparent 30%);
}

/* Custom scrollbar - from reference */
::-webkit-scrollbar {
  width: 8px;
  height: 8px;
}
::-webkit-scrollbar-track {
  background: oklch(0.14 0.02 260);
}
::-webkit-scrollbar-thumb {
  background: oklch(0.3 0.02 260);
  border-radius: 4px;
}
::-webkit-scrollbar-thumb:hover {
  background: oklch(0.4 0.02 260);
}

/* Firefox scrollbar */
* {
  scrollbar-width: thin;
  scrollbar-color: oklch(0.3 0.02 260) oklch(0.14 0.02 260);
}

/* ============================================
   Layout - Full Width Utilization
   ============================================ */

.app-container {
  width: 100%;
  max-width: 100%;
  margin: 0 auto;
  padding: var(--space-4);
  padding-bottom: calc(var(--space-8) + env(safe-area-inset-bottom, 0px));
}

@media (min-width: 768px) {
  .app-container {
    padding: var(--space-6) var(--space-8);
  }
}

@media (min-width: 1024px) {
  .app-container {
    padding: var(--space-8) var(--space-12);
    max-width: calc(100% - 80px);
  }
}

@media (min-width: 1280px) {
  .app-container {
    max-width: calc(100% - 160px);
    padding: var(--space-8) var(--space-16);
  }
}

@media (min-width: 1536px) {
  .app-container {
    max-width: 1400px;
  }
}

/* ============================================
   Glass morphism - exact match to reference
   ============================================ */

.glass {
  background: oklch(0.14 0.02 260 / 0.8);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
  border: 1px solid oklch(0.3 0.02 260 / 0.3);
}

.glass-subtle {
  background: oklch(0.14 0.02 260 / 0.6);
  backdrop-filter: blur(8px);
  -webkit-backdrop-filter: blur(8px);
}

/* ============================================
   Typography
   ============================================ */

h1, h2, h3, h4, h5, h6 {
  font-weight: 600;
  line-height: 1.3;
  color: var(--foreground);
  letter-spacing: -0.02em;
}

h1 { font-size: var(--text-2xl); }
h2 { font-size: var(--text-xl); }
h3 { font-size: var(--text-lg); }

p {
  margin-bottom: 1em;
}
p:last-child { margin-bottom: 0; }

a {
  color: var(--primary);
  text-decoration: none;
  transition: color var(--transition-fast);
}

a:hover {
  color: oklch(0.85 0.18 195);
  text-decoration: underline;
}

/* Inline code */
code:not(pre code) {
  font-family: 'JetBrains Mono', 'Fira Code', 'SF Mono', ui-monospace, monospace;
  font-size: 0.875em;
  padding: 0.125rem 0.375rem;
  background: var(--secondary);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  color: var(--primary);
  overflow-wrap: break-word;
  word-break: break-word;
}

/* Code blocks */
pre {
  font-family: 'JetBrains Mono', 'Fira Code', 'SF Mono', ui-monospace, monospace;
  font-size: 0.8125rem;
  line-height: 1.7;
  background: oklch(0.08 0.015 260);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  padding: var(--space-4);
  overflow-x: auto;
  margin: var(--space-4) 0;
  max-width: 100%;
}

pre code {
  padding: 0;
  background: transparent;
  border: none;
  color: var(--foreground);
  font-size: inherit;
}

/* Lists */
ul, ol {
  margin: var(--space-2) 0;
  padding-left: 1.5em;
}
li {
  margin-bottom: 0.25em;
}
li::marker { color: var(--muted-foreground); }

/* Blockquotes */
blockquote {
  border-left: 3px solid var(--primary);
  padding: var(--space-2) var(--space-4);
  margin: var(--space-4) 0;
  background: linear-gradient(90deg, oklch(0.75 0.18 195 / 0.05) 0%, transparent 100%);
  border-radius: 0 var(--radius-sm) var(--radius-sm) 0;
  color: var(--secondary-foreground);
}

/* Tables */
table {
  width: 100%;
  border-collapse: collapse;
  margin: var(--space-4) 0;
  font-size: 0.875rem;
}
th, td {
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border);
  text-align: left;
}
th {
  background: var(--secondary);
  font-weight: 600;
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  color: var(--muted-foreground);
}
tr:hover td {
  background: var(--muted);
}
"#;

const COMPONENT_STYLES: &str = r#"
/* ============================================
   Header - Terminal Style
   ============================================ */

.header {
  margin-bottom: var(--space-6);
  padding: var(--space-4) var(--space-5);
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius-xl);
  position: relative;
}

/* Terminal traffic lights */
.header::before {
  content: '';
  position: absolute;
  top: var(--space-4);
  left: var(--space-5);
  width: 12px;
  height: 12px;
  border-radius: 50%;
  background: oklch(0.65 0.22 25);
  box-shadow:
    20px 0 0 oklch(0.78 0.16 75),
    40px 0 0 oklch(0.72 0.19 145);
}

.header-content {
  padding-left: 72px;
}

.header-title {
  font-size: var(--text-lg);
  font-weight: 600;
  color: var(--foreground);
  margin-bottom: var(--space-2);
  line-height: 1.4;
  font-family: 'JetBrains Mono', ui-monospace, monospace;
}

.header-meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2) var(--space-4);
  font-size: var(--text-sm);
  color: var(--muted-foreground);
}

.header-meta span {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.header-agent {
  color: var(--primary);
  font-weight: 500;
}

.header-project {
  font-family: 'JetBrains Mono', ui-monospace, monospace;
  font-size: var(--text-xs);
  padding: 0.25rem 0.625rem;
  background: var(--secondary);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  color: var(--muted-foreground);
}

/* ============================================
   Toolbar - Glassmorphic
   ============================================ */

.toolbar {
  position: sticky;
  top: var(--space-4);
  z-index: 50;
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-3) var(--space-4);
  margin-bottom: var(--space-6);
  background: oklch(0.14 0.02 260 / 0.8);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
  border: 1px solid oklch(0.3 0.02 260 / 0.3);
  border-radius: var(--radius-xl);
  box-shadow: var(--shadow-lg);
  transition: all var(--transition-normal);
}

.toolbar:hover {
  box-shadow: var(--shadow-xl), var(--shadow-glow-sm);
}

[data-theme="light"] .toolbar {
  background: oklch(1 0 0 / 0.85);
  border-color: var(--border);
}

.search-wrapper {
  flex: 1;
  position: relative;
  min-width: 0;
}

.search-input {
  width: 100%;
  padding: 0.625rem 0.875rem;
  padding-right: 3rem;
  font-size: var(--text-sm);
  color: var(--foreground);
  background: var(--input);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  outline: none;
  transition: all var(--transition-fast);
}

.search-input::placeholder {
  color: var(--muted-foreground);
}

.search-input:hover {
  border-color: oklch(0.35 0.02 260);
}

.search-input:focus {
  border-color: var(--primary);
  box-shadow: 0 0 0 3px oklch(0.75 0.18 195 / 0.15), var(--shadow-glow-sm);
}

.search-count {
  position: absolute;
  right: 0.875rem;
  top: 50%;
  transform: translateY(-50%);
  font-size: var(--text-xs);
  font-weight: 500;
  color: var(--muted-foreground);
  background: var(--secondary);
  padding: 0.125rem 0.375rem;
  border-radius: var(--radius-sm);
}

.toolbar-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: var(--touch-min);
  height: var(--touch-min);
  min-width: var(--touch-min);
  background: transparent;
  border: 1px solid transparent;
  border-radius: var(--radius-md);
  color: var(--muted-foreground);
  cursor: pointer;
  transition: all var(--transition-fast);
  position: relative;
}

.toolbar-btn:hover {
  background: var(--secondary);
  border-color: var(--border);
  color: var(--foreground);
}

.toolbar-btn:active {
  transform: scale(0.95);
}

.toolbar-btn svg {
  width: 20px;
  height: 20px;
  transition: transform var(--transition-fast);
}

.toolbar-btn:hover svg {
  transform: scale(1.1);
}

/* Theme toggle icon states */
.icon-sun, .icon-moon {
  transition: opacity var(--transition-fast), transform var(--transition-fast);
}
[data-theme="dark"] .icon-sun { opacity: 0; position: absolute; transform: rotate(90deg) scale(0.8); }
[data-theme="dark"] .icon-moon { opacity: 1; }
[data-theme="light"] .icon-sun { opacity: 1; }
[data-theme="light"] .icon-moon { opacity: 0; position: absolute; transform: rotate(-90deg) scale(0.8); }

/* ============================================
   Messages - Card Based
   ============================================ */

.conversation {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  position: relative;
  z-index: 1;
}

/* Message wrapper - inherits conversation layout */
.conversation-messages {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}

.message {
  position: relative;
  padding: var(--space-4) var(--space-5);
  background: #1e1e28;
  background: var(--card);
  border: 1px solid #2d2d3a;
  border: 1px solid var(--border);
  border-radius: var(--radius-xl);
  border-left: 4px solid #2d2d3a;
  border-left: 4px solid var(--border);
  transition: all var(--transition-fast);
}

.message:hover {
  border-color: oklch(0.35 0.02 260);
  box-shadow: var(--shadow-md);
}

/* Role-specific styling */
.message-user {
  border-left-color: var(--green);
}
.message-user:hover {
  box-shadow: var(--shadow-md), 0 0 30px -10px var(--green);
}

.message-assistant, .message-agent {
  border-left-color: var(--primary);
}
.message-assistant:hover, .message-agent:hover {
  box-shadow: var(--shadow-md), 0 0 30px -10px var(--primary);
}

.message-tool {
  border-left-color: var(--amber);
}
.message-tool:hover {
  box-shadow: var(--shadow-md), 0 0 30px -10px var(--amber);
}

.message-system {
  border-left-color: var(--purple);
  background: linear-gradient(135deg, var(--card) 0%, oklch(0.65 0.18 290 / 0.03) 100%);
}

.message-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  margin-bottom: var(--space-3);
  padding-bottom: var(--space-2);
  border-bottom: 1px solid oklch(0.25 0.02 260 / 0.5);
}

.message-header-left {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  min-width: 0;
}

.message-header-right {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-1);
  flex-shrink: 0;
}

/* Lucide SVG icon styling */
.lucide-icon {
  display: inline-block;
  vertical-align: middle;
  flex-shrink: 0;
}

.lucide-spin {
  animation: lucide-spin 1s linear infinite;
}

@keyframes lucide-spin {
  from { transform: rotate(0deg); }
  to { transform: rotate(360deg); }
}

.message-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  line-height: 1;
}

.message-icon .lucide-icon {
  width: 16px;
  height: 16px;
}

.message-author {
  font-weight: 600;
  font-size: var(--text-sm);
  letter-spacing: -0.01em;
}

.message-user .message-author { color: var(--green); }
.message-assistant .message-author, .message-agent .message-author { color: var(--primary); }
.message-tool .message-author { color: var(--amber); }
.message-system .message-author { color: var(--purple); }

.message-time {
  margin-left: auto;
  font-size: var(--text-xs);
  font-weight: 500;
  color: var(--muted-foreground);
  font-variant-numeric: tabular-nums;
}

.message-content {
  font-size: var(--text-base);
  line-height: 1.7;
  color: var(--secondary-foreground);
}

.message-content > *:first-child { margin-top: 0; }
.message-content > *:last-child { margin-bottom: 0; }

/* Message content typography */
.message-content p { margin-bottom: 0.85em; }
.message-content h1, .message-content h2, .message-content h3 {
  margin-top: 1.25em;
  margin-bottom: 0.5em;
  font-weight: 600;
  color: var(--foreground);
}
.message-content h1 { font-size: 1.25rem; }
.message-content h2 { font-size: 1.125rem; }
.message-content h3 { font-size: 1rem; }
.message-content ul, .message-content ol {
  margin: 0.5em 0;
  padding-left: 1.25em;
}
.message-content li { margin-bottom: 0.25em; }
.message-content li::marker { color: var(--muted-foreground); }
.message-content strong { color: var(--foreground); font-weight: 600; }

/* Message link button */
.message-link {
  position: absolute;
  top: var(--space-4);
  right: var(--space-4);
  opacity: 0;
  padding: 6px;
  background: var(--secondary);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  color: var(--muted-foreground);
  cursor: pointer;
  transition: all var(--transition-fast);
}

.message:hover .message-link { opacity: 1; }
.message-link:hover {
  color: var(--primary);
  border-color: var(--primary);
  box-shadow: var(--shadow-glow-sm);
}
.message-link.copied {
  color: var(--green);
  border-color: var(--green);
}

/* ============================================
   Tool Calls - Collapsible
   ============================================ */

/* Tool Badge - Compact inline badges with hover popovers */
.tool-badges {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px;
}

.tool-badge {
  position: relative;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-width: 28px;
  height: 28px;
  padding: 0 6px;
  font-size: 0.6875rem;
  font-family: 'JetBrains Mono', ui-monospace, monospace;
  background: transparent;
  border: 1px solid oklch(0.3 0.02 260 / 0.5);
  border-radius: 6px;
  cursor: pointer;
  transition: all var(--transition-fast);
  white-space: nowrap;
  color: var(--amber);
}

.tool-badge:hover,
.tool-badge:focus {
  background: oklch(0.78 0.16 75 / 0.15);
  border-color: var(--amber);
  transform: scale(1.1);
  outline: none;
  box-shadow: var(--shadow-glow-amber);
}

.tool-badge:focus-visible {
  box-shadow: 0 0 0 2px var(--primary);
}

.tool-badge-icon {
  display: flex;
  align-items: center;
  justify-content: center;
}

.tool-badge-icon .lucide-icon {
  width: 14px;
  height: 14px;
  stroke-width: 2;
}

.tool-badge-name {
  font-weight: 600;
  font-size: 0.625rem;
  color: var(--amber);
  margin-left: 4px;
}

.tool-badge-status {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0.125rem;
  border-radius: 2px;
  margin-left: 2px;
}

.tool-badge-status .lucide-icon {
  width: 10px;
  height: 10px;
}

/* Status-based badge styling with subtle left accent */
.tool-badge.tool-status-success { border-color: var(--green); }
.tool-badge.tool-status-error { border-color: var(--red); }
.tool-badge.tool-status-pending { border-color: var(--amber); }

.tool-badge.tool-status-success:hover { box-shadow: 0 4px 20px oklch(0.72 0.19 145 / 0.35); }
.tool-badge.tool-status-error:hover { box-shadow: 0 4px 20px oklch(0.65 0.22 25 / 0.35); }

.tool-badge-status.success {
  background: oklch(0.72 0.19 145 / 0.2);
  color: var(--green);
}
.tool-badge-status.error {
  background: oklch(0.65 0.22 25 / 0.2);
  color: var(--red);
}
.tool-badge-status.pending {
  background: oklch(0.78 0.16 75 / 0.2);
  color: var(--amber);
}

/* Overflow badge - "+X more" */
.tool-badge.tool-overflow {
  min-width: auto;
  padding: 0 8px;
  font-size: 0.6875rem;
  font-weight: 600;
  color: var(--muted-foreground);
  border-style: dashed;
}

.tool-badge.tool-overflow:hover {
  color: var(--foreground);
  border-style: solid;
}

/* Expanded state for overflow - show all badges */
.message-header-right.expanded .tool-badge {
  display: inline-flex;
}

.message-header-right.expanded .tool-overflow {
  order: 999; /* Move to end */
}

/* Popover - Glassmorphic with fixed positioning */
.tool-popover {
  position: fixed;
  z-index: 1000;
  min-width: 280px;
  max-width: 400px;
  max-height: 300px;
  overflow: auto;
  padding: var(--space-3);
  background: oklch(0.14 0.02 260 / 0.95);
  backdrop-filter: blur(16px);
  -webkit-backdrop-filter: blur(16px);
  border: 1px solid oklch(0.3 0.02 260 / 0.5);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-xl), var(--shadow-glow-sm);
  opacity: 0;
  visibility: hidden;
  transform: translateY(-4px);
  transition: all 0.15s ease-out;
  pointer-events: none;
  text-align: left;
  white-space: normal;
}

.tool-popover.visible {
  opacity: 1;
  visibility: visible;
  transform: translateY(0);
  pointer-events: auto;
}

/* Light theme popover */
[data-theme="light"] .tool-popover {
  background: oklch(1 0 0 / 0.95);
  border-color: var(--border);
  box-shadow: 0 8px 32px oklch(0 0 0 / 0.15);
}

/* Arrow indicator (CSS-only, optional) */
.tool-popover::before {
  content: '';
  position: absolute;
  top: -6px;
  left: 20px;
  width: 12px;
  height: 12px;
  background: inherit;
  border: inherit;
  border-right: none;
  border-bottom: none;
  transform: rotate(45deg);
  pointer-events: none;
}

.tool-popover.popover-above::before {
  top: auto;
  bottom: -6px;
  transform: rotate(225deg);
}

.tool-popover-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding-bottom: var(--space-2);
  margin-bottom: var(--space-2);
  border-bottom: 1px solid var(--border);
  font-weight: 600;
  color: var(--amber);
}

.tool-popover-header .lucide-icon {
  width: 14px;
  height: 14px;
  flex-shrink: 0;
}

.tool-popover-header span {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.tool-popover-section {
  margin-bottom: var(--space-2);
}
.tool-popover-section:last-child { margin-bottom: 0; }

.tool-popover-label {
  font-size: 0.5625rem;
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  color: var(--muted-foreground);
  margin-bottom: 0.25rem;
}

.tool-popover pre {
  margin: 0;
  padding: var(--space-2);
  font-size: 0.625rem;
  background: var(--secondary);
  border-radius: var(--radius-sm);
  max-height: 150px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-word;
}

.tool-truncated {
  font-size: 0.5625rem;
  color: var(--amber);
  margin-top: 0.25rem;
  font-weight: 500;
  font-style: italic;
}

/* ============================================
   Floating Navigation
   ============================================ */

.floating-nav {
  position: fixed;
  bottom: calc(24px + env(safe-area-inset-bottom, 0px));
  right: 24px;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  opacity: 0;
  transform: translateY(20px) scale(0.9);
  transition: all var(--transition-normal);
  pointer-events: none;
  z-index: 100;
}

.floating-nav.visible {
  opacity: 1;
  transform: translateY(0) scale(1);
  pointer-events: auto;
}

.floating-btn {
  width: 48px;
  height: 48px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: oklch(0.14 0.02 260 / 0.8);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
  border: 1px solid oklch(0.3 0.02 260 / 0.3);
  border-radius: var(--radius-xl);
  color: var(--muted-foreground);
  cursor: pointer;
  box-shadow: var(--shadow-lg);
  transition: all var(--transition-fast);
}

.floating-btn:hover {
  background: var(--secondary);
  border-color: var(--primary);
  color: var(--primary);
  box-shadow: var(--shadow-lg), var(--shadow-glow);
  transform: translateY(-2px);
}

.floating-btn:active {
  transform: scale(0.95);
}

.floating-btn svg {
  width: 22px;
  height: 22px;
}

/* ============================================
   Scroll Progress
   ============================================ */

.scroll-progress {
  position: fixed;
  top: 0;
  left: 0;
  height: 3px;
  background: linear-gradient(90deg, var(--primary), var(--magenta), var(--primary));
  background-size: 200% 100%;
  z-index: 1000;
  width: 0;
  transition: width 0.1s ease-out;
  box-shadow: 0 0 10px var(--primary);
}

/* ============================================
   Keyboard Shortcuts Hint
   ============================================ */

.shortcuts-hint {
  position: fixed;
  bottom: calc(24px + env(safe-area-inset-bottom, 0px));
  left: 50%;
  transform: translateX(-50%) translateY(20px);
  padding: 0.75rem 1.25rem;
  background: oklch(0.14 0.02 260 / 0.8);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
  border: 1px solid oklch(0.3 0.02 260 / 0.3);
  border-radius: var(--radius-xl);
  font-size: var(--text-xs);
  color: var(--secondary-foreground);
  opacity: 0;
  transition: all var(--transition-normal);
  z-index: 100;
  box-shadow: var(--shadow-xl);
  white-space: nowrap;
}

.shortcuts-hint.visible {
  opacity: 1;
  transform: translateX(-50%) translateY(0);
}

.shortcuts-hint kbd {
  display: inline-block;
  padding: 0.1875rem 0.5rem;
  margin: 0 0.1875rem;
  font-family: 'JetBrains Mono', ui-monospace, monospace;
  font-size: 0.6875rem;
  font-weight: 500;
  background: var(--secondary);
  border: 1px solid var(--border);
  border-radius: 5px;
  box-shadow: 0 2px 0 var(--background);
}

/* ============================================
   Animations
   ============================================ */

@keyframes fadeIn {
  from {
    opacity: 0;
    transform: translateY(12px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}

@keyframes slideUp {
  from {
    opacity: 0;
    transform: translateY(20px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}

/* Staggered fade-in animation - uses forwards to ensure visibility after animation */
.message {
  animation: fadeIn 0.35s cubic-bezier(0.33, 1, 0.68, 1) forwards;
  opacity: 1; /* Fallback for when animations don't run */
}

/* Staggered animation delays for visual polish */
.message:nth-child(1) { animation-delay: 0.02s; }
.message:nth-child(2) { animation-delay: 0.04s; }
.message:nth-child(3) { animation-delay: 0.06s; }
.message:nth-child(4) { animation-delay: 0.08s; }
.message:nth-child(5) { animation-delay: 0.1s; }
.message:nth-child(n+6) { animation-delay: 0.12s; }

/* ============================================
   Accessibility
   ============================================ */

@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 0.01ms !important;
    animation-delay: 0ms !important;
    transition-duration: 0.01ms !important;
    scroll-behavior: auto !important;
  }
  .message { animation: none; }
}

:focus-visible {
  outline: 2px solid var(--primary);
  outline-offset: 2px;
}

@media (prefers-contrast: high) {
  :root {
    --border: oklch(0.5 0.02 260);
    --muted-foreground: oklch(0.75 0.02 260);
  }
  .tool-badge {
    border-width: 2px;
  }
  .message {
    border-width: 2px;
  }
  .tool-popover {
    border-width: 2px;
  }
}

/* ============================================
   MOBILE (< 768px)
   ============================================ */

@media (max-width: 767px) {
  .app-container {
    padding: var(--space-3);
    padding-bottom: calc(80px + env(safe-area-inset-bottom, 0px));
  }

  .header {
    padding: var(--space-3) var(--space-4);
    margin-bottom: var(--space-4);
  }

  .header::before {
    width: 10px;
    height: 10px;
    top: var(--space-3);
    left: var(--space-4);
    box-shadow:
      16px 0 0 oklch(0.78 0.16 75),
      32px 0 0 oklch(0.72 0.19 145);
  }

  .header-content {
    padding-left: 56px;
  }

  .header-title {
    font-size: var(--text-base);
  }

  .header-meta {
    gap: var(--space-1) var(--space-2);
    font-size: var(--text-xs);
  }

  .toolbar {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    top: auto;
    margin: 0;
    padding: var(--space-2);
    padding-bottom: calc(var(--space-2) + env(safe-area-inset-bottom, 0px));
    border-radius: var(--radius-xl) var(--radius-xl) 0 0;
    border-bottom: none;
    z-index: 100;
  }

  .search-input {
    padding: 0.75rem;
    font-size: 1rem; /* Prevent zoom on iOS */
  }

  .conversation {
    gap: var(--space-3);
  }

  .message {
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-lg);
  }

  .message-header {
    gap: var(--space-1);
    margin-bottom: var(--space-2);
    padding-bottom: var(--space-1);
  }

  .message-icon { font-size: 0.875rem; }
  .message-author { font-size: var(--text-xs); }
  .message-time { font-size: 0.625rem; }

  .message-content {
    font-size: var(--text-sm);
    line-height: 1.6;
  }

  .message-link {
    top: var(--space-3);
    right: var(--space-3);
    padding: 8px;
    opacity: 1; /* Always visible on mobile */
  }

  .tool-call {
    margin-top: var(--space-3);
  }

  .tool-call summary {
    padding: var(--space-2);
    min-height: 48px;
  }

  .tool-call-body {
    padding: var(--space-3);
  }

  .tool-call pre {
    font-size: 0.625rem;
    padding: var(--space-1) var(--space-2);
    max-height: 200px;
  }

  .floating-nav {
    bottom: calc(80px + env(safe-area-inset-bottom, 0px));
    right: var(--space-3);
  }

  .floating-btn {
    width: 44px;
    height: 44px;
  }

  .shortcuts-hint {
    display: none;
  }

  /* Larger tap targets */
  button, a, summary {
    min-height: var(--touch-min);
  }

  /* Block-level code overflow */
  pre, code {
    max-width: 100%;
  }

  /* Tool badges - larger touch targets on mobile */
  .tool-badge {
    min-width: 32px;
    height: 32px;
  }

  .tool-badges {
    gap: 6px;
  }

  /* Mobile popover - bottom sheet style */
  .tool-popover {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    top: auto;
    max-width: 100%;
    max-height: 60vh;
    border-radius: var(--radius-xl) var(--radius-xl) 0 0;
    padding: var(--space-4);
    padding-bottom: calc(var(--space-4) + env(safe-area-inset-bottom, 0px));
    transform: translateY(100%);
  }

  .tool-popover.visible {
    transform: translateY(0);
  }

  /* Hide arrow on mobile */
  .tool-popover::before {
    display: none;
  }

  /* Add drag handle indicator */
  .tool-popover::after {
    content: '';
    position: absolute;
    top: 8px;
    left: 50%;
    transform: translateX(-50%);
    width: 36px;
    height: 4px;
    background: oklch(0.4 0.02 260);
    border-radius: 2px;
  }
}

/* ============================================
   TABLET (768px - 1023px)
   ============================================ */

@media (min-width: 768px) and (max-width: 1023px) {
  .message {
    padding: var(--space-4) var(--space-5);
  }
}

/* ============================================
   LARGE DESKTOP (1280px+)
   ============================================ */

@media (min-width: 1280px) {
  .message {
    padding: var(--space-5) var(--space-6);
  }

  .message-content {
    font-size: 1.0625rem;
    line-height: 1.75;
  }

  .toolbar {
    padding: var(--space-4) var(--space-5);
  }
}

/* ============================================
   Message Collapse
   ============================================ */

.message-collapse summary {
  cursor: pointer;
  list-style: none;
}

.message-collapse summary::-webkit-details-marker { display: none; }

.message-preview {
  color: var(--secondary-foreground);
  display: -webkit-box;
  -webkit-line-clamp: 3;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

.message-expand-hint {
  display: block;
  margin-top: 6px;
  font-size: var(--text-xs);
  font-weight: 500;
  color: var(--primary);
}

.message-collapse[open] .message-expand-hint { display: none; }

.message-expanded { margin-top: var(--space-3); }

/* ============================================
   Code Block Copy Button
   ============================================ */

pre {
  position: relative;
}

.copy-code-btn {
  position: absolute;
  top: 8px;
  right: 8px;
  padding: 4px;
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  color: var(--muted-foreground);
  cursor: pointer;
  opacity: 0;
  transition: opacity var(--transition-fast), color var(--transition-fast);
}

pre:hover .copy-code-btn { opacity: 1; }
.copy-code-btn:hover { color: var(--primary); border-color: var(--primary); }
.copy-code-btn.copied { color: var(--green); border-color: var(--green); }

/* ============================================
   Toast Notifications
   ============================================ */

.toast {
  padding: 0.625rem 1rem;
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  color: var(--foreground);
  box-shadow: var(--shadow-lg);
  font-size: var(--text-sm);
}

.toast-success { border-color: var(--green); }
.toast-error { border-color: var(--red); }

/* ============================================
   Agent-Specific Theming
   ============================================ */

.agent-claude .message-assistant { border-left-color: oklch(0.7 0.18 50); }
.agent-codex .message-assistant { border-left-color: oklch(0.7 0.2 145); }
.agent-cursor .message-assistant { border-left-color: oklch(0.7 0.2 280); }
.agent-chatgpt .message-assistant { border-left-color: oklch(0.72 0.19 165); }
.agent-gemini .message-assistant { border-left-color: oklch(0.7 0.2 250); }
.agent-aider .message-assistant { border-left-color: oklch(0.72 0.16 85); }
.agent-copilot .message-assistant { border-left-color: oklch(0.7 0.18 200); }
.agent-cody .message-assistant { border-left-color: oklch(0.68 0.2 340); }
.agent-windsurf .message-assistant { border-left-color: oklch(0.7 0.2 205); }
.agent-amp .message-assistant { border-left-color: oklch(0.7 0.18 270); }
.agent-grok .message-assistant { border-left-color: oklch(0.7 0.22 350); }

/* Print styles */
@media print {
  body::before { display: none; }
  .toolbar, .floating-nav, .scroll-progress { display: none !important; }
  .message {
    background: white;
    backdrop-filter: none;
    box-shadow: none;
    border: 1px solid #ccc;
    break-inside: avoid;
  }
  .message-link { display: none; }
  .copy-code-btn { display: none; }
  .tool-popover { display: none !important; }
  .tool-badge {
    border: 1px solid #666;
    background: #f5f5f5;
    color: #333;
  }
  .tool-badge-icon { color: #666; }
}
"#;

const SEARCH_STYLES: &str = r#"
/* Search highlighting */
.search-highlight {
  background: oklch(0.75 0.18 195 / 0.3);
  border-radius: 2px;
  padding: 1px 0;
}

.search-current {
  background: oklch(0.78 0.16 75 / 0.5);
}
"#;

const ENCRYPTION_STYLES: &str = r#"
/* Encryption modal */
.decrypt-modal {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: flex;
  align-items: center;
  justify-content: center;
  background: oklch(0 0 0 / 0.85);
  backdrop-filter: blur(8px);
}

.decrypt-form {
  width: 100%;
  max-width: 360px;
  padding: var(--space-6);
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius-xl);
  box-shadow: var(--shadow-xl);
}

.decrypt-form h2 {
  margin: 0 0 var(--space-4);
  font-size: var(--text-lg);
  color: var(--foreground);
}

.decrypt-form input {
  width: 100%;
  padding: 0.625rem 0.75rem;
  margin-bottom: var(--space-3);
  background: var(--input);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  color: var(--foreground);
  font-size: var(--text-sm);
}

.decrypt-form input:focus {
  outline: none;
  border-color: var(--primary);
  box-shadow: 0 0 0 3px oklch(0.75 0.18 195 / 0.15);
}

.decrypt-form button {
  width: 100%;
  padding: 0.625rem;
  background: var(--primary);
  border: none;
  border-radius: var(--radius-md);
  color: var(--primary-foreground);
  font-size: var(--text-sm);
  font-weight: 600;
  cursor: pointer;
  transition: background var(--transition-fast);
}

.decrypt-form button:hover {
  background: oklch(0.8 0.18 195);
}

.decrypt-error {
  color: var(--red);
  font-size: var(--text-sm);
  margin-top: var(--space-2);
}
"#;

fn generate_print_css() -> String {
    String::from(
        r#"@media print {
  body {
    font-size: 11pt;
    background: #fff;
    color: #000;
  }
  .message {
    border: 1px solid #ddd;
    page-break-inside: avoid;
  }
  pre {
    border: 1px solid #ddd;
    background: #f5f5f5;
  }
  a {
    color: #000;
    text-decoration: underline;
  }
}"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_styles_includes_colors() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("--background"));
        assert!(bundle.critical_css.contains("--foreground"));
    }

    #[test]
    fn test_generate_styles_includes_search_when_enabled() {
        let opts = ExportOptions {
            include_search: true,
            ..Default::default()
        };
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains(".search-highlight"));
    }

    #[test]
    fn test_generate_styles_excludes_search_when_disabled() {
        let opts = ExportOptions {
            include_search: false,
            ..Default::default()
        };
        let bundle = generate_styles(&opts);
        assert!(!bundle.critical_css.contains(".search-highlight"));
    }

    #[test]
    fn test_styles_include_theme_toggle_when_enabled() {
        let opts = ExportOptions {
            include_theme_toggle: true,
            ..Default::default()
        };
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("[data-theme=\"light\"]"));
    }

    #[test]
    fn test_styles_include_responsive_breakpoints() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("@media"));
    }

    #[test]
    fn test_print_css_hides_interactive_elements() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.print_css.contains("@media print"));
    }

    #[test]
    fn test_styles_include_accessibility() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("prefers-reduced-motion"));
    }

    #[test]
    fn test_styles_include_animations() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("@keyframes"));
    }

    #[test]
    fn test_styles_include_glass_morphism() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("backdrop-filter: blur"));
        assert!(bundle.critical_css.contains(".glass"));
    }

    #[test]
    fn test_styles_include_oklch_colors() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);
        assert!(bundle.critical_css.contains("oklch(0.11 0.015 260)"));
        assert!(bundle.critical_css.contains("oklch(0.75 0.18 195)"));
    }

    #[test]
    fn test_styles_include_tool_badge_styling() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        // Tool badge base styles
        assert!(bundle.critical_css.contains(".tool-badge"));
        assert!(bundle.critical_css.contains("min-width: 28px"));
        assert!(bundle.critical_css.contains("height: 28px"));

        // Status variants
        assert!(bundle.critical_css.contains(".tool-status-success"));
        assert!(bundle.critical_css.contains(".tool-status-error"));

        // Overflow badge
        assert!(bundle.critical_css.contains(".tool-overflow"));
    }

    #[test]
    fn test_styles_include_glassmorphism_popover() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        // Glassmorphic popover
        assert!(bundle.critical_css.contains(".tool-popover"));
        assert!(bundle.critical_css.contains("backdrop-filter: blur(16px)"));
        assert!(bundle.critical_css.contains("-webkit-backdrop-filter: blur(16px)"));

        // Fixed positioning
        assert!(bundle.critical_css.contains("position: fixed"));

        // Visibility states
        assert!(bundle.critical_css.contains(".tool-popover.visible"));
    }

    #[test]
    fn test_styles_include_mobile_bottom_sheet() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        // Mobile popover becomes bottom sheet
        assert!(bundle.critical_css.contains("max-height: 60vh"));
        assert!(bundle.critical_css.contains("border-radius: var(--radius-xl) var(--radius-xl) 0 0"));
    }

    #[test]
    fn test_styles_include_high_contrast() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        // High contrast mode
        assert!(bundle.critical_css.contains("prefers-contrast: high"));
        assert!(bundle.critical_css.contains("border-width: 2px"));
    }

    #[test]
    fn test_styles_include_glow_effects() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        // Glow shadow variables
        assert!(bundle.critical_css.contains("--shadow-glow"));
        assert!(bundle.critical_css.contains("--shadow-glow-amber"));

        // Hover glow on tool badges
        assert!(bundle.critical_css.contains("box-shadow: var(--shadow-glow-amber)"));
    }

    #[test]
    fn test_print_styles_hide_popovers() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        // Print mode hides popovers
        assert!(bundle.critical_css.contains(".tool-popover { display: none"));
    }
}
