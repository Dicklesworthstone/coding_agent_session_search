//! CSS generation for HTML export.
//!
//! Generates world-class, Stripe-level CSS with:
//! - Beautiful micro-interactions and animations
//! - Perfect typography hierarchy with fluid scaling
//! - Desktop-optimized hover states, keyboard navigation, and wide-screen layouts
//! - Mobile-optimized touch targets (44px min), gestures, and bottom navigation
//! - Elegant depth through layered shadows and glassmorphism
//! - Accessibility-first design with reduced motion and high contrast support
//! - Print-optimized styles for clean PDF output
//!
//! ## Design Philosophy
//!
//! Following Stripe's design principles:
//! - **Restraint**: Limited color palette with strategic accent usage
//! - **Whitespace**: Generous padding creates breathing room
//! - **Typography**: Clear hierarchy through weight, size, and color
//! - **Motion**: Subtle, purposeful animations that feel responsive
//! - **Depth**: Layered shadows create visual hierarchy
//! - **Polish**: Every pixel is intentional, every interaction delightful

use super::colors;
use super::template::ExportOptions;

/// Bundle of CSS styles for the template.
pub struct StyleBundle {
    /// Critical CSS inlined in the document (required for offline)
    pub critical_css: String,

    /// Print-specific CSS
    pub print_css: String,
}

/// Generate all CSS styles for the template.
pub fn generate_styles(options: &ExportOptions) -> StyleBundle {
    StyleBundle {
        critical_css: generate_critical_css(options),
        print_css: generate_print_css(),
    }
}

fn generate_critical_css(options: &ExportOptions) -> String {
    let search_styles = if options.include_search {
        SEARCH_STYLES
    } else {
        ""
    };

    let theme_toggle_styles = if options.include_theme_toggle {
        THEME_TOGGLE_STYLES
    } else {
        ""
    };

    let encryption_styles = if options.encrypt {
        ENCRYPTION_STYLES
    } else {
        ""
    };

    format!(
        r#"{base_variables}
{reset_and_base}
{typography}
{layout}
{header_styles}
{toolbar_styles}
{message_styles}
{code_block_styles}
{tool_call_styles}
{animations}
{search_styles}
{theme_toggle_styles}
{encryption_styles}
{desktop_enhancements}
{mobile_optimizations}
{world_class_enhancements}
{mobile_world_class}
{accessibility}
{utility_classes}"#,
        base_variables = generate_base_variables(),
        reset_and_base = RESET_AND_BASE,
        typography = TYPOGRAPHY,
        layout = LAYOUT,
        header_styles = HEADER_STYLES,
        toolbar_styles = TOOLBAR_STYLES,
        message_styles = MESSAGE_STYLES,
        code_block_styles = CODE_BLOCK_STYLES,
        tool_call_styles = TOOL_CALL_STYLES,
        animations = ANIMATIONS,
        search_styles = search_styles,
        theme_toggle_styles = theme_toggle_styles,
        encryption_styles = encryption_styles,
        desktop_enhancements = DESKTOP_ENHANCEMENTS,
        mobile_optimizations = MOBILE_OPTIMIZATIONS,
        world_class_enhancements = WORLD_CLASS_ENHANCEMENTS,
        mobile_world_class = MOBILE_WORLD_CLASS,
        accessibility = ACCESSIBILITY,
        utility_classes = UTILITY_CLASSES,
    )
}

fn generate_base_variables() -> String {
    format!(
        r#"/* ============================================
   CSS Custom Properties - Tokyo Night Palette
   World-class design system inspired by Stripe
   ============================================ */
:root {{
    /* Background layers - creates visual depth */
    --bg-deep: {bg_deep};
    --bg-surface: {bg_surface};
    --bg-elevated: {bg_highlight};
    --bg-overlay: rgba(26, 27, 38, 0.97);
    --bg-frosted: rgba(36, 40, 59, 0.85);

    /* Borders - subtle definition */
    --border: {border};
    --border-subtle: rgba(59, 66, 97, 0.5);
    --border-hover: {border_focus};
    --border-focus: {accent_primary};

    /* Text hierarchy */
    --text-primary: {text_primary};
    --text-secondary: {text_secondary};
    --text-muted: {text_muted};
    --text-disabled: {text_disabled};
    --text-inverse: {bg_deep};

    /* Accent colors for interactive elements */
    --accent: {accent_primary};
    --accent-hover: #8cb4f8;
    --accent-pressed: #6992e5;
    --accent-muted: rgba(122, 162, 247, 0.15);
    --accent-secondary: {accent_secondary};
    --accent-tertiary: {accent_tertiary};

    /* Role-specific colors - conversation participants */
    --role-user: {role_user};
    --role-user-bg: {role_user_bg};
    --role-user-glow: rgba(158, 206, 106, 0.12);
    --role-user-border: rgba(158, 206, 106, 0.25);

    --role-agent: {role_agent};
    --role-agent-bg: {role_agent_bg};
    --role-agent-glow: rgba(122, 162, 247, 0.12);
    --role-agent-border: rgba(122, 162, 247, 0.25);

    --role-tool: {role_tool};
    --role-tool-bg: {role_tool_bg};
    --role-tool-glow: rgba(255, 158, 100, 0.12);
    --role-tool-border: rgba(255, 158, 100, 0.25);

    --role-system: {role_system};
    --role-system-bg: {role_system_bg};
    --role-system-glow: rgba(224, 175, 104, 0.12);
    --role-system-border: rgba(224, 175, 104, 0.25);

    /* Status colors with borders */
    --success: {status_success};
    --success-bg: rgba(115, 218, 202, 0.1);
    --success-border: rgba(115, 218, 202, 0.25);
    --warning: {status_warning};
    --warning-bg: rgba(224, 175, 104, 0.1);
    --warning-border: rgba(224, 175, 104, 0.25);
    --error: {status_error};
    --error-bg: rgba(247, 118, 142, 0.1);
    --error-border: rgba(247, 118, 142, 0.25);
    --info: {status_info};
    --info-bg: rgba(125, 207, 255, 0.1);
    --info-border: rgba(125, 207, 255, 0.25);

    /* Shadows - Stripe-inspired layered depth */
    --shadow-xs: 0 1px 2px rgba(0, 0, 0, 0.04);
    --shadow-sm: 0 1px 3px rgba(0, 0, 0, 0.06), 0 1px 2px rgba(0, 0, 0, 0.04);
    --shadow-md: 0 4px 6px -1px rgba(0, 0, 0, 0.1), 0 2px 4px -1px rgba(0, 0, 0, 0.06);
    --shadow-lg: 0 10px 15px -3px rgba(0, 0, 0, 0.12), 0 4px 6px -2px rgba(0, 0, 0, 0.05);
    --shadow-xl: 0 20px 25px -5px rgba(0, 0, 0, 0.15), 0 10px 10px -5px rgba(0, 0, 0, 0.04);
    --shadow-2xl: 0 25px 50px -12px rgba(0, 0, 0, 0.25);
    --shadow-glow: 0 0 20px rgba(122, 162, 247, 0.2);
    --shadow-glow-lg: 0 0 40px rgba(122, 162, 247, 0.15);
    --shadow-inner: inset 0 2px 4px 0 rgba(0, 0, 0, 0.06);
    --shadow-ring: 0 0 0 3px rgba(122, 162, 247, 0.15);
    --shadow-ring-error: 0 0 0 3px rgba(247, 118, 142, 0.15);
    --shadow-ring-success: 0 0 0 3px rgba(115, 218, 202, 0.15);

    /* Timing functions - buttery smooth */
    --ease-out-expo: cubic-bezier(0.16, 1, 0.3, 1);
    --ease-out-back: cubic-bezier(0.34, 1.56, 0.64, 1);
    --ease-out-quart: cubic-bezier(0.25, 1, 0.5, 1);
    --ease-in-out: cubic-bezier(0.4, 0, 0.2, 1);
    --spring: cubic-bezier(0.175, 0.885, 0.32, 1.275);
    --spring-bouncy: cubic-bezier(0.68, -0.55, 0.265, 1.55);

    /* Duration scale */
    --duration-fast: 150ms;
    --duration-normal: 250ms;
    --duration-slow: 400ms;

    /* Spacing scale - 4px base grid */
    --space-0: 0;
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
    --space-20: 5rem;
    --space-xs: 0.25rem;
    --space-sm: 0.5rem;
    --space-md: 1rem;
    --space-lg: 1.5rem;
    --space-xl: 2rem;
    --space-2xl: 3rem;
    --space-3xl: 4rem;

    /* Border radius */
    --radius-sm: 6px;
    --radius-md: 10px;
    --radius-lg: 14px;
    --radius-xl: 18px;
    --radius-2xl: 24px;
    --radius-full: 9999px;

    /* Z-index layers */
    --z-base: 0;
    --z-raised: 10;
    --z-dropdown: 100;
    --z-sticky: 200;
    --z-overlay: 250;
    --z-modal: 300;
    --z-toast: 400;
    --z-tooltip: 500;

    /* Typography scale - fluid sizing */
    --text-xs: clamp(0.6875rem, 0.65rem + 0.1vw, 0.75rem);
    --text-sm: clamp(0.8125rem, 0.79rem + 0.1vw, 0.875rem);
    --text-base: clamp(0.9375rem, 0.9rem + 0.15vw, 1rem);
    --text-lg: clamp(1.0625rem, 1rem + 0.2vw, 1.125rem);
    --text-xl: clamp(1.1875rem, 1.1rem + 0.3vw, 1.25rem);
    --text-2xl: clamp(1.375rem, 1.2rem + 0.5vw, 1.5rem);
    --text-3xl: clamp(1.625rem, 1.4rem + 0.8vw, 1.875rem);
    --text-4xl: clamp(1.875rem, 1.6rem + 1vw, 2.25rem);

    /* Line heights */
    --leading-tight: 1.25;
    --leading-snug: 1.375;
    --leading-normal: 1.5;
    --leading-relaxed: 1.625;
    --leading-loose: 1.75;

    /* Content widths */
    --content-width: min(900px, 100% - var(--space-8));
    --content-width-lg: min(1100px, 100% - var(--space-10));
}}"#,
        bg_deep = colors::BG_DEEP,
        bg_surface = colors::BG_SURFACE,
        bg_highlight = colors::BG_HIGHLIGHT,
        border = colors::BORDER,
        border_focus = colors::BORDER_FOCUS,
        text_primary = colors::TEXT_PRIMARY,
        text_secondary = colors::TEXT_SECONDARY,
        text_muted = colors::TEXT_MUTED,
        text_disabled = colors::TEXT_DISABLED,
        accent_primary = colors::ACCENT_PRIMARY,
        accent_secondary = colors::ACCENT_SECONDARY,
        accent_tertiary = colors::ACCENT_TERTIARY,
        role_user = colors::ROLE_USER,
        role_user_bg = colors::ROLE_USER_BG,
        role_agent = colors::ROLE_AGENT,
        role_agent_bg = colors::ROLE_AGENT_BG,
        role_tool = colors::ROLE_TOOL,
        role_tool_bg = colors::ROLE_TOOL_BG,
        role_system = colors::ROLE_SYSTEM,
        role_system_bg = colors::ROLE_SYSTEM_BG,
        status_success = colors::STATUS_SUCCESS,
        status_warning = colors::STATUS_WARNING,
        status_error = colors::STATUS_ERROR,
        status_info = colors::STATUS_INFO,
    )
}

const RESET_AND_BASE: &str = r#"
/* ============================================
   Reset & Base Styles - Stripe-level Foundation
   ============================================ */
*, *::before, *::after {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
}

html {
    font-size: 16px;
    -webkit-text-size-adjust: 100%;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
    text-rendering: optimizeLegibility;
    font-feature-settings: 'kern' 1, 'liga' 1;
    scroll-behavior: smooth;
    scroll-padding-top: var(--space-20);
}

body {
    font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Display', 'SF Pro Text',
                 'Segoe UI Variable', 'Segoe UI', system-ui, Roboto,
                 'Helvetica Neue', Arial, sans-serif;
    font-size: var(--text-base);
    line-height: var(--leading-relaxed);
    color: var(--text-primary);
    background: var(--bg-deep);
    background-image: radial-gradient(ellipse 80% 50% at 50% -20%, rgba(122, 162, 247, 0.03), transparent);
    min-height: 100vh;
    min-height: 100dvh;
    overflow-x: hidden;
    overflow-y: scroll;
}

/* Elegant scrollbar styling */
::-webkit-scrollbar {
    width: 10px;
    height: 10px;
}
::-webkit-scrollbar-track {
    background: transparent;
}
::-webkit-scrollbar-thumb {
    background: var(--border);
    border: 2px solid var(--bg-deep);
    border-radius: var(--radius-full);
    transition: background var(--duration-fast) var(--ease-out-expo);
}
::-webkit-scrollbar-thumb:hover {
    background: var(--border-hover);
}
::-webkit-scrollbar-corner {
    background: transparent;
}

/* Firefox scrollbar */
* {
    scrollbar-width: thin;
    scrollbar-color: var(--border) transparent;
}

/* Selection styling */
::selection {
    background: var(--accent);
    color: var(--text-inverse);
}

/* Image handling */
img, picture, video, canvas, svg {
    display: block;
    max-width: 100%;
    height: auto;
}

/* Form elements inherit fonts */
input, button, textarea, select {
    font: inherit;
    color: inherit;
}

/* Remove button styling */
button {
    background: none;
    border: none;
    cursor: pointer;
}

/* Links base */
a {
    color: inherit;
    text-decoration: none;
}
"#;

const TYPOGRAPHY: &str = r#"
/* ============================================
   Typography - Stripe-level Perfect Hierarchy
   ============================================ */
h1, h2, h3, h4, h5, h6 {
    font-weight: 650;
    line-height: var(--leading-tight);
    letter-spacing: -0.025em;
    color: var(--text-primary);
    text-wrap: balance;
}

h1 {
    font-size: var(--text-4xl);
    font-weight: 700;
    letter-spacing: -0.03em;
}
h2 {
    font-size: var(--text-3xl);
    letter-spacing: -0.025em;
}
h3 {
    font-size: var(--text-2xl);
}
h4 {
    font-size: var(--text-xl);
}
h5 {
    font-size: var(--text-lg);
    font-weight: 600;
}
h6 {
    font-size: var(--text-base);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
}

p {
    margin-bottom: var(--space-md);
    color: var(--text-secondary);
    line-height: var(--leading-relaxed);
    text-wrap: pretty;
}

p:last-child {
    margin-bottom: 0;
}

/* Links with smooth transitions */
.message-content a,
a.link {
    color: var(--accent);
    text-decoration: none;
    background-image: linear-gradient(var(--accent-muted), var(--accent-muted));
    background-size: 100% 2px;
    background-position: 0 100%;
    background-repeat: no-repeat;
    padding-bottom: 1px;
    transition: color var(--duration-fast) var(--ease-out-expo),
                background-size var(--duration-fast) var(--ease-out-expo);
}

.message-content a:hover,
a.link:hover {
    color: var(--accent-hover);
    background-size: 100% 100%;
}

strong, b {
    font-weight: 600;
    color: var(--text-primary);
}

em, i {
    font-style: italic;
}

small {
    font-size: var(--text-sm);
    color: var(--text-muted);
}

/* Monospace font stack with ligatures */
code, pre, .mono, kbd {
    font-family: 'JetBrains Mono', 'SF Mono', 'Fira Code', 'Cascadia Code',
                 'Consolas', 'Liberation Mono', 'Menlo', monospace;
    font-feature-settings: 'liga' 1, 'calt' 1, 'zero' 1;
    font-variant-ligatures: common-ligatures;
}

/* Lists */
ul, ol {
    padding-left: var(--space-6);
    margin-bottom: var(--space-md);
}

li {
    margin-bottom: var(--space-2);
    line-height: var(--leading-relaxed);
}

li::marker {
    color: var(--text-muted);
}

/* Blockquotes */
blockquote {
    margin: var(--space-md) 0;
    padding: var(--space-md) var(--space-lg);
    border-left: 3px solid var(--accent);
    background: var(--bg-elevated);
    border-radius: 0 var(--radius-md) var(--radius-md) 0;
    font-style: italic;
    color: var(--text-secondary);
}

/* Horizontal rule */
hr {
    border: none;
    height: 1px;
    background: linear-gradient(90deg, transparent, var(--border), transparent);
    margin: var(--space-xl) 0;
}
"#;

const LAYOUT: &str = r#"
/* ============================================
   Layout - Container & Structure
   ============================================ */
#app {
    max-width: 900px;
    margin: 0 auto;
    padding: var(--space-xl) var(--space-md);
    min-height: 100vh;
}

/* Glass morphism effect for elevated surfaces */
.glass {
    background: rgba(36, 40, 59, 0.8);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    border: 1px solid rgba(59, 66, 97, 0.5);
}
"#;

const HEADER_STYLES: &str = r#"
/* ============================================
   Header - Session Metadata
   ============================================ */
.header {
    margin-bottom: var(--space-xl);
    padding-bottom: var(--space-lg);
    border-bottom: 1px solid var(--border);
    position: relative;
}

.header::after {
    content: '';
    position: absolute;
    bottom: -1px;
    left: 0;
    right: 0;
    height: 1px;
    background: linear-gradient(90deg, var(--accent) 0%, transparent 100%);
    opacity: 0.5;
}

.title {
    font-size: 1.75rem;
    font-weight: 700;
    margin: 0 0 var(--space-sm);
    background: linear-gradient(135deg, var(--text-primary) 0%, var(--accent) 100%);
    -webkit-background-clip: text;
    -webkit-text-fill-color: transparent;
    background-clip: text;
}

.meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-md);
    font-size: 0.875rem;
    color: var(--text-muted);
}

.meta-item {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
}

.agent-badge {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    background: var(--role-agent-bg);
    border: 1px solid var(--role-agent);
    border-radius: var(--radius-full);
    font-weight: 500;
    font-size: 0.75rem;
    color: var(--role-agent);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    transition: all 0.2s var(--ease-out-expo);
}

.agent-badge:hover {
    background: var(--role-agent);
    color: var(--bg-deep);
    box-shadow: var(--shadow-glow);
}

/* Agent-specific badge colors */
.agent-claude { border-color: #7aa2f7; color: #7aa2f7; }
.agent-codex { border-color: #9ece6a; color: #9ece6a; }
.agent-cursor { border-color: #bb9af7; color: #bb9af7; }
.agent-chatgpt { border-color: #73daca; color: #73daca; }
.agent-gemini { border-color: #7dcfff; color: #7dcfff; }
.agent-aider { border-color: #ff9e64; color: #ff9e64; }

.workspace {
    padding: var(--space-xs) var(--space-sm);
    background: var(--bg-elevated);
    border-radius: var(--radius-sm);
    font-family: monospace;
    font-size: 0.75rem;
    max-width: 200px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}
"#;

const TOOLBAR_STYLES: &str = r#"
/* ============================================
   Toolbar - Actions & Controls
   ============================================ */
.toolbar {
    position: sticky;
    top: var(--space-md);
    z-index: var(--z-sticky);
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    margin-bottom: var(--space-xl);
    padding: var(--space-sm) var(--space-md);
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-lg);
    backdrop-filter: blur(8px);
    -webkit-backdrop-filter: blur(8px);
}

.toolbar-group {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
}

.toolbar-divider {
    width: 1px;
    height: 24px;
    background: var(--border);
    margin: 0 var(--space-sm);
}

.toolbar-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 40px;
    height: 40px;
    padding: 0;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
    color: var(--text-secondary);
    cursor: pointer;
    transition: all 0.2s var(--ease-out-expo);
    position: relative;
}

.toolbar-btn::before {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: inherit;
    background: var(--accent);
    opacity: 0;
    transform: scale(0.8);
    transition: all 0.2s var(--ease-out-expo);
}

.toolbar-btn:hover {
    color: var(--text-primary);
    border-color: var(--border);
    background: var(--bg-elevated);
}

.toolbar-btn:hover::before {
    opacity: 0.1;
    transform: scale(1);
}

.toolbar-btn:active {
    transform: scale(0.95);
}

.toolbar-btn .icon {
    width: 20px;
    height: 20px;
    position: relative;
    z-index: 1;
}

/* Tooltip on hover */
.toolbar-btn[data-tooltip]::after {
    content: attr(data-tooltip);
    position: absolute;
    bottom: calc(100% + 8px);
    left: 50%;
    transform: translateX(-50%) translateY(4px);
    padding: var(--space-xs) var(--space-sm);
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    font-size: 0.75rem;
    color: var(--text-primary);
    white-space: nowrap;
    opacity: 0;
    visibility: hidden;
    transition: all 0.2s var(--ease-out-expo);
    box-shadow: var(--shadow-md);
    z-index: var(--z-tooltip);
}

.toolbar-btn[data-tooltip]:hover::after {
    opacity: 1;
    visibility: visible;
    transform: translateX(-50%) translateY(0);
}
"#;

const MESSAGE_STYLES: &str = r#"
/* ============================================
   Messages - Stripe-level Conversation Flow
   ============================================ */
.conversation {
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
}

/* Staggered entrance animation */
.conversation .message:nth-child(1) { animation-delay: 0ms; }
.conversation .message:nth-child(2) { animation-delay: 50ms; }
.conversation .message:nth-child(3) { animation-delay: 100ms; }
.conversation .message:nth-child(4) { animation-delay: 150ms; }
.conversation .message:nth-child(5) { animation-delay: 200ms; }
.conversation .message:nth-child(n+6) { animation-delay: 250ms; }

.message {
    position: relative;
    padding: var(--space-6);
    border-radius: var(--radius-xl);
    border: 1px solid var(--border-subtle);
    background: var(--bg-surface);
    box-shadow: var(--shadow-xs);
    transition: transform var(--duration-normal) var(--ease-out-expo),
                box-shadow var(--duration-normal) var(--ease-out-expo),
                border-color var(--duration-fast) var(--ease-out-expo);
    animation: messageSlideIn 0.5s var(--ease-out-expo) backwards;
    will-change: transform;
}

@keyframes messageSlideIn {
    from {
        opacity: 0;
        transform: translateY(16px) scale(0.98);
    }
    to {
        opacity: 1;
        transform: translateY(0) scale(1);
    }
}

/* Accent bar on the left */
.message::before {
    content: '';
    position: absolute;
    left: 0;
    top: var(--space-4);
    bottom: var(--space-4);
    width: 3px;
    border-radius: var(--radius-full);
    opacity: 0.8;
    transition: opacity var(--duration-fast) var(--ease-out-expo),
                transform var(--duration-fast) var(--ease-out-expo);
}

/* Subtle inner glow on hover */
.message::after {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: inherit;
    pointer-events: none;
    opacity: 0;
    transition: opacity var(--duration-normal) var(--ease-out-expo);
}

.message:hover {
    border-color: var(--border);
    box-shadow: var(--shadow-md);
    transform: translateY(-2px);
}

.message:hover::before {
    opacity: 1;
    transform: scaleY(1.1);
}

.message:hover::after {
    opacity: 1;
}

/* Target/anchor highlighting */
.message:target {
    animation: messageHighlight 2s var(--ease-out-expo);
}

@keyframes messageHighlight {
    0%, 30% {
        box-shadow: var(--shadow-ring), var(--shadow-md);
    }
}

/* ─────────────────────────────────────────
   Role-specific styling - Visual Identity
   ───────────────────────────────────────── */
.message-user {
    background: var(--role-user-bg);
    border-color: var(--role-user-border);
}
.message-user::before {
    background: var(--role-user);
}
.message-user::after {
    background: radial-gradient(ellipse at top left, var(--role-user-glow), transparent 70%);
}
.message-user:hover {
    border-color: rgba(158, 206, 106, 0.4);
}

.message-assistant, .message-agent {
    background: var(--role-agent-bg);
    border-color: var(--role-agent-border);
}
.message-assistant::before, .message-agent::before {
    background: var(--role-agent);
}
.message-assistant::after, .message-agent::after {
    background: radial-gradient(ellipse at top left, var(--role-agent-glow), transparent 70%);
}
.message-assistant:hover, .message-agent:hover {
    border-color: rgba(122, 162, 247, 0.4);
}

.message-tool {
    background: var(--role-tool-bg);
    border-color: var(--role-tool-border);
}
.message-tool::before {
    background: var(--role-tool);
}
.message-tool::after {
    background: radial-gradient(ellipse at top left, var(--role-tool-glow), transparent 70%);
}
.message-tool:hover {
    border-color: rgba(255, 158, 100, 0.4);
}

.message-system {
    background: var(--role-system-bg);
    border-color: var(--role-system-border);
}
.message-system::before {
    background: var(--role-system);
}
.message-system::after {
    background: radial-gradient(ellipse at top left, var(--role-system-glow), transparent 70%);
}
.message-system:hover {
    border-color: rgba(224, 175, 104, 0.4);
}

/* ─────────────────────────────────────────
   Message Header - Author & Timestamp
   ───────────────────────────────────────── */
.message-header {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-bottom: var(--space-4);
}

/* Role icon styling */
.role-icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    font-size: 1rem;
    background: var(--bg-elevated);
    border-radius: var(--radius-md);
    flex-shrink: 0;
}

.message-user .role-icon { background: var(--role-user-glow); }
.message-assistant .role-icon, .message-agent .role-icon { background: var(--role-agent-glow); }
.message-tool .role-icon { background: var(--role-tool-glow); }
.message-system .role-icon { background: var(--role-system-glow); }

.message-author {
    font-weight: 600;
    font-size: var(--text-sm);
    letter-spacing: 0.01em;
}

.message-user .message-author { color: var(--role-user); }
.message-assistant .message-author, .message-agent .message-author { color: var(--role-agent); }
.message-tool .message-author { color: var(--role-tool); }
.message-system .message-author { color: var(--role-system); }

.message-time {
    margin-left: auto;
    font-size: var(--text-xs);
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
    opacity: 0.8;
    transition: opacity var(--duration-fast);
}

.message:hover .message-time {
    opacity: 1;
}

/* ─────────────────────────────────────────
   Message Content - Beautiful Typography
   ───────────────────────────────────────── */
.message-content {
    color: var(--text-secondary);
    line-height: var(--leading-relaxed);
    font-size: var(--text-base);
}

.message-content > *:first-child {
    margin-top: 0;
}

.message-content > *:last-child {
    margin-bottom: 0;
}

.message-content p {
    margin-bottom: var(--space-4);
}

.message-content ul, .message-content ol {
    margin: var(--space-4) 0;
    padding-left: var(--space-6);
}

.message-content li {
    margin-bottom: var(--space-2);
}

/* Message collapse/expand for long content */
.message-collapsed summary {
    cursor: pointer;
    list-style: none;
}

.message-collapsed summary::-webkit-details-marker {
    display: none;
}

.message-preview {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
}

.preview-text {
    color: var(--text-secondary);
    display: -webkit-box;
    -webkit-line-clamp: 3;
    -webkit-box-orient: vertical;
    overflow: hidden;
}

.expand-hint {
    font-size: var(--text-xs);
    color: var(--accent);
    font-weight: 500;
}

.expand-hint:hover {
    text-decoration: underline;
}

.message-full {
    animation: expandContent 0.3s var(--ease-out-expo);
}

@keyframes expandContent {
    from {
        opacity: 0;
        transform: translateY(-8px);
    }
}
"#;

const CODE_BLOCK_STYLES: &str = r#"
/* ============================================
   Code Blocks - Polished Developer Experience
   ============================================ */
pre {
    position: relative;
    margin: var(--space-5) 0;
    padding: 0;
    background: var(--bg-deep);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    overflow: hidden;
    box-shadow: var(--shadow-xs);
    transition: border-color var(--duration-fast) var(--ease-out-expo),
                box-shadow var(--duration-normal) var(--ease-out-expo);
}

pre:hover {
    border-color: var(--border);
    box-shadow: var(--shadow-sm);
}

/* Horizontal scroll shadow indicators */
pre::before,
pre::after {
    content: '';
    position: absolute;
    top: 0;
    bottom: 0;
    width: 24px;
    pointer-events: none;
    z-index: 2;
    opacity: 0;
    transition: opacity var(--duration-fast);
}

pre::before {
    left: 0;
    background: linear-gradient(90deg, var(--bg-deep) 0%, transparent 100%);
}

pre::after {
    right: 0;
    background: linear-gradient(270deg, var(--bg-deep) 0%, transparent 100%);
}

pre:hover::after {
    opacity: 1;
}

pre code {
    display: block;
    padding: var(--space-5);
    overflow-x: auto;
    font-size: var(--text-sm);
    line-height: var(--leading-relaxed);
    tab-size: 4;
    background: none;
    /* Custom scrollbar for code */
    scrollbar-width: thin;
    scrollbar-color: var(--border) transparent;
}

pre code::-webkit-scrollbar {
    height: 6px;
}

pre code::-webkit-scrollbar-track {
    background: transparent;
}

pre code::-webkit-scrollbar-thumb {
    background: var(--border);
    border-radius: var(--radius-full);
}

pre code::-webkit-scrollbar-thumb:hover {
    background: var(--border-hover);
}

/* Code header with language badge */
.code-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-2) var(--space-4);
    background: var(--bg-elevated);
    border-bottom: 1px solid var(--border-subtle);
}

.code-language {
    font-size: var(--text-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
    padding: var(--space-1) var(--space-2);
    background: var(--bg-deep);
    border-radius: var(--radius-sm);
}

/* Copy button */
.copy-code-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: var(--space-xs) var(--space-sm);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    color: var(--text-muted);
    font-size: 0.75rem;
    cursor: pointer;
    opacity: 0;
    transition: all 0.2s var(--ease-out-expo);
}

pre:hover .copy-code-btn {
    opacity: 1;
}

.copy-code-btn:hover {
    background: var(--bg-elevated);
    color: var(--text-primary);
    border-color: var(--border-hover);
}

.copy-code-btn.copied {
    color: var(--success);
    border-color: var(--success);
}

/* Inline code */
code:not(pre code) {
    padding: 2px 6px;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    font-size: 0.875em;
    color: var(--accent-tertiary);
}

/* Line numbers */
.line-numbers {
    counter-reset: line;
}
.line-numbers .line::before {
    counter-increment: line;
    content: counter(line);
    display: inline-block;
    width: 3ch;
    margin-right: var(--space-md);
    text-align: right;
    color: var(--text-disabled);
    user-select: none;
}
"#;

const TOOL_CALL_STYLES: &str = r#"
/* ============================================
   Tool Calls - Collapsible Sections
   ============================================ */
.tool-call {
    margin: var(--space-md) 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    overflow: hidden;
    transition: all 0.3s var(--ease-out-expo);
}

.tool-call:hover {
    border-color: var(--role-tool);
}

.tool-call-header {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    padding: var(--space-md);
    background: var(--bg-elevated);
    cursor: pointer;
    user-select: none;
    transition: background 0.2s var(--ease-out-expo);
    list-style: none;
}

.tool-call-header::-webkit-details-marker {
    display: none;
}

.tool-call-header:hover {
    background: var(--role-tool-bg);
}

.tool-call-icon {
    width: 24px;
    height: 24px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--role-tool-bg);
    border-radius: var(--radius-sm);
    color: var(--role-tool);
    font-size: 0.875rem;
}

.tool-call-name {
    font-weight: 600;
    font-size: 0.875rem;
    color: var(--role-tool);
}

.tool-call-status {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    padding: 2px 8px;
    border-radius: var(--radius-full);
    font-size: 0.75rem;
    font-weight: 500;
}

.tool-call-status.success {
    background: var(--success-bg);
    color: var(--success);
}

.tool-call-status.error {
    background: var(--error-bg);
    color: var(--error);
}

.tool-call-toggle {
    color: var(--text-muted);
    transition: transform 0.3s var(--ease-out-expo);
}

details[open] .tool-call-toggle {
    transform: rotate(180deg);
}

.tool-call-body {
    padding: var(--space-md);
    border-top: 1px solid var(--border);
    background: var(--bg-surface);
    animation: toolBodySlideIn 0.3s var(--ease-out-expo);
}

@keyframes toolBodySlideIn {
    from {
        opacity: 0;
        transform: translateY(-8px);
    }
}

.tool-input, .tool-output {
    margin-bottom: var(--space-md);
}

.tool-input:last-child, .tool-output:last-child {
    margin-bottom: 0;
}

.tool-label {
    display: block;
    font-size: 0.75rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
    margin-bottom: var(--space-xs);
}
"#;

const ANIMATIONS: &str = r#"
/* ============================================
   Animations - Stripe-level Micro-interactions
   Purposeful, subtle, buttery smooth
   ============================================ */

/* --- Core Keyframes --- */
@keyframes fadeIn {
    from { opacity: 0; }
    to { opacity: 1; }
}

@keyframes fadeOut {
    from { opacity: 1; }
    to { opacity: 0; }
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

@keyframes slideDown {
    from {
        opacity: 0;
        transform: translateY(-20px);
    }
    to {
        opacity: 1;
        transform: translateY(0);
    }
}

@keyframes slideInRight {
    from {
        opacity: 0;
        transform: translateX(16px);
    }
    to {
        opacity: 1;
        transform: translateX(0);
    }
}

@keyframes slideInLeft {
    from {
        opacity: 0;
        transform: translateX(-16px);
    }
    to {
        opacity: 1;
        transform: translateX(0);
    }
}

@keyframes scaleIn {
    from {
        opacity: 0;
        transform: scale(0.95);
    }
    to {
        opacity: 1;
        transform: scale(1);
    }
}

@keyframes scaleInBounce {
    0% {
        opacity: 0;
        transform: scale(0.9);
    }
    60% {
        transform: scale(1.02);
    }
    100% {
        opacity: 1;
        transform: scale(1);
    }
}

@keyframes popIn {
    0% {
        opacity: 0;
        transform: scale(0.8);
    }
    50% {
        transform: scale(1.05);
    }
    100% {
        opacity: 1;
        transform: scale(1);
    }
}

/* --- Loading & Progress --- */
@keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.5; }
}

@keyframes pulseSubtle {
    0%, 100% { opacity: 0.8; }
    50% { opacity: 0.6; }
}

@keyframes shimmer {
    0% { background-position: -200% 0; }
    100% { background-position: 200% 0; }
}

@keyframes spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
}

@keyframes ellipsis {
    0% { content: '.'; }
    33% { content: '..'; }
    66% { content: '...'; }
}

/* --- Attention & Highlight --- */
@keyframes glow {
    0%, 100% {
        box-shadow: 0 0 8px rgba(122, 162, 247, 0.3);
    }
    50% {
        box-shadow: 0 0 20px rgba(122, 162, 247, 0.5);
    }
}

@keyframes highlightFade {
    0% {
        background: var(--accent-muted);
    }
    100% {
        background: transparent;
    }
}

@keyframes borderPulse {
    0%, 100% {
        border-color: var(--accent);
        box-shadow: var(--shadow-ring);
    }
    50% {
        border-color: var(--accent-hover);
        box-shadow: 0 0 0 4px rgba(122, 162, 247, 0.2);
    }
}

/* --- Utility Classes --- */
.animate-fade-in { animation: fadeIn 0.3s var(--ease-out-expo); }
.animate-fade-out { animation: fadeOut 0.3s var(--ease-out-expo); }
.animate-slide-up { animation: slideUp 0.4s var(--ease-out-expo); }
.animate-slide-down { animation: slideDown 0.4s var(--ease-out-expo); }
.animate-slide-in-right { animation: slideInRight 0.3s var(--ease-out-expo); }
.animate-slide-in-left { animation: slideInLeft 0.3s var(--ease-out-expo); }
.animate-scale-in { animation: scaleIn 0.3s var(--ease-out-expo); }
.animate-scale-in-bounce { animation: scaleInBounce 0.4s var(--ease-out-expo); }
.animate-pop-in { animation: popIn 0.35s var(--spring); }
.animate-pulse { animation: pulse 2s infinite; }
.animate-pulse-subtle { animation: pulseSubtle 3s infinite; }
.animate-spin { animation: spin 1s linear infinite; }
.animate-glow { animation: glow 2s infinite; }

/* Loading skeleton - elegant shimmer */
.skeleton {
    background: linear-gradient(
        90deg,
        var(--bg-elevated) 0%,
        rgba(122, 162, 247, 0.05) 50%,
        var(--bg-elevated) 100%
    );
    background-size: 200% 100%;
    animation: shimmer 1.5s ease-in-out infinite;
    border-radius: var(--radius-sm);
}

/* Success checkmark animation */
@keyframes checkmarkDraw {
    0% {
        stroke-dashoffset: 24;
    }
    100% {
        stroke-dashoffset: 0;
    }
}

.animate-checkmark {
    stroke-dasharray: 24;
    stroke-dashoffset: 24;
    animation: checkmarkDraw 0.4s var(--ease-out-expo) forwards;
}

/* Copy success feedback */
@keyframes copySuccess {
    0% {
        transform: scale(1);
    }
    50% {
        transform: scale(1.2);
        color: var(--success);
    }
    100% {
        transform: scale(1);
        color: var(--success);
    }
}

.copy-success {
    animation: copySuccess 0.4s var(--ease-out-expo);
}

/* Toast entrance */
@keyframes toastSlideIn {
    from {
        opacity: 0;
        transform: translateY(16px) scale(0.95);
    }
    to {
        opacity: 1;
        transform: translateY(0) scale(1);
    }
}

@keyframes toastSlideOut {
    from {
        opacity: 1;
        transform: translateY(0) scale(1);
    }
    to {
        opacity: 0;
        transform: translateY(-8px) scale(0.95);
    }
}

.toast-enter {
    animation: toastSlideIn 0.3s var(--ease-out-expo);
}

.toast-exit {
    animation: toastSlideOut 0.2s var(--ease-out-expo);
}

/* Stagger delay utilities */
.delay-75 { animation-delay: 75ms; }
.delay-100 { animation-delay: 100ms; }
.delay-150 { animation-delay: 150ms; }
.delay-200 { animation-delay: 200ms; }
.delay-300 { animation-delay: 300ms; }
.delay-500 { animation-delay: 500ms; }

/* Fill mode utilities */
.fill-forwards { animation-fill-mode: forwards; }
.fill-backwards { animation-fill-mode: backwards; }
.fill-both { animation-fill-mode: both; }

/* Transition utilities */
.transition-none { transition: none; }
.transition-fast { transition: all var(--duration-fast) var(--ease-out-expo); }
.transition-normal { transition: all var(--duration-normal) var(--ease-out-expo); }
.transition-slow { transition: all var(--duration-slow) var(--ease-out-expo); }
"#;

const SEARCH_STYLES: &str = r#"
/* ============================================
   Search - Stripe-level Find Experience
   ============================================ */
.search-container {
    position: relative;
    flex: 1;
    max-width: 320px;
}

#search-input {
    width: 100%;
    height: 40px;
    padding: 0 var(--space-md);
    padding-left: 40px;
    padding-right: 80px;
    background: var(--bg-elevated);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-full);
    color: var(--text-primary);
    font-size: var(--text-sm);
    transition: all var(--duration-fast) var(--ease-out-expo);
}

#search-input:hover {
    border-color: var(--border);
    background: var(--bg-surface);
}

#search-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: var(--shadow-ring);
    background: var(--bg-surface);
}

#search-input::placeholder {
    color: var(--text-muted);
    transition: color var(--duration-fast) var(--ease-out-expo);
}

#search-input:focus::placeholder {
    color: var(--text-disabled);
}

/* Search icon with subtle animation */
.search-icon {
    position: absolute;
    left: 14px;
    top: 50%;
    transform: translateY(-50%);
    width: 16px;
    height: 16px;
    color: var(--text-muted);
    pointer-events: none;
    transition: color var(--duration-fast) var(--ease-out-expo),
                transform var(--duration-fast) var(--ease-out-expo);
}

#search-input:focus ~ .search-icon {
    color: var(--accent);
    transform: translateY(-50%) scale(1.05);
}

/* Result count badge */
#search-count {
    position: absolute;
    right: 14px;
    top: 50%;
    transform: translateY(-50%);
    font-size: var(--text-xs);
    font-weight: 500;
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
    padding: var(--space-1) var(--space-2);
    background: var(--bg-deep);
    border-radius: var(--radius-sm);
    animation: fadeIn var(--duration-fast) var(--ease-out-expo);
}

#search-count:empty {
    display: none;
}

/* Search highlight with elegant glow */
.search-highlight {
    background: linear-gradient(
        135deg,
        rgba(224, 175, 104, 0.25) 0%,
        rgba(224, 175, 104, 0.35) 100%
    );
    border-radius: 3px;
    padding: 1px 3px;
    margin: -1px -3px;
    transition: all var(--duration-fast) var(--ease-out-expo);
}

/* Current match with prominent styling */
.search-current {
    background: linear-gradient(
        135deg,
        rgba(224, 175, 104, 0.5) 0%,
        rgba(224, 175, 104, 0.7) 100%
    );
    box-shadow: 0 0 0 2px var(--warning),
                0 0 12px rgba(224, 175, 104, 0.4);
    animation: highlightPulse 1.5s ease-in-out infinite;
}

@keyframes highlightPulse {
    0%, 100% {
        box-shadow: 0 0 0 2px var(--warning),
                    0 0 12px rgba(224, 175, 104, 0.4);
    }
    50% {
        box-shadow: 0 0 0 3px var(--warning),
                    0 0 20px rgba(224, 175, 104, 0.6);
    }
}

/* Navigation buttons */
.search-nav {
    display: flex;
    gap: 4px;
    margin-left: var(--space-sm);
}

.search-nav-btn {
    width: 32px;
    height: 32px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-elevated);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    color: var(--text-muted);
    cursor: pointer;
    transition: all var(--duration-fast) var(--ease-out-expo);
}

.search-nav-btn:hover {
    background: var(--bg-surface);
    border-color: var(--border);
    color: var(--text-primary);
    transform: translateY(-1px);
}

.search-nav-btn:active {
    transform: translateY(0) scale(0.95);
}

.search-nav-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
    transform: none;
}

.search-nav-btn .icon {
    width: 14px;
    height: 14px;
}

/* No results state */
.search-no-results #search-count {
    color: var(--error);
    background: var(--error-bg);
}

/* Clear button */
.search-clear {
    position: absolute;
    right: 48px;
    top: 50%;
    transform: translateY(-50%);
    width: 20px;
    height: 20px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-deep);
    border: none;
    border-radius: var(--radius-full);
    color: var(--text-muted);
    cursor: pointer;
    opacity: 0;
    transition: all var(--duration-fast) var(--ease-out-expo);
}

#search-input:not(:placeholder-shown) ~ .search-clear {
    opacity: 1;
}

.search-clear:hover {
    background: var(--error-bg);
    color: var(--error);
}

/* Mobile search optimizations */
@media (max-width: 767px) {
    .search-container {
        max-width: none;
        flex: 1;
    }

    #search-input {
        height: 44px;
        font-size: 16px; /* Prevents iOS zoom on focus */
        border-radius: var(--radius-lg);
    }
}
"#;

const THEME_TOGGLE_STYLES: &str = r#"
/* ============================================
   Theme Toggle - Light/Dark Mode
   ============================================ */
.theme-toggle {
    position: relative;
    width: 40px;
    height: 40px;
    border-radius: var(--radius-md);
    overflow: hidden;
}

.theme-toggle .icon-sun,
.theme-toggle .icon-moon {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: all 0.3s var(--ease-out-expo);
}

.theme-toggle .icon-sun svg,
.theme-toggle .icon-moon svg {
    width: 20px;
    height: 20px;
}

[data-theme="dark"] .icon-sun {
    transform: rotate(90deg) scale(0);
    opacity: 0;
}
[data-theme="dark"] .icon-moon {
    transform: rotate(0) scale(1);
    opacity: 1;
}

[data-theme="light"] .icon-sun {
    transform: rotate(0) scale(1);
    opacity: 1;
}
[data-theme="light"] .icon-moon {
    transform: rotate(-90deg) scale(0);
    opacity: 0;
}

/* Light theme overrides */
[data-theme="light"] {
    --bg-deep: #f8f9fc;
    --bg-surface: #ffffff;
    --bg-elevated: #f0f1f5;
    --bg-overlay: rgba(248, 249, 252, 0.95);
    --bg-frosted: rgba(255, 255, 255, 0.85);
    --border: #e1e4eb;
    --border-subtle: rgba(225, 228, 235, 0.6);
    --border-hover: #c8cdd8;
    --border-focus: #7aa2f7;
    --text-primary: #1a1b26;
    --text-secondary: #3d4259;
    --text-muted: #6b7394;
    --text-disabled: #9da3be;
    --text-inverse: #ffffff;
    --accent-muted: rgba(122, 162, 247, 0.12);
    --role-user-bg: #f0f5f3;
    --role-user-glow: rgba(158, 206, 106, 0.08);
    --role-user-border: rgba(158, 206, 106, 0.2);
    --role-agent-bg: #f0f2f8;
    --role-agent-glow: rgba(122, 162, 247, 0.08);
    --role-agent-border: rgba(122, 162, 247, 0.2);
    --role-tool-bg: #f8f5f0;
    --role-tool-glow: rgba(255, 158, 100, 0.08);
    --role-tool-border: rgba(255, 158, 100, 0.2);
    --role-system-bg: #f8f6f0;
    --role-system-glow: rgba(224, 175, 104, 0.08);
    --role-system-border: rgba(224, 175, 104, 0.2);
    --shadow-xs: 0 1px 2px rgba(0, 0, 0, 0.03);
    --shadow-sm: 0 1px 2px rgba(0, 0, 0, 0.05);
    --shadow-md: 0 4px 6px -1px rgba(0, 0, 0, 0.08), 0 2px 4px -1px rgba(0, 0, 0, 0.05);
    --shadow-lg: 0 10px 15px -3px rgba(0, 0, 0, 0.1), 0 4px 6px -2px rgba(0, 0, 0, 0.05);
    --shadow-glow: 0 0 20px rgba(122, 162, 247, 0.15);
}

/* Smooth theme transition */
html {
    transition: background-color 0.3s var(--ease-out-expo);
}

body, .message, .toolbar, .tool-call, pre {
    transition: background-color 0.3s var(--ease-out-expo),
                border-color 0.3s var(--ease-out-expo),
                color 0.3s var(--ease-out-expo);
}
"#;

const ENCRYPTION_STYLES: &str = r#"
/* ============================================
   Encryption - Password Modal
   ============================================ */
.modal {
    position: fixed;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-overlay);
    backdrop-filter: blur(8px);
    -webkit-backdrop-filter: blur(8px);
    z-index: var(--z-modal);
    animation: fadeIn 0.3s var(--ease-out-expo);
}

.modal[hidden] {
    display: none;
}

.modal-content {
    width: 90%;
    max-width: 400px;
    padding: var(--space-xl);
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-xl);
    box-shadow: var(--shadow-xl);
    text-align: center;
    animation: scaleIn 0.3s var(--ease-out-back);
}

.modal-icon {
    width: 64px;
    height: 64px;
    margin: 0 auto var(--space-lg);
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--accent);
    background: linear-gradient(135deg, var(--accent) 0%, var(--accent-secondary) 100%);
    border-radius: var(--radius-full);
    color: white;
}

.modal-icon svg {
    width: 32px;
    height: 32px;
}

.modal-title {
    margin: 0 0 var(--space-sm);
    font-size: 1.25rem;
    font-weight: 600;
    color: var(--text-primary);
}

.modal-text {
    margin: 0 0 var(--space-lg);
    color: var(--text-secondary);
    font-size: 0.875rem;
}

.modal-input {
    width: 100%;
    padding: var(--space-md);
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    color: var(--text-primary);
    font-size: 1rem;
    text-align: center;
    letter-spacing: 0.1em;
    margin-bottom: var(--space-md);
    transition: all 0.2s var(--ease-out-expo);
}

.modal-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px rgba(122, 162, 247, 0.15);
}

.modal-btn {
    width: 100%;
    padding: var(--space-md);
    background: var(--accent);
    background: linear-gradient(135deg, var(--accent) 0%, #6992e5 100%);
    border: none;
    border-radius: var(--radius-md);
    color: white;
    font-size: 1rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.2s var(--ease-out-expo);
}

.modal-btn:hover {
    transform: translateY(-1px);
    box-shadow: var(--shadow-lg), var(--shadow-glow);
}

.modal-btn:active {
    transform: translateY(0);
}

.modal-error {
    margin-top: var(--space-md);
    padding: var(--space-sm) var(--space-md);
    background: var(--error-bg);
    border-radius: var(--radius-md);
    color: var(--error);
    font-size: 0.875rem;
}

.encrypted-notice {
    text-align: center;
    padding: var(--space-xl);
    color: var(--text-muted);
}
"#;

const DESKTOP_ENHANCEMENTS: &str = r#"
/* ============================================
   Desktop Enhancements - Stripe-level Polish
   Optimized for mouse, keyboard, wide screens
   ============================================ */
@media (min-width: 768px) {
    /* Generous whitespace on larger screens */
    #app {
        padding: var(--space-2xl) var(--space-xl);
    }

    /* Elegant header grid layout */
    .header {
        display: grid;
        grid-template-columns: 1fr auto;
        gap: var(--space-lg);
        align-items: start;
    }

    .title {
        font-size: var(--text-4xl);
        letter-spacing: -0.03em;
    }

    /* Enhanced toolbar with more breathing room */
    .toolbar {
        padding: var(--space-md) var(--space-lg);
        gap: var(--space-md);
    }

    /* Wide code blocks - break out of container elegantly */
    pre {
        margin-left: calc(-1 * var(--space-lg));
        margin-right: calc(-1 * var(--space-lg));
        padding-left: var(--space-lg);
        padding-right: var(--space-lg);
    }

    /* Elegant keyboard shortcut hints */
    .kbd-hint {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        margin-left: var(--space-sm);
        font-size: var(--text-xs);
        color: var(--text-muted);
    }

    .kbd {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        min-width: 22px;
        height: 22px;
        padding: 0 6px;
        background: linear-gradient(to bottom, var(--bg-elevated), var(--bg-surface));
        border: 1px solid var(--border);
        border-bottom-width: 2px;
        border-radius: var(--radius-sm);
        font-family: inherit;
        font-size: 0.6875rem;
        font-weight: 500;
        color: var(--text-secondary);
        box-shadow: 0 1px 2px rgba(0, 0, 0, 0.1);
    }

    /* Message hover states - subtle lift effect */
    .message {
        transition: transform var(--duration-normal) var(--ease-out-expo),
                    box-shadow var(--duration-normal) var(--ease-out-expo),
                    border-color var(--duration-fast) var(--ease-out-expo);
    }

    .message:hover {
        transform: translateY(-2px);
        box-shadow: var(--shadow-lg);
        border-color: var(--border-hover);
    }

    /* Message action buttons - appear on hover */
    .message-actions {
        position: absolute;
        top: var(--space-3);
        right: var(--space-3);
        display: flex;
        gap: var(--space-2);
        opacity: 0;
        transform: translateY(-4px);
        transition: opacity var(--duration-fast) var(--ease-out-expo),
                    transform var(--duration-fast) var(--ease-out-expo);
        pointer-events: none;
    }

    .message:hover .message-actions {
        opacity: 1;
        transform: translateY(0);
        pointer-events: auto;
    }

    .message-action-btn {
        width: 30px;
        height: 30px;
        display: flex;
        align-items: center;
        justify-content: center;
        background: var(--bg-frosted);
        backdrop-filter: blur(8px);
        -webkit-backdrop-filter: blur(8px);
        border: 1px solid var(--border-subtle);
        border-radius: var(--radius-md);
        color: var(--text-muted);
        cursor: pointer;
        transition: all var(--duration-fast) var(--ease-out-expo);
    }

    .message-action-btn:hover {
        background: var(--bg-elevated);
        border-color: var(--border);
        color: var(--text-primary);
        transform: scale(1.05);
    }

    .message-action-btn:active {
        transform: scale(0.95);
    }

    .message-action-btn .icon {
        width: 14px;
        height: 14px;
    }

    /* Enhanced tool call expansion on desktop */
    .tool-call {
        transition: all var(--duration-normal) var(--ease-out-expo);
    }

    .tool-call:hover {
        border-color: var(--role-tool-border);
    }

    .tool-call.expanded {
        background: var(--bg-elevated);
    }

    /* Code block copy button - elegant reveal */
    .copy-code-btn {
        opacity: 0;
        transform: translateY(-2px) scale(0.95);
        transition: opacity var(--duration-fast) var(--ease-out-expo),
                    transform var(--duration-fast) var(--ease-out-expo),
                    background var(--duration-fast) var(--ease-out-expo);
    }

    pre:hover .copy-code-btn {
        opacity: 1;
        transform: translateY(0) scale(1);
    }

    /* Refined focus states for keyboard navigation */
    .toolbar-btn:focus-visible,
    .message-action-btn:focus-visible {
        outline: none;
        box-shadow: var(--shadow-ring);
    }

    /* Enhanced tooltip animations */
    .toolbar-btn[data-tooltip]::after {
        transition: opacity var(--duration-fast) var(--ease-out-expo),
                    transform var(--duration-fast) var(--ease-out-expo);
        transform: translateX(-50%) translateY(6px);
    }

    .toolbar-btn[data-tooltip]:hover::after {
        transform: translateX(-50%) translateY(0);
    }
}

/* Large desktop - refined typography and spacing */
@media (min-width: 1024px) {
    #app {
        max-width: 1000px;
        padding: var(--space-3xl) var(--space-2xl);
    }

    /* Meta items flow to the right on wide screens */
    .meta {
        justify-content: flex-end;
    }

    /* More generous message padding */
    .message {
        padding: var(--space-8);
    }

    /* Larger code blocks with more context */
    pre code {
        max-height: 500px;
    }
}

/* Extra large desktop - optimal reading width */
@media (min-width: 1280px) {
    #app {
        max-width: 1100px;
    }

    /* Subtle side gutters for visual balance */
    .conversation {
        padding: 0 var(--space-4);
    }
}

/* Ultra-wide screens - centered with max readability */
@media (min-width: 1536px) {
    #app {
        max-width: 1200px;
    }

    .title {
        font-size: 2.5rem;
    }

    .message {
        padding: var(--space-10);
    }
}
"#;

const MOBILE_OPTIMIZATIONS: &str = r#"
/* ============================================
   Mobile Optimizations - World-class Touch UX
   Touch-first, thumb-friendly, performant
   ============================================ */
@media (max-width: 767px) {
    /* Slightly smaller base font for mobile density */
    html {
        font-size: 15px;
    }

    /* Compact but breathable layout */
    #app {
        padding: var(--space-md);
        padding-top: var(--space-lg);
    }

    /* Mobile-optimized title */
    .title {
        font-size: var(--text-2xl);
        line-height: 1.2;
        margin-bottom: var(--space-3);
    }

    /* Stack meta items vertically */
    .meta {
        flex-direction: column;
        align-items: flex-start;
        gap: var(--space-2);
    }

    .meta-item {
        font-size: var(--text-sm);
    }

    /* Elegant floating toolbar with glassmorphism */
    .toolbar {
        position: fixed;
        bottom: 0;
        left: 0;
        right: 0;
        top: auto;
        margin: 0;
        border-radius: var(--radius-2xl) var(--radius-2xl) 0 0;
        border-bottom: none;
        box-shadow: 0 -4px 24px rgba(0, 0, 0, 0.3),
                    0 -1px 8px rgba(0, 0, 0, 0.2);
        justify-content: center;
        z-index: var(--z-sticky);
        background: var(--bg-frosted);
        backdrop-filter: blur(20px) saturate(1.2);
        -webkit-backdrop-filter: blur(20px) saturate(1.2);
        padding: var(--space-3) var(--space-4);
        padding-bottom: var(--space-3);
    }

    /* Handle notch/home indicator */
    @supports (padding-bottom: env(safe-area-inset-bottom)) {
        .toolbar {
            padding-bottom: calc(var(--space-3) + env(safe-area-inset-bottom));
        }
    }

    /* Bottom padding for content above toolbar */
    .conversation {
        padding-bottom: calc(80px + env(safe-area-inset-bottom, 0px));
    }

    /* Apple HIG: 44px minimum touch targets */
    .toolbar-btn {
        width: 48px;
        height: 48px;
        border-radius: var(--radius-lg);
    }

    .toolbar-btn .icon {
        width: 24px;
        height: 24px;
    }

    /* Haptic feedback simulation via scale */
    .toolbar-btn:active {
        transform: scale(0.92);
        transition: transform 0.1s ease-out;
    }

    /* Stack toolbar items for smaller screens */
    .toolbar-group {
        flex-wrap: wrap;
        justify-content: center;
        gap: var(--space-2);
    }

    .toolbar-divider {
        display: none;
    }

    /* Messages - compact but readable */
    .message {
        padding: var(--space-4);
        border-radius: var(--radius-lg);
        margin-left: calc(-1 * var(--space-2));
        margin-right: calc(-1 * var(--space-2));
    }

    /* Simpler animation on mobile for performance */
    .message {
        animation-duration: 0.3s;
    }

    /* Remove hover lift on touch devices */
    .message:hover {
        transform: none;
        box-shadow: var(--shadow-xs);
    }

    /* Header wraps on mobile */
    .message-header {
        flex-wrap: wrap;
        gap: var(--space-2);
    }

    .message-time {
        width: 100%;
        margin-top: var(--space-1);
        margin-left: 0;
        font-size: var(--text-xs);
    }

    /* Role icon slightly smaller */
    .role-icon {
        width: 28px;
        height: 28px;
        font-size: 0.75rem;
    }

    /* Full-bleed code blocks for more code real estate */
    pre {
        margin-left: calc(-1 * var(--space-4));
        margin-right: calc(-1 * var(--space-4));
        border-radius: 0;
        border-left: none;
        border-right: none;
    }

    pre code {
        padding: var(--space-4);
        font-size: 0.8125rem;
        line-height: 1.5;
        /* Limit height to prevent scroll fatigue */
        max-height: 300px;
    }

    /* Code header adjustments */
    .code-header {
        padding: var(--space-2) var(--space-4);
        margin: 0 calc(-1 * var(--space-4));
        border-radius: 0;
    }

    /* Always show copy button on mobile */
    .copy-code-btn {
        opacity: 1;
        width: 36px;
        height: 36px;
    }

    /* Tool calls - compact */
    .tool-call {
        margin-left: calc(-1 * var(--space-2));
        margin-right: calc(-1 * var(--space-2));
        border-radius: var(--radius-md);
    }

    .tool-call-header {
        padding: var(--space-3) var(--space-4);
    }

    .tool-call-body {
        padding: var(--space-3) var(--space-4);
    }

    /* Message actions - always visible on mobile */
    .message-actions {
        opacity: 1;
        transform: none;
        pointer-events: auto;
        position: relative;
        top: auto;
        right: auto;
        margin-top: var(--space-3);
        justify-content: flex-end;
    }

    .message-action-btn {
        width: 40px;
        height: 40px;
    }

    /* Hide keyboard hints - not applicable to touch */
    .kbd-hint {
        display: none !important;
    }

    /* Modal adjustments for mobile */
    .modal-content {
        width: 100%;
        max-width: none;
        margin: var(--space-4);
        padding: var(--space-6);
        border-radius: var(--radius-xl);
        max-height: calc(100vh - var(--space-8));
        overflow-y: auto;
    }

    .modal-title {
        font-size: var(--text-xl);
    }

    .modal-input {
        height: 52px;
        font-size: 16px; /* Prevent iOS zoom */
    }

    .modal-btn {
        height: 52px;
    }
}

/* Very small phones - even more compact */
@media (max-width: 374px) {
    #app {
        padding: var(--space-3);
    }

    .title {
        font-size: var(--text-xl);
    }

    .message {
        padding: var(--space-3);
    }

    .toolbar-btn {
        width: 44px;
        height: 44px;
    }
}

/* Landscape phone optimization */
@media (max-width: 767px) and (orientation: landscape) {
    /* More horizontal space, less vertical */
    #app {
        padding: var(--space-sm) var(--space-lg);
    }

    /* Toolbar becomes more compact */
    .toolbar {
        padding: var(--space-2) var(--space-4);
    }

    .toolbar-btn {
        width: 40px;
        height: 40px;
    }

    /* Reduce vertical spacing */
    .conversation {
        gap: var(--space-3);
        padding-bottom: 60px;
    }

    .message {
        padding: var(--space-3) var(--space-4);
    }
}

/* Touch device optimization - no hover states */
@media (hover: none) and (pointer: coarse) {
    /* Remove hover effects that feel wrong on touch */
    .toolbar-btn:hover::before {
        opacity: 0;
    }

    .toolbar-btn:hover {
        background: transparent;
        border-color: transparent;
    }

    /* Active states for touch feedback */
    .toolbar-btn:active {
        background: var(--accent-muted);
    }

    .message:hover {
        transform: none;
        box-shadow: var(--shadow-xs);
        border-color: var(--border-subtle);
    }

    /* Touch-tap highlight */
    .tool-call-header {
        -webkit-tap-highlight-color: var(--accent-muted);
    }

    /* Always visible interactive elements */
    .copy-code-btn,
    .message-actions {
        opacity: 1;
    }
}

/* Safe area handling for notched devices (iPhone X+, etc.) */
@supports (padding: max(0px)) {
    @media (max-width: 767px) {
        #app {
            padding-left: max(var(--space-md), env(safe-area-inset-left));
            padding-right: max(var(--space-md), env(safe-area-inset-right));
        }

        .toolbar {
            padding-left: max(var(--space-4), env(safe-area-inset-left));
            padding-right: max(var(--space-4), env(safe-area-inset-right));
        }
    }
}

/* iOS-specific optimizations */
@supports (-webkit-touch-callout: none) {
    /* Smooth scrolling momentum */
    .conversation,
    .modal-content,
    pre code {
        -webkit-overflow-scrolling: touch;
    }

    /* Prevent text selection during scroll */
    .message-content {
        -webkit-user-select: text;
    }
}
"#;

const ACCESSIBILITY: &str = r#"
/* ============================================
   Accessibility
   ============================================ */
/* Focus visible for keyboard navigation */
:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
}

/* Remove focus outline for mouse users */
:focus:not(:focus-visible) {
    outline: none;
}

/* Skip link */
.skip-link {
    position: absolute;
    top: -100%;
    left: var(--space-md);
    padding: var(--space-sm) var(--space-md);
    background: var(--accent);
    color: white;
    border-radius: var(--radius-md);
    z-index: 9999;
    transition: top 0.2s var(--ease-out-expo);
}

.skip-link:focus {
    top: var(--space-md);
}

/* Reduced motion */
@media (prefers-reduced-motion: reduce) {
    *, *::before, *::after {
        animation-duration: 0.01ms !important;
        animation-iteration-count: 1 !important;
        transition-duration: 0.01ms !important;
        scroll-behavior: auto !important;
    }
}

/* High contrast mode */
@media (prefers-contrast: high) {
    :root {
        --border: #ffffff;
        --text-primary: #ffffff;
        --text-secondary: #ffffff;
    }

    .message {
        border-width: 2px;
    }

    .message::before {
        width: 4px;
    }
}

/* Screen reader only */
.sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
}
"#;

const WORLD_CLASS_ENHANCEMENTS: &str = r#"
/* ============================================
   World-Class UI/UX Enhancements
   Stripe-level polish that makes users gasp
   ============================================ */

/* --- Scroll Progress Indicator --- */
.scroll-progress {
    position: fixed;
    top: 0;
    left: 0;
    width: 0%;
    height: 3px;
    background: linear-gradient(90deg,
        var(--accent) 0%,
        var(--accent-secondary) 50%,
        var(--accent-tertiary) 100%);
    z-index: var(--z-toast);
    transition: width 50ms linear;
    box-shadow: 0 0 10px var(--accent), 0 0 5px var(--accent);
    border-radius: 0 var(--radius-full) var(--radius-full) 0;
}

/* --- Gradient Mesh Background --- */
.gradient-mesh {
    position: fixed;
    inset: 0;
    z-index: -1;
    pointer-events: none;
    overflow: hidden;
}

.gradient-mesh::before {
    content: '';
    position: absolute;
    top: -50%;
    left: -50%;
    width: 200%;
    height: 200%;
    background:
        radial-gradient(ellipse 600px 400px at 20% 30%, rgba(122, 162, 247, 0.08), transparent),
        radial-gradient(ellipse 500px 300px at 80% 20%, rgba(187, 154, 247, 0.06), transparent),
        radial-gradient(ellipse 400px 250px at 60% 80%, rgba(115, 218, 202, 0.05), transparent);
    animation: meshFloat 30s ease-in-out infinite;
}

@keyframes meshFloat {
    0%, 100% { transform: translate(0, 0) rotate(0deg); }
    25% { transform: translate(2%, 1%) rotate(1deg); }
    50% { transform: translate(-1%, 2%) rotate(-1deg); }
    75% { transform: translate(1%, -1%) rotate(0.5deg); }
}

[data-theme="light"] .gradient-mesh::before {
    background:
        radial-gradient(ellipse 600px 400px at 20% 30%, rgba(122, 162, 247, 0.06), transparent),
        radial-gradient(ellipse 500px 300px at 80% 20%, rgba(187, 154, 247, 0.04), transparent),
        radial-gradient(ellipse 400px 250px at 60% 80%, rgba(115, 218, 202, 0.03), transparent);
}

/* --- Floating Navigation Buttons --- */
.floating-nav {
    position: fixed;
    right: var(--space-4);
    bottom: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    z-index: var(--z-sticky);
    opacity: 0;
    transform: translateY(16px) scale(0.9);
    transition: opacity var(--duration-normal) var(--ease-out-expo),
                transform var(--duration-normal) var(--ease-out-expo);
    pointer-events: none;
}

.floating-nav.visible {
    opacity: 1;
    transform: translateY(0) scale(1);
    pointer-events: auto;
}

.floating-btn {
    width: 44px;
    height: 44px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-frosted);
    backdrop-filter: blur(12px) saturate(1.5);
    -webkit-backdrop-filter: blur(12px) saturate(1.5);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-full);
    color: var(--text-secondary);
    cursor: pointer;
    box-shadow: var(--shadow-lg);
    transition: all var(--duration-fast) var(--ease-out-expo);
}

.floating-btn:hover {
    background: var(--bg-surface);
    border-color: var(--accent);
    color: var(--accent);
    transform: scale(1.08);
    box-shadow: var(--shadow-xl), var(--shadow-glow);
}

.floating-btn:active {
    transform: scale(0.95);
}

.floating-btn .icon {
    width: 20px;
    height: 20px;
}

/* --- Message Link Copy --- */
.message-link-btn {
    position: absolute;
    top: var(--space-4);
    right: var(--space-4);
    width: 32px;
    height: 32px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-frosted);
    backdrop-filter: blur(8px);
    -webkit-backdrop-filter: blur(8px);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    color: var(--text-muted);
    cursor: pointer;
    opacity: 0;
    transform: translateY(-4px);
    transition: all var(--duration-fast) var(--ease-out-expo);
}

.message:hover .message-link-btn {
    opacity: 1;
    transform: translateY(0);
}

.message-link-btn:hover {
    background: var(--bg-elevated);
    border-color: var(--accent);
    color: var(--accent);
}

.message-link-btn.copied {
    background: var(--success-bg);
    border-color: var(--success);
    color: var(--success);
    animation: copySuccess 0.5s var(--ease-out-expo);
}

/* --- Message Intersection Animations --- */
.message.in-view {
    animation: messageReveal 0.6s var(--ease-out-expo) forwards;
}

@keyframes messageReveal {
    from {
        opacity: 0;
        transform: translateY(24px) scale(0.97);
    }
    to {
        opacity: 1;
        transform: translateY(0) scale(1);
    }
}

.message.in-view::after {
    animation: glowPulse 1.5s var(--ease-out-expo) forwards;
}

@keyframes glowPulse {
    0% { opacity: 0.8; }
    100% { opacity: 0; }
}

/* --- Keyboard Navigation Indicator --- */
.message.keyboard-focus {
    outline: none;
    box-shadow: var(--shadow-ring), var(--shadow-md);
    border-color: var(--accent);
}

.message.keyboard-focus::before {
    opacity: 1;
    width: 4px;
    background: var(--accent);
}

/* --- Enhanced Code Block Interactions --- */
pre:hover code {
    background: linear-gradient(180deg,
        rgba(122, 162, 247, 0.02) 0%,
        transparent 100%);
}

/* Line highlighting on hover (when JS adds line spans) */
pre code .line:hover {
    background: rgba(122, 162, 247, 0.08);
    border-radius: 2px;
    margin: 0 calc(-1 * var(--space-2));
    padding: 0 var(--space-2);
}

/* Language badge floating in corner */
pre[data-language]::before {
    content: attr(data-language);
    position: absolute;
    top: var(--space-2);
    left: var(--space-3);
    padding: 2px 8px;
    background: var(--bg-surface);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    font-size: 0.625rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
    z-index: 1;
}

/* --- Reading Time Badge --- */
.reading-time {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 2px 10px;
    background: var(--bg-elevated);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-full);
    font-size: var(--text-xs);
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
}

.reading-time .icon {
    width: 12px;
    height: 12px;
    opacity: 0.7;
}

/* --- Share Button --- */
.share-btn {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    color: var(--text-secondary);
    font-size: var(--text-sm);
    font-weight: 500;
    cursor: pointer;
    transition: all var(--duration-fast) var(--ease-out-expo);
}

.share-btn:hover {
    background: var(--accent-muted);
    border-color: var(--accent);
    color: var(--accent);
}

.share-btn .icon {
    width: 16px;
    height: 16px;
}

/* --- Enhanced Toast Styling --- */
.toast {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-xl);
    font-size: var(--text-sm);
    color: var(--text-primary);
}

.toast-success {
    border-color: var(--success-border);
    background: linear-gradient(135deg, var(--bg-surface), var(--success-bg));
}

.toast-success::before {
    content: '✓';
    display: flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    background: var(--success);
    border-radius: var(--radius-full);
    color: white;
    font-size: 12px;
    font-weight: bold;
}

.toast-error {
    border-color: var(--error-border);
    background: linear-gradient(135deg, var(--bg-surface), var(--error-bg));
}

.toast-error::before {
    content: '✕';
    display: flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    background: var(--error);
    border-radius: var(--radius-full);
    color: white;
    font-size: 12px;
    font-weight: bold;
}

/* --- Current Message Indicator --- */
.message-indicator {
    position: fixed;
    left: 0;
    top: 50%;
    transform: translateY(-50%);
    width: 4px;
    height: 60px;
    background: var(--accent);
    border-radius: 0 var(--radius-full) var(--radius-full) 0;
    opacity: 0;
    transition: opacity var(--duration-fast) var(--ease-out-expo),
                top var(--duration-fast) var(--ease-out-expo);
    z-index: var(--z-raised);
}

.message-indicator.visible {
    opacity: 1;
}

/* --- Keyboard Shortcuts Hint Panel --- */
.shortcuts-hint {
    position: fixed;
    bottom: var(--space-4);
    left: var(--space-4);
    padding: var(--space-2) var(--space-3);
    background: var(--bg-frosted);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    font-size: var(--text-xs);
    color: var(--text-muted);
    opacity: 0;
    transform: translateY(8px);
    transition: all var(--duration-fast) var(--ease-out-expo);
    z-index: var(--z-raised);
}

.shortcuts-hint.visible {
    opacity: 1;
    transform: translateY(0);
}

.shortcuts-hint kbd {
    display: inline-block;
    min-width: 18px;
    padding: 2px 5px;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-bottom-width: 2px;
    border-radius: 4px;
    font-family: inherit;
    font-size: 0.6875rem;
    text-align: center;
}
"#;

const MOBILE_WORLD_CLASS: &str = r#"
/* ============================================
   Mobile World-Class Enhancements
   Touch-first experiences that delight
   ============================================ */
@media (max-width: 767px) {
    /* --- Scroll-Aware Toolbar --- */
    .toolbar {
        transition: transform var(--duration-normal) var(--ease-out-expo),
                    opacity var(--duration-normal) var(--ease-out-expo);
    }

    .toolbar.toolbar-hidden {
        transform: translateY(calc(100% + 20px));
        opacity: 0;
        pointer-events: none;
    }

    /* --- Floating Navigation on Mobile --- */
    .floating-nav {
        right: var(--space-3);
        bottom: calc(90px + env(safe-area-inset-bottom, 0px));
    }

    .floating-btn {
        width: 48px;
        height: 48px;
        box-shadow: var(--shadow-xl);
    }

    /* --- Pull to Refresh Visual --- */
    .pull-indicator {
        position: fixed;
        top: -60px;
        left: 50%;
        transform: translateX(-50%);
        width: 40px;
        height: 40px;
        display: flex;
        align-items: center;
        justify-content: center;
        background: var(--bg-surface);
        border: 1px solid var(--border);
        border-radius: var(--radius-full);
        box-shadow: var(--shadow-lg);
        transition: top var(--duration-normal) var(--ease-out-expo);
        z-index: var(--z-sticky);
    }

    .pull-indicator.pulling {
        top: var(--space-4);
    }

    .pull-indicator .icon {
        width: 20px;
        height: 20px;
        color: var(--text-muted);
        transition: transform var(--duration-normal) var(--ease-out-expo);
    }

    .pull-indicator.ready .icon {
        transform: rotate(180deg);
        color: var(--accent);
    }

    /* --- Bottom Sheet Modal --- */
    .modal {
        align-items: flex-end;
    }

    .modal-content {
        width: 100%;
        max-width: none;
        margin: 0;
        border-radius: var(--radius-2xl) var(--radius-2xl) 0 0;
        max-height: 90vh;
        animation: bottomSheetSlideIn 0.4s var(--ease-out-expo);
    }

    @keyframes bottomSheetSlideIn {
        from {
            transform: translateY(100%);
            opacity: 0;
        }
        to {
            transform: translateY(0);
            opacity: 1;
        }
    }

    /* Bottom sheet drag handle */
    .modal-content::before {
        content: '';
        display: block;
        width: 36px;
        height: 4px;
        background: var(--border);
        border-radius: var(--radius-full);
        margin: 0 auto var(--space-4);
    }

    /* --- Reading Progress Bar on Mobile --- */
    .scroll-progress {
        height: 2px;
    }

    /* --- Share Button in Toolbar --- */
    .toolbar .share-btn {
        padding: var(--space-2);
        border: none;
        background: transparent;
    }

    .toolbar .share-btn span {
        display: none;
    }

    /* --- Message Link Button - Always Visible --- */
    .message-link-btn {
        opacity: 0.6;
        transform: translateY(0);
    }

    .message-link-btn:active {
        opacity: 1;
        transform: scale(0.92);
    }

    /* --- Swipe Hint on First Message --- */
    .swipe-hint {
        position: absolute;
        bottom: var(--space-3);
        left: 50%;
        transform: translateX(-50%);
        display: flex;
        align-items: center;
        gap: var(--space-2);
        padding: var(--space-2) var(--space-3);
        background: var(--bg-frosted);
        backdrop-filter: blur(8px);
        -webkit-backdrop-filter: blur(8px);
        border: 1px solid var(--border-subtle);
        border-radius: var(--radius-full);
        font-size: var(--text-xs);
        color: var(--text-muted);
        animation: swipeHintPulse 2s ease-in-out infinite;
    }

    @keyframes swipeHintPulse {
        0%, 100% { opacity: 0.8; transform: translateX(-50%); }
        50% { opacity: 1; transform: translateX(-50%) translateY(-2px); }
    }

    /* --- Touch Ripple Effect --- */
    .ripple {
        position: absolute;
        border-radius: 50%;
        background: var(--accent-muted);
        transform: scale(0);
        animation: rippleEffect 0.6s ease-out;
        pointer-events: none;
    }

    @keyframes rippleEffect {
        to {
            transform: scale(4);
            opacity: 0;
        }
    }

    /* --- Hide Keyboard Shortcuts Panel on Mobile --- */
    .shortcuts-hint {
        display: none !important;
    }

    /* --- Gradient Mesh - Simpler on Mobile --- */
    .gradient-mesh::before {
        animation: none;
        background:
            radial-gradient(ellipse 400px 300px at 30% 20%, rgba(122, 162, 247, 0.06), transparent),
            radial-gradient(ellipse 300px 200px at 70% 80%, rgba(115, 218, 202, 0.04), transparent);
    }
}

/* --- Touch Device Specific --- */
@media (hover: none) and (pointer: coarse) {
    /* Larger tap targets for links in messages */
    .message-content a {
        padding: var(--space-1) var(--space-2);
        margin: calc(-1 * var(--space-1)) calc(-1 * var(--space-2));
        border-radius: var(--radius-sm);
    }

    /* Active state ripple on buttons */
    .toolbar-btn,
    .floating-btn,
    .share-btn,
    .message-link-btn {
        position: relative;
        overflow: hidden;
    }

    /* Smoother scroll on iOS */
    .conversation {
        scroll-behavior: smooth;
        -webkit-overflow-scrolling: touch;
    }
}
"#;

const UTILITY_CLASSES: &str = r#"
/* ============================================
   Utility Classes
   ============================================ */
.hidden { display: none !important; }
.invisible { visibility: hidden !important; }
.opacity-0 { opacity: 0; }
.opacity-50 { opacity: 0.5; }
.opacity-100 { opacity: 1; }

.flex { display: flex; }
.flex-col { flex-direction: column; }
.items-center { align-items: center; }
.justify-center { justify-content: center; }
.justify-between { justify-content: space-between; }
.gap-xs { gap: var(--space-xs); }
.gap-sm { gap: var(--space-sm); }
.gap-md { gap: var(--space-md); }
.gap-lg { gap: var(--space-lg); }

.text-center { text-align: center; }
.text-left { text-align: left; }
.text-right { text-align: right; }

.font-semibold { font-weight: 600; }
.font-bold { font-weight: 700; }

.text-xs { font-size: 0.75rem; }
.text-sm { font-size: 0.875rem; }
.text-base { font-size: 1rem; }
.text-lg { font-size: 1.125rem; }
.text-xl { font-size: 1.25rem; }

.text-primary { color: var(--text-primary); }
.text-secondary { color: var(--text-secondary); }
.text-muted { color: var(--text-muted); }
.text-accent { color: var(--accent); }
.text-success { color: var(--success); }
.text-warning { color: var(--warning); }
.text-error { color: var(--error); }

.bg-deep { background: var(--bg-deep); }
.bg-surface { background: var(--bg-surface); }
.bg-elevated { background: var(--bg-elevated); }

.rounded { border-radius: var(--radius-md); }
.rounded-lg { border-radius: var(--radius-lg); }
.rounded-full { border-radius: var(--radius-full); }

.shadow { box-shadow: var(--shadow-md); }
.shadow-lg { box-shadow: var(--shadow-lg); }

.truncate {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}

.no-scrollbar {
    scrollbar-width: none;
    -ms-overflow-style: none;
}
.no-scrollbar::-webkit-scrollbar {
    display: none;
}
"#;

fn generate_print_css() -> String {
    r#"/* ============================================
   Print Styles - Clean PDF Output
   ============================================ */
@page {
    margin: 0.75in;
    size: auto;
}

@media print {
    * {
        -webkit-print-color-adjust: exact !important;
        print-color-adjust: exact !important;
    }

    html {
        font-size: 11pt;
    }

    body {
        background: white !important;
        color: #1a1b26 !important;
        line-height: 1.4;
    }

    #app {
        max-width: none;
        padding: 0;
    }

    /* Hide interactive elements */
    .toolbar,
    .theme-toggle,
    .copy-code-btn,
    .message-actions,
    .skip-link,
    #search-input,
    .search-nav,
    .kbd-hint {
        display: none !important;
    }

    /* Header styling */
    .header {
        border-bottom: 2px solid #1a1b26;
        margin-bottom: 1rem;
        padding-bottom: 0.5rem;
    }

    .title {
        font-size: 14pt;
        background: none;
        -webkit-text-fill-color: #1a1b26;
        color: #1a1b26 !important;
    }

    .agent-badge {
        background: none;
        border: 1px solid currentColor;
    }

    .meta {
        color: #666 !important;
    }

    /* Message styling */
    .message {
        background: none !important;
        border: 1px solid #ccc;
        border-left-width: 4px;
        page-break-inside: avoid;
        margin-bottom: 0.75rem;
        box-shadow: none !important;
    }

    .message::before {
        display: none;
    }

    .message-user {
        border-left-color: #4a7c4a !important;
    }

    .message-assistant, .message-agent {
        border-left-color: #4a6fa7 !important;
    }

    .message-tool {
        border-left-color: #a77a4a !important;
    }

    .message-system {
        border-left-color: #a79a4a !important;
    }

    .message-role {
        color: #1a1b26 !important;
    }

    .message-content {
        color: #333 !important;
    }

    /* Code blocks */
    pre {
        background: #f5f5f5 !important;
        border: 1px solid #ddd;
        font-size: 9pt;
        page-break-inside: avoid;
        box-shadow: none !important;
    }

    pre code {
        white-space: pre-wrap;
        word-wrap: break-word;
    }

    code:not(pre code) {
        background: #eee !important;
        border-color: #ddd !important;
        color: #333 !important;
    }

    /* Tool calls - expanded */
    .tool-call-body {
        display: block !important;
    }

    .tool-call-toggle {
        display: none;
    }

    /* Links */
    a {
        color: inherit;
        text-decoration: underline;
    }

    a[href^="http"]::after {
        content: " (" attr(href) ")";
        font-size: 0.8em;
        color: #666;
    }

    /* Page breaks */
    h1, h2, h3 {
        page-break-after: avoid;
    }

    .conversation {
        padding-bottom: 0;
    }
}
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_styles_includes_colors() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        assert!(bundle.critical_css.contains("--bg-deep:"));
        assert!(bundle.critical_css.contains("#1a1b26"));
        assert!(bundle.critical_css.contains("--role-user:"));
        assert!(bundle.critical_css.contains("#9ece6a"));
    }

    #[test]
    fn test_generate_styles_includes_search_when_enabled() {
        let opts = ExportOptions {
            include_search: true,
            ..Default::default()
        };
        let bundle = generate_styles(&opts);

        assert!(bundle.critical_css.contains("#search-input"));
        assert!(bundle.critical_css.contains(".search-highlight"));
    }

    #[test]
    fn test_generate_styles_excludes_search_when_disabled() {
        let opts = ExportOptions {
            include_search: false,
            ..Default::default()
        };
        let bundle = generate_styles(&opts);

        assert!(!bundle.critical_css.contains("#search-input"));
    }

    #[test]
    fn test_print_css_hides_interactive_elements() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        assert!(bundle.print_css.contains(".toolbar"));
        assert!(bundle.print_css.contains("display: none"));
        assert!(bundle.print_css.contains("@page"));
    }

    #[test]
    fn test_styles_include_animations() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        assert!(bundle.critical_css.contains("@keyframes"));
        assert!(bundle.critical_css.contains("fadeIn"));
        assert!(bundle.critical_css.contains("slideUp"));
    }

    #[test]
    fn test_styles_include_responsive_breakpoints() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        assert!(bundle.critical_css.contains("@media (min-width: 768px)"));
        assert!(bundle.critical_css.contains("@media (max-width: 767px)"));
    }

    #[test]
    fn test_styles_include_accessibility() {
        let opts = ExportOptions::default();
        let bundle = generate_styles(&opts);

        assert!(bundle.critical_css.contains("prefers-reduced-motion"));
        assert!(bundle.critical_css.contains("prefers-contrast"));
        assert!(bundle.critical_css.contains(".sr-only"));
        assert!(bundle.critical_css.contains(":focus-visible"));
    }

    #[test]
    fn test_styles_include_theme_toggle_when_enabled() {
        let opts = ExportOptions {
            include_theme_toggle: true,
            ..Default::default()
        };
        let bundle = generate_styles(&opts);

        assert!(bundle.critical_css.contains("[data-theme=\"light\"]"));
        assert!(bundle.critical_css.contains(".theme-toggle"));
    }
}
