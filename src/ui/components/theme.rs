//! Premium theme definitions with world-class, Stripe-level aesthetics.
//!
//! Design principles:
//! - Muted, sophisticated colors that are easy on the eyes
//! - Clear visual hierarchy with accent colors used sparingly
//! - Consistent design language across all elements
//! - High contrast where it matters (text legibility)
//! - Subtle agent differentiation via tinted backgrounds

use ratatui::style::{Color, Modifier, Style};

/// Premium color palette inspired by modern design systems.
/// Uses low-saturation colors for comfort with refined accents for highlights.
pub mod colors {
    use ratatui::style::Color;

    // ═══════════════════════════════════════════════════════════════════════════
    // BASE COLORS - The foundation of the UI
    // ═══════════════════════════════════════════════════════════════════════════

    /// Deep background - primary canvas color
    pub const BG_DEEP: Color = Color::Rgb(26, 27, 38); // #1a1b26

    /// Elevated surface - cards, modals, popups
    pub const BG_SURFACE: Color = Color::Rgb(36, 40, 59); // #24283b

    /// Subtle surface - hover states, selected items
    pub const BG_HIGHLIGHT: Color = Color::Rgb(41, 46, 66); // #292e42

    /// Border color - subtle separators
    pub const BORDER: Color = Color::Rgb(59, 66, 97); // #3b4261

    /// Border accent - focused/active elements
    pub const BORDER_FOCUS: Color = Color::Rgb(125, 145, 200); // #7d91c8

    // ═══════════════════════════════════════════════════════════════════════════
    // TEXT COLORS - Hierarchical text styling
    // ═══════════════════════════════════════════════════════════════════════════

    /// Primary text - headings, important content
    pub const TEXT_PRIMARY: Color = Color::Rgb(192, 202, 245); // #c0caf5

    /// Secondary text - body content
    pub const TEXT_SECONDARY: Color = Color::Rgb(169, 177, 214); // #a9b1d6

    /// Muted text - hints, placeholders, timestamps
    pub const TEXT_MUTED: Color = Color::Rgb(86, 95, 137); // #565f89 (original Tokyo Night)

    /// Disabled/inactive text
    pub const TEXT_DISABLED: Color = Color::Rgb(68, 75, 106); // #444b6a

    // ═══════════════════════════════════════════════════════════════════════════
    // ACCENT COLORS - Brand and interaction highlights
    // ═══════════════════════════════════════════════════════════════════════════

    /// Primary accent - main actions, links, focus states
    pub const ACCENT_PRIMARY: Color = Color::Rgb(122, 162, 247); // #7aa2f7

    /// Secondary accent - complementary highlights
    pub const ACCENT_SECONDARY: Color = Color::Rgb(187, 154, 247); // #bb9af7

    /// Tertiary accent - subtle highlights
    pub const ACCENT_TERTIARY: Color = Color::Rgb(125, 207, 255); // #7dcfff

    // ═══════════════════════════════════════════════════════════════════════════
    // SEMANTIC COLORS - Role-based coloring (muted versions)
    // ═══════════════════════════════════════════════════════════════════════════

    /// User messages - soft sage green
    pub const ROLE_USER: Color = Color::Rgb(158, 206, 106); // #9ece6a

    /// Agent/Assistant messages - matches primary accent
    pub const ROLE_AGENT: Color = Color::Rgb(122, 162, 247); // #7aa2f7

    /// Tool invocations - warm peach
    pub const ROLE_TOOL: Color = Color::Rgb(255, 158, 100); // #ff9e64

    /// System messages - soft amber
    pub const ROLE_SYSTEM: Color = Color::Rgb(224, 175, 104); // #e0af68

    // ═══════════════════════════════════════════════════════════════════════════
    // STATUS COLORS - Feedback and state indication
    // ═══════════════════════════════════════════════════════════════════════════

    /// Success states
    pub const STATUS_SUCCESS: Color = Color::Rgb(115, 218, 202); // #73daca

    /// Warning states
    pub const STATUS_WARNING: Color = Color::Rgb(224, 175, 104); // #e0af68

    /// Error states
    pub const STATUS_ERROR: Color = Color::Rgb(247, 118, 142); // #f7768e

    /// Info states
    pub const STATUS_INFO: Color = Color::Rgb(125, 207, 255); // #7dcfff

    // ═══════════════════════════════════════════════════════════════════════════
    // AGENT-SPECIFIC TINTS - Distinct background variations per agent
    // ═══════════════════════════════════════════════════════════════════════════

    /// Claude Code - distinct blue tint
    pub const AGENT_CLAUDE_BG: Color = Color::Rgb(24, 30, 52); // #181e34 - blue

    /// Codex - distinct green tint
    pub const AGENT_CODEX_BG: Color = Color::Rgb(22, 38, 32); // #162620 - green

    /// Cline - distinct cyan tint
    pub const AGENT_CLINE_BG: Color = Color::Rgb(20, 34, 42); // #14222a - cyan

    /// Gemini - distinct purple tint
    pub const AGENT_GEMINI_BG: Color = Color::Rgb(34, 24, 48); // #221830 - purple

    /// Amp - distinct warm/orange tint
    pub const AGENT_AMP_BG: Color = Color::Rgb(42, 28, 24); // #2a1c18 - warm

    /// Aider - distinct teal tint
    pub const AGENT_AIDER_BG: Color = Color::Rgb(20, 36, 36); // #142424 - teal

    /// Cursor - distinct magenta tint
    pub const AGENT_CURSOR_BG: Color = Color::Rgb(38, 24, 38); // #261826 - magenta

    /// ChatGPT - distinct emerald tint
    pub const AGENT_CHATGPT_BG: Color = Color::Rgb(20, 38, 28); // #14261c - emerald

    /// `OpenCode` - neutral gray
    pub const AGENT_OPENCODE_BG: Color = Color::Rgb(32, 32, 36); // #202024 - neutral

    // ═══════════════════════════════════════════════════════════════════════════
    // ROLE-AWARE BACKGROUND TINTS - Subtle backgrounds per message type
    // ═══════════════════════════════════════════════════════════════════════════

    /// User message background - subtle green tint
    pub const ROLE_USER_BG: Color = Color::Rgb(26, 32, 30); // #1a201e

    /// Assistant/agent message background - subtle blue tint
    pub const ROLE_AGENT_BG: Color = Color::Rgb(26, 28, 36); // #1a1c24

    /// Tool invocation background - subtle orange/warm tint
    pub const ROLE_TOOL_BG: Color = Color::Rgb(32, 28, 26); // #201c1a

    /// System message background - subtle amber tint
    pub const ROLE_SYSTEM_BG: Color = Color::Rgb(32, 30, 26); // #201e1a

    // ═══════════════════════════════════════════════════════════════════════════
    // GRADIENT SIMULATION COLORS - Multi-shade for depth effects
    // ═══════════════════════════════════════════════════════════════════════════

    /// Header gradient top - darkest shade
    pub const GRADIENT_HEADER_TOP: Color = Color::Rgb(22, 24, 32); // #161820

    /// Header gradient middle - mid shade
    pub const GRADIENT_HEADER_MID: Color = Color::Rgb(30, 32, 44); // #1e202c

    /// Header gradient bottom - lightest shade
    pub const GRADIENT_HEADER_BOT: Color = Color::Rgb(36, 40, 54); // #242836

    /// Pill gradient left
    pub const GRADIENT_PILL_LEFT: Color = Color::Rgb(50, 56, 80); // #323850

    /// Pill gradient center
    pub const GRADIENT_PILL_CENTER: Color = Color::Rgb(60, 68, 96); // #3c4460

    /// Pill gradient right
    pub const GRADIENT_PILL_RIGHT: Color = Color::Rgb(50, 56, 80); // #323850

    // ═══════════════════════════════════════════════════════════════════════════
    // BORDER VARIANTS - For adaptive width styling
    // ═══════════════════════════════════════════════════════════════════════════

    /// Subtle border - for narrow terminals
    pub const BORDER_MINIMAL: Color = Color::Rgb(45, 50, 72); // #2d3248

    /// Standard border - normal terminals
    pub const BORDER_STANDARD: Color = Color::Rgb(59, 66, 97); // #3b4261 (same as BORDER)

    /// Emphasized border - for wide terminals
    pub const BORDER_EMPHASIZED: Color = Color::Rgb(75, 85, 120); // #4b5578
}

/// Complete styling for a message role (user, assistant, tool, system).
#[derive(Clone, Copy)]
pub struct RoleTheme {
    /// Foreground (text) color
    pub fg: Color,
    /// Background tint (subtle)
    pub bg: Color,
    /// Border/accent color
    pub border: Color,
    /// Badge/indicator color
    pub badge: Color,
}

/// Gradient shades for simulating depth effects in headers/pills.
#[derive(Clone, Copy)]
pub struct GradientShades {
    /// Darkest shade (top/edges)
    pub dark: Color,
    /// Mid-tone shade
    pub mid: Color,
    /// Lightest shade (center/bottom)
    pub light: Color,
}

impl GradientShades {
    /// Header gradient - darkest at top, lightest at bottom
    pub fn header() -> Self {
        Self {
            dark: colors::GRADIENT_HEADER_TOP,
            mid: colors::GRADIENT_HEADER_MID,
            light: colors::GRADIENT_HEADER_BOT,
        }
    }

    /// Pill gradient - darker at edges, lighter in center
    pub fn pill() -> Self {
        Self {
            dark: colors::GRADIENT_PILL_LEFT,
            mid: colors::GRADIENT_PILL_CENTER,
            light: colors::GRADIENT_PILL_RIGHT,
        }
    }

    /// Create styles for each shade
    pub fn styles(&self) -> (Style, Style, Style) {
        (
            Style::default().bg(self.dark),
            Style::default().bg(self.mid),
            Style::default().bg(self.light),
        )
    }
}

/// Terminal width classification for adaptive styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalWidth {
    /// Narrow terminal (<80 cols) - minimal decorations
    Narrow,
    /// Normal terminal (80-120 cols) - standard styling
    Normal,
    /// Wide terminal (>120 cols) - enhanced decorations
    Wide,
}

impl TerminalWidth {
    /// Classify terminal width from column count
    pub fn from_cols(cols: u16) -> Self {
        if cols < 80 {
            Self::Narrow
        } else if cols <= 120 {
            Self::Normal
        } else {
            Self::Wide
        }
    }

    /// Get the appropriate border color for this width
    pub fn border_color(self) -> Color {
        match self {
            Self::Narrow => colors::BORDER_MINIMAL,
            Self::Normal => colors::BORDER_STANDARD,
            Self::Wide => colors::BORDER_EMPHASIZED,
        }
    }

    /// Get border style for this width
    pub fn border_style(self) -> Style {
        Style::default().fg(self.border_color())
    }

    /// Should we show decorative elements at this width?
    pub fn show_decorations(self) -> bool {
        !matches!(self, Self::Narrow)
    }

    /// Should we show extended info panels at this width?
    pub fn show_extended_info(self) -> bool {
        matches!(self, Self::Wide)
    }
}

/// Adaptive border configuration based on terminal width.
#[derive(Clone, Copy)]
pub struct AdaptiveBorders {
    /// Current terminal width classification
    pub width_class: TerminalWidth,
    /// Border color
    pub color: Color,
    /// Border style
    pub style: Style,
    /// Use double borders for emphasis
    pub use_double: bool,
    /// Show corner decorations
    pub show_corners: bool,
}

impl AdaptiveBorders {
    /// Create adaptive borders for the given terminal width
    pub fn for_width(cols: u16) -> Self {
        let width_class = TerminalWidth::from_cols(cols);
        let color = width_class.border_color();
        Self {
            width_class,
            color,
            style: Style::default().fg(color),
            use_double: matches!(width_class, TerminalWidth::Wide),
            show_corners: width_class.show_decorations(),
        }
    }

    /// Create borders for focused/active elements
    pub fn focused(cols: u16) -> Self {
        let mut borders = Self::for_width(cols);
        borders.color = colors::BORDER_FOCUS;
        borders.style = Style::default().fg(colors::BORDER_FOCUS);
        borders
    }
}

#[derive(Clone, Copy)]
pub struct PaneTheme {
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
}

#[derive(Clone, Copy)]
pub struct ThemePalette {
    pub accent: Color,
    pub accent_alt: Color,
    pub bg: Color,
    pub fg: Color,
    pub surface: Color,
    pub hint: Color,
    pub border: Color,
    pub user: Color,
    pub agent: Color,
    pub tool: Color,
    pub system: Color,
    /// Alternating stripe colors for zebra-striping results (sux.6.3)
    pub stripe_even: Color,
    pub stripe_odd: Color,
}

impl ThemePalette {
    /// Light theme - clean, minimal, professional
    pub fn light() -> Self {
        Self {
            accent: Color::Rgb(47, 107, 231),       // Rich blue
            accent_alt: Color::Rgb(124, 93, 198),   // Purple
            bg: Color::Rgb(250, 250, 252),          // Off-white
            fg: Color::Rgb(36, 41, 46),             // Near-black
            surface: Color::Rgb(240, 241, 245),     // Light gray
            hint: Color::Rgb(125, 134, 144),        // Medium gray (higher contrast)
            border: Color::Rgb(216, 222, 228),      // Border gray
            user: Color::Rgb(45, 138, 72),          // Forest green
            agent: Color::Rgb(47, 107, 231),        // Rich blue
            tool: Color::Rgb(207, 107, 44),         // Warm orange
            system: Color::Rgb(177, 133, 41),       // Amber
            stripe_even: Color::Rgb(250, 250, 252), // Same as bg
            stripe_odd: Color::Rgb(240, 241, 245),  // Slightly darker
        }
    }

    /// Dark theme - premium, refined, easy on the eyes
    pub fn dark() -> Self {
        Self {
            accent: colors::ACCENT_PRIMARY,
            accent_alt: colors::ACCENT_SECONDARY,
            bg: colors::BG_DEEP,
            fg: colors::TEXT_PRIMARY,
            surface: colors::BG_SURFACE,
            hint: colors::TEXT_MUTED,
            border: colors::BORDER,
            user: colors::ROLE_USER,
            agent: colors::ROLE_AGENT,
            tool: colors::ROLE_TOOL,
            system: colors::ROLE_SYSTEM,
            stripe_even: colors::BG_DEEP,       // #1a1b26
            stripe_odd: Color::Rgb(30, 32, 48), // #1e2030 - slightly lighter
        }
    }

    /// Title style - accent colored with bold modifier
    pub fn title(self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Subtle title style - less prominent headers
    pub fn title_subtle(self) -> Style {
        Style::default().fg(self.fg).add_modifier(Modifier::BOLD)
    }

    /// Hint text style - for secondary/muted information
    pub fn hint_style(self) -> Style {
        Style::default().fg(self.hint)
    }

    /// Border style - for unfocused elements
    pub fn border_style(self) -> Style {
        Style::default().fg(self.border)
    }

    /// Focused border style - for active elements
    pub fn border_focus_style(self) -> Style {
        Style::default().fg(colors::BORDER_FOCUS)
    }

    /// Surface style - for cards, modals, elevated content
    pub fn surface_style(self) -> Style {
        Style::default().bg(self.surface)
    }

    /// Per-agent pane colors - distinct tinted backgrounds with consistent text colors.
    ///
    /// Design philosophy: Each agent gets a visually distinct background color that makes
    /// it immediately clear which tool produced the result. Accent colors are chosen to
    /// complement the background while remaining cohesive.
    pub fn agent_pane(agent: &str) -> PaneTheme {
        let slug = agent.to_lowercase().replace('-', "_");

        let (bg, accent) = match slug.as_str() {
            // Core agents with distinct color identities
            "claude_code" | "claude" => (colors::AGENT_CLAUDE_BG, colors::ACCENT_PRIMARY), // Blue
            "codex" => (colors::AGENT_CODEX_BG, colors::STATUS_SUCCESS),                   // Green
            "cline" => (colors::AGENT_CLINE_BG, colors::ACCENT_TERTIARY),                  // Cyan
            "gemini" | "gemini_cli" => (colors::AGENT_GEMINI_BG, colors::ACCENT_SECONDARY), // Purple
            "amp" => (colors::AGENT_AMP_BG, colors::STATUS_ERROR), // Orange/Red
            "aider" => (colors::AGENT_AIDER_BG, Color::Rgb(64, 224, 208)), // Turquoise accent
            "cursor" => (colors::AGENT_CURSOR_BG, Color::Rgb(236, 72, 153)), // Pink accent
            "chatgpt" => (colors::AGENT_CHATGPT_BG, Color::Rgb(16, 163, 127)), // ChatGPT green
            "opencode" => (colors::AGENT_OPENCODE_BG, colors::ROLE_USER), // Neutral/sage
            "pi_agent" => (colors::AGENT_CODEX_BG, Color::Rgb(255, 140, 0)), // Orange for pi
            _ => (colors::BG_DEEP, colors::ACCENT_PRIMARY),
        };

        PaneTheme {
            bg,
            fg: colors::TEXT_PRIMARY, // Consistent, legible text
            accent,
        }
    }

    /// Returns a small, legible icon for the given agent slug.
    /// Icons favor single-width glyphs to avoid layout jitter in result headers.
    pub fn agent_icon(agent: &str) -> &'static str {
        match agent.to_lowercase().as_str() {
            "codex" => "🔹",
            "claude_code" | "claude" => "🤖",
            "gemini" | "gemini_cli" => "💎",
            "cline" => "🧭",
            "amp" => "⚡",
            "aider" => "🔧",
            "cursor" => "🎯",
            "chatgpt" => "💬",
            "opencode" => "📦",
            "pi_agent" => "🥧",
            _ => "✨",
        }
    }

    /// Get a role-specific style for message rendering
    pub fn role_style(self, role: &str) -> Style {
        let color = match role.to_lowercase().as_str() {
            "user" => self.user,
            "assistant" | "agent" => self.agent,
            "tool" => self.tool,
            "system" => self.system,
            _ => self.hint,
        };
        Style::default().fg(color)
    }

    /// Get a complete `RoleTheme` for a message role with full styling options.
    ///
    /// Includes foreground, background tint, border, and badge colors for
    /// comprehensive role-aware message rendering.
    pub fn role_theme(self, role: &str) -> RoleTheme {
        match role.to_lowercase().as_str() {
            "user" => RoleTheme {
                fg: self.user,
                bg: colors::ROLE_USER_BG,
                border: self.user,
                badge: colors::STATUS_SUCCESS,
            },
            "assistant" | "agent" => RoleTheme {
                fg: self.agent,
                bg: colors::ROLE_AGENT_BG,
                border: self.agent,
                badge: colors::ACCENT_PRIMARY,
            },
            "tool" => RoleTheme {
                fg: self.tool,
                bg: colors::ROLE_TOOL_BG,
                border: self.tool,
                badge: colors::ROLE_TOOL,
            },
            "system" => RoleTheme {
                fg: self.system,
                bg: colors::ROLE_SYSTEM_BG,
                border: self.system,
                badge: colors::STATUS_WARNING,
            },
            _ => RoleTheme {
                fg: self.hint,
                bg: self.bg,
                border: self.border,
                badge: self.hint,
            },
        }
    }

    /// Get the gradient shades for header backgrounds
    pub fn header_gradient(&self) -> GradientShades {
        GradientShades::header()
    }

    /// Get the gradient shades for pills/badges
    pub fn pill_gradient(&self) -> GradientShades {
        GradientShades::pill()
    }

    /// Get adaptive borders for the given terminal width
    pub fn adaptive_borders(&self, cols: u16) -> AdaptiveBorders {
        AdaptiveBorders::for_width(cols)
    }

    /// Get focused adaptive borders for the given terminal width
    pub fn adaptive_borders_focused(&self, cols: u16) -> AdaptiveBorders {
        AdaptiveBorders::focused(cols)
    }

    /// Highlighted text style - for search matches
    /// Uses high-contrast background with theme-aware foreground for visibility
    pub fn highlight_style(self) -> Style {
        Style::default()
            .fg(self.bg) // Dark text on light bg, light text on dark bg
            .bg(self.accent) // Accent color background for high visibility
            .add_modifier(Modifier::BOLD)
    }

    /// Selected item style - for list selections
    pub fn selected_style(self) -> Style {
        Style::default()
            .bg(colors::BG_HIGHLIGHT)
            .add_modifier(Modifier::BOLD)
    }

    /// Code block background style
    pub fn code_style(self) -> Style {
        Style::default()
            .bg(colors::BG_SURFACE)
            .fg(colors::TEXT_SECONDARY)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// STYLE HELPERS - Common style patterns
// ═══════════════════════════════════════════════════════════════════════════════

/// Creates a subtle badge/chip style for filter indicators
pub fn chip_style(palette: ThemePalette) -> Style {
    Style::default()
        .fg(palette.accent_alt)
        .add_modifier(Modifier::BOLD)
}

/// Creates a keyboard shortcut style (for help text)
pub fn kbd_style(palette: ThemePalette) -> Style {
    Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD)
}

/// Creates style for score indicators based on magnitude
pub fn score_style(score: f32, palette: ThemePalette) -> Style {
    let color = if score >= 8.0 {
        colors::STATUS_SUCCESS
    } else if score >= 5.0 {
        palette.accent
    } else {
        palette.hint
    };

    let modifier = if score >= 8.0 {
        Modifier::BOLD
    } else if score >= 5.0 {
        Modifier::empty()
    } else {
        Modifier::DIM
    };

    Style::default().fg(color).add_modifier(modifier)
}

// ═══════════════════════════════════════════════════════════════════════════════
// CONTRAST UTILITIES - WCAG compliance helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Calculate relative luminance of an RGB color per WCAG 2.1.
/// Returns a value from 0.0 (black) to 1.0 (white).
pub fn relative_luminance(color: Color) -> f64 {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        // For non-RGB colors, approximate with reasonable values
        Color::Black => (0, 0, 0),
        Color::White => (255, 255, 255),
        Color::Red => (255, 0, 0),
        Color::Green => (0, 255, 0),
        Color::Blue => (0, 0, 255),
        Color::Yellow => (255, 255, 0),
        Color::Magenta => (255, 0, 255),
        Color::Cyan => (0, 255, 255),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightRed => (255, 128, 128),
        Color::LightGreen => (128, 255, 128),
        Color::LightBlue => (128, 128, 255),
        Color::LightYellow => (255, 255, 128),
        Color::LightMagenta => (255, 128, 255),
        Color::LightCyan => (128, 255, 255),
        _ => (128, 128, 128), // Default gray for indexed colors
    };

    fn linearize(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    let r_lin = linearize(r);
    let g_lin = linearize(g);
    let b_lin = linearize(b);

    0.2126 * r_lin + 0.7152 * g_lin + 0.0722 * b_lin
}

/// Calculate WCAG contrast ratio between two colors.
/// Returns a value from 1.0 (no contrast) to 21.0 (black/white).
pub fn contrast_ratio(fg: Color, bg: Color) -> f64 {
    let lum_fg = relative_luminance(fg);
    let lum_bg = relative_luminance(bg);
    let (lighter, darker) = if lum_fg > lum_bg {
        (lum_fg, lum_bg)
    } else {
        (lum_bg, lum_fg)
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// WCAG compliance level for contrast ratios.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContrastLevel {
    /// Fails WCAG requirements (ratio < 3.0)
    Fail,
    /// WCAG AA for large text (ratio >= 3.0)
    AALarge,
    /// WCAG AA for normal text (ratio >= 4.5)
    AA,
    /// WCAG AAA for large text (ratio >= 4.5)
    AAALarge,
    /// WCAG AAA for normal text (ratio >= 7.0)
    AAA,
}

impl ContrastLevel {
    /// Determine WCAG compliance level from a contrast ratio
    pub fn from_ratio(ratio: f64) -> Self {
        if ratio >= 7.0 {
            Self::AAA
        } else if ratio >= 4.5 {
            Self::AA
        } else if ratio >= 3.0 {
            Self::AALarge
        } else {
            Self::Fail
        }
    }

    /// Check if this level meets the specified minimum requirement
    pub fn meets(self, required: ContrastLevel) -> bool {
        match required {
            Self::Fail => true,
            Self::AALarge => !matches!(self, Self::Fail),
            Self::AA | Self::AAALarge => matches!(self, Self::AA | Self::AAALarge | Self::AAA),
            Self::AAA => matches!(self, Self::AAA),
        }
    }

    /// Display name for this compliance level
    pub fn name(self) -> &'static str {
        match self {
            Self::Fail => "Fail",
            Self::AALarge => "AA (large text)",
            Self::AA => "AA",
            Self::AAALarge => "AAA (large text)",
            Self::AAA => "AAA",
        }
    }
}

/// Check contrast compliance between foreground and background colors.
pub fn check_contrast(fg: Color, bg: Color) -> ContrastLevel {
    ContrastLevel::from_ratio(contrast_ratio(fg, bg))
}

/// Ensure a color meets minimum contrast against a background.
/// If the color doesn't meet the requirement, returns a suggested alternative.
pub fn ensure_contrast(fg: Color, bg: Color, min_level: ContrastLevel) -> Color {
    let level = check_contrast(fg, bg);
    if level.meets(min_level) {
        return fg;
    }

    // Try lightening or darkening the foreground
    let bg_lum = relative_luminance(bg);
    if bg_lum > 0.5 {
        // Dark background, use a darker foreground wouldn't help
        // Return pure black for maximum contrast
        Color::Rgb(0, 0, 0)
    } else {
        // Light background, lighten the foreground
        Color::Rgb(255, 255, 255)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// THEME PRESETS - Popular color schemes for user preference
// ═══════════════════════════════════════════════════════════════════════════════

/// Available theme presets that users can cycle through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemePreset {
    /// Default dark theme - Tokyo Night inspired, premium feel
    #[default]
    Dark,
    /// Light theme - clean, minimal, professional
    Light,
    /// Catppuccin Mocha - warm, pastel colors
    Catppuccin,
    /// Dracula - purple-tinted dark theme
    Dracula,
    /// Nord - arctic, cool blue tones
    Nord,
    /// High Contrast - maximum contrast for accessibility (WCAG AAA)
    HighContrast,
}

impl ThemePreset {
    /// Get the display name for this theme preset
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::Catppuccin => "Catppuccin",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::HighContrast => "High Contrast",
        }
    }

    /// Cycle to the next theme preset
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Catppuccin,
            Self::Catppuccin => Self::Dracula,
            Self::Dracula => Self::Nord,
            Self::Nord => Self::HighContrast,
            Self::HighContrast => Self::Dark,
        }
    }

    /// Cycle to the previous theme preset
    pub fn prev(self) -> Self {
        match self {
            Self::Dark => Self::HighContrast,
            Self::Light => Self::Dark,
            Self::Catppuccin => Self::Light,
            Self::Dracula => Self::Catppuccin,
            Self::Nord => Self::Dracula,
            Self::HighContrast => Self::Nord,
        }
    }

    /// Convert this preset to its `ThemePalette`
    pub fn to_palette(self) -> ThemePalette {
        match self {
            Self::Dark => ThemePalette::dark(),
            Self::Light => ThemePalette::light(),
            Self::Catppuccin => ThemePalette::catppuccin(),
            Self::Dracula => ThemePalette::dracula(),
            Self::Nord => ThemePalette::nord(),
            Self::HighContrast => ThemePalette::high_contrast(),
        }
    }

    /// List all available presets
    pub fn all() -> &'static [Self] {
        &[
            Self::Dark,
            Self::Light,
            Self::Catppuccin,
            Self::Dracula,
            Self::Nord,
            Self::HighContrast,
        ]
    }
}

impl ThemePalette {
    /// Catppuccin Mocha theme - warm, pastel colors
    /// <https://github.com/catppuccin/catppuccin>
    pub fn catppuccin() -> Self {
        Self {
            // Catppuccin Mocha palette
            accent: Color::Rgb(137, 180, 250),     // Blue
            accent_alt: Color::Rgb(203, 166, 247), // Mauve
            bg: Color::Rgb(30, 30, 46),            // Base
            fg: Color::Rgb(205, 214, 244),         // Text
            surface: Color::Rgb(49, 50, 68),       // Surface0
            hint: Color::Rgb(127, 132, 156),       // Overlay1
            border: Color::Rgb(69, 71, 90),        // Surface1
            user: Color::Rgb(166, 227, 161),       // Green
            agent: Color::Rgb(137, 180, 250),      // Blue
            tool: Color::Rgb(250, 179, 135),       // Peach
            system: Color::Rgb(249, 226, 175),     // Yellow
            stripe_even: Color::Rgb(30, 30, 46),   // Base
            stripe_odd: Color::Rgb(36, 36, 54),    // Slightly lighter
        }
    }

    /// Dracula theme - purple-tinted dark theme
    /// <https://draculatheme.com>/
    pub fn dracula() -> Self {
        Self {
            // Dracula palette
            accent: Color::Rgb(189, 147, 249),     // Purple
            accent_alt: Color::Rgb(255, 121, 198), // Pink
            bg: Color::Rgb(40, 42, 54),            // Background
            fg: Color::Rgb(248, 248, 242),         // Foreground
            surface: Color::Rgb(68, 71, 90),       // Current Line
            hint: Color::Rgb(98, 114, 164),        // Comment (#6272a4 - original Dracula)
            border: Color::Rgb(68, 71, 90),        // Current Line
            user: Color::Rgb(80, 250, 123),        // Green
            agent: Color::Rgb(189, 147, 249),      // Purple
            tool: Color::Rgb(255, 184, 108),       // Orange
            system: Color::Rgb(241, 250, 140),     // Yellow
            stripe_even: Color::Rgb(40, 42, 54),   // Background
            stripe_odd: Color::Rgb(48, 50, 64),    // Slightly lighter
        }
    }

    /// Nord theme - arctic, cool blue tones
    /// <https://www.nordtheme.com>/
    pub fn nord() -> Self {
        Self {
            // Nord palette
            accent: Color::Rgb(136, 192, 208), // Nord8 (frost cyan)
            accent_alt: Color::Rgb(180, 142, 173), // Nord15 (aurora purple)
            bg: Color::Rgb(46, 52, 64),        // Nord0 (polar night)
            fg: Color::Rgb(236, 239, 244),     // Nord6 (snow storm)
            surface: Color::Rgb(59, 66, 82),   // Nord1
            hint: Color::Rgb(76, 86, 106),     // Nord3 (#4c566a - original Nord)
            border: Color::Rgb(67, 76, 94),    // Nord2
            user: Color::Rgb(163, 190, 140),   // Nord14 (aurora green)
            agent: Color::Rgb(136, 192, 208),  // Nord8 (frost cyan)
            tool: Color::Rgb(208, 135, 112),   // Nord12 (aurora orange)
            system: Color::Rgb(235, 203, 139), // Nord13 (aurora yellow)
            stripe_even: Color::Rgb(46, 52, 64), // Nord0
            stripe_odd: Color::Rgb(52, 58, 72), // Slightly lighter
        }
    }

    /// High Contrast theme - maximum contrast for accessibility
    ///
    /// Designed to meet WCAG AAA standards (7:1 contrast ratio).
    /// Uses pure black/white with saturated accent colors for maximum visibility.
    pub fn high_contrast() -> Self {
        Self {
            // High contrast palette - pure black background, white text
            accent: Color::Rgb(0, 191, 255), // Bright cyan - high visibility
            accent_alt: Color::Rgb(255, 105, 180), // Hot pink - distinct secondary
            bg: Color::Rgb(0, 0, 0),         // Pure black
            fg: Color::Rgb(255, 255, 255),   // Pure white
            surface: Color::Rgb(28, 28, 28), // Very dark gray for elevation
            hint: Color::Rgb(180, 180, 180), // Light gray - still readable
            border: Color::Rgb(255, 255, 255), // White borders for clear separation
            user: Color::Rgb(0, 255, 127),   // Bright spring green
            agent: Color::Rgb(0, 191, 255),  // Bright cyan (matches accent)
            tool: Color::Rgb(255, 165, 0),   // Bright orange
            system: Color::Rgb(255, 255, 0), // Pure yellow
            stripe_even: Color::Rgb(0, 0, 0), // Pure black
            stripe_odd: Color::Rgb(24, 24, 24), // Very dark gray
        }
    }
}
