//! Premium theme definitions with world-class, Stripe-level aesthetics.
//!
//! Design principles:
//! - Muted, sophisticated colors that are easy on the eyes
//! - Clear visual hierarchy with accent colors used sparingly
//! - Consistent design language across all elements
//! - High contrast where it matters (text legibility)
//! - Subtle agent differentiation via tinted backgrounds

use ftui::Style;
use ftui::render::cell::PackedRgba;

/// Premium color palette inspired by modern design systems.
/// Uses low-saturation colors for comfort with refined accents for highlights.
pub mod colors {
    use ftui::render::cell::PackedRgba as Color;

    // ═══════════════════════════════════════════════════════════════════════════
    // BASE COLORS - The foundation of the UI
    // ═══════════════════════════════════════════════════════════════════════════

    /// Deep background - primary canvas color
    pub const BG_DEEP: Color = Color::rgb(26, 27, 38); // #1a1b26

    /// Elevated surface - cards, modals, popups
    pub const BG_SURFACE: Color = Color::rgb(36, 40, 59); // #24283b

    /// Subtle surface - hover states, selected items
    pub const BG_HIGHLIGHT: Color = Color::rgb(41, 46, 66); // #292e42

    /// Border color - subtle separators
    pub const BORDER: Color = Color::rgb(59, 66, 97); // #3b4261

    /// Border accent - focused/active elements
    pub const BORDER_FOCUS: Color = Color::rgb(125, 145, 200); // #7d91c8

    // ═══════════════════════════════════════════════════════════════════════════
    // TEXT COLORS - Hierarchical text styling
    // ═══════════════════════════════════════════════════════════════════════════

    /// Primary text - headings, important content
    pub const TEXT_PRIMARY: Color = Color::rgb(192, 202, 245); // #c0caf5

    /// Secondary text - body content
    pub const TEXT_SECONDARY: Color = Color::rgb(169, 177, 214); // #a9b1d6

    /// Muted text - hints, placeholders, timestamps
    /// Lightened from original Tokyo Night #565f89 to meet WCAG AA-large (3:1) contrast
    pub const TEXT_MUTED: Color = Color::rgb(105, 114, 158); // #696e9e (WCAG AA-large compliant)

    /// Disabled/inactive text
    pub const TEXT_DISABLED: Color = Color::rgb(68, 75, 106); // #444b6a

    // ═══════════════════════════════════════════════════════════════════════════
    // ACCENT COLORS - Brand and interaction highlights
    // ═══════════════════════════════════════════════════════════════════════════

    /// Primary accent - main actions, links, focus states
    pub const ACCENT_PRIMARY: Color = Color::rgb(122, 162, 247); // #7aa2f7

    /// Secondary accent - complementary highlights
    pub const ACCENT_SECONDARY: Color = Color::rgb(187, 154, 247); // #bb9af7

    /// Tertiary accent - subtle highlights
    pub const ACCENT_TERTIARY: Color = Color::rgb(125, 207, 255); // #7dcfff

    // ═══════════════════════════════════════════════════════════════════════════
    // SEMANTIC COLORS - Role-based coloring (muted versions)
    // ═══════════════════════════════════════════════════════════════════════════

    /// User messages - soft sage green
    pub const ROLE_USER: Color = Color::rgb(158, 206, 106); // #9ece6a

    /// Agent/Assistant messages - matches primary accent
    pub const ROLE_AGENT: Color = Color::rgb(122, 162, 247); // #7aa2f7

    /// Tool invocations - warm peach
    pub const ROLE_TOOL: Color = Color::rgb(255, 158, 100); // #ff9e64

    /// System messages - soft amber
    pub const ROLE_SYSTEM: Color = Color::rgb(224, 175, 104); // #e0af68

    // ═══════════════════════════════════════════════════════════════════════════
    // STATUS COLORS - Feedback and state indication
    // ═══════════════════════════════════════════════════════════════════════════

    /// Success states
    pub const STATUS_SUCCESS: Color = Color::rgb(115, 218, 202); // #73daca

    /// Warning states
    pub const STATUS_WARNING: Color = Color::rgb(224, 175, 104); // #e0af68

    /// Error states
    pub const STATUS_ERROR: Color = Color::rgb(247, 118, 142); // #f7768e

    /// Info states
    pub const STATUS_INFO: Color = Color::rgb(125, 207, 255); // #7dcfff

    // ═══════════════════════════════════════════════════════════════════════════
    // AGENT-SPECIFIC TINTS - Distinct background variations per agent
    // ═══════════════════════════════════════════════════════════════════════════

    /// Claude Code - distinct blue tint
    pub const AGENT_CLAUDE_BG: Color = Color::rgb(24, 30, 52); // #181e34 - blue

    /// Codex - distinct green tint
    pub const AGENT_CODEX_BG: Color = Color::rgb(22, 38, 32); // #162620 - green

    /// Cline - distinct cyan tint
    pub const AGENT_CLINE_BG: Color = Color::rgb(20, 34, 42); // #14222a - cyan

    /// Gemini - distinct purple tint
    pub const AGENT_GEMINI_BG: Color = Color::rgb(34, 24, 48); // #221830 - purple

    /// Amp - distinct warm/orange tint
    pub const AGENT_AMP_BG: Color = Color::rgb(42, 28, 24); // #2a1c18 - warm

    /// Aider - distinct teal tint
    pub const AGENT_AIDER_BG: Color = Color::rgb(20, 36, 36); // #142424 - teal

    /// Cursor - distinct magenta tint
    pub const AGENT_CURSOR_BG: Color = Color::rgb(38, 24, 38); // #261826 - magenta

    /// ChatGPT - distinct emerald tint
    pub const AGENT_CHATGPT_BG: Color = Color::rgb(20, 38, 28); // #14261c - emerald

    /// `OpenCode` - neutral gray
    pub const AGENT_OPENCODE_BG: Color = Color::rgb(32, 32, 36); // #202024 - neutral

    /// Factory (Droid) - warm amber tint
    pub const AGENT_FACTORY_BG: Color = Color::rgb(36, 30, 20); // #241e14 - amber

    /// Clawdbot - indigo tint
    pub const AGENT_CLAWDBOT_BG: Color = Color::rgb(26, 24, 44); // #1a182c - indigo

    /// Vibe (Mistral) - rose tint
    pub const AGENT_VIBE_BG: Color = Color::rgb(36, 22, 30); // #24161e - rose

    /// Openclaw - slate tint
    pub const AGENT_OPENCLAW_BG: Color = Color::rgb(24, 30, 34); // #181e22 - slate

    // ═══════════════════════════════════════════════════════════════════════════
    // ROLE-AWARE BACKGROUND TINTS - Subtle backgrounds per message type
    // ═══════════════════════════════════════════════════════════════════════════

    /// User message background - subtle green tint
    pub const ROLE_USER_BG: Color = Color::rgb(26, 32, 30); // #1a201e

    /// Assistant/agent message background - subtle blue tint
    pub const ROLE_AGENT_BG: Color = Color::rgb(26, 28, 36); // #1a1c24

    /// Tool invocation background - subtle orange/warm tint
    pub const ROLE_TOOL_BG: Color = Color::rgb(32, 28, 26); // #201c1a

    /// System message background - subtle amber tint
    pub const ROLE_SYSTEM_BG: Color = Color::rgb(32, 30, 26); // #201e1a

    // ═══════════════════════════════════════════════════════════════════════════
    // GRADIENT SIMULATION COLORS - Multi-shade for depth effects
    // ═══════════════════════════════════════════════════════════════════════════

    /// Header gradient top - darkest shade
    pub const GRADIENT_HEADER_TOP: Color = Color::rgb(22, 24, 32); // #161820

    /// Header gradient middle - mid shade
    pub const GRADIENT_HEADER_MID: Color = Color::rgb(30, 32, 44); // #1e202c

    /// Header gradient bottom - lightest shade
    pub const GRADIENT_HEADER_BOT: Color = Color::rgb(36, 40, 54); // #242836

    /// Pill gradient left
    pub const GRADIENT_PILL_LEFT: Color = Color::rgb(50, 56, 80); // #323850

    /// Pill gradient center
    pub const GRADIENT_PILL_CENTER: Color = Color::rgb(60, 68, 96); // #3c4460

    /// Pill gradient right
    pub const GRADIENT_PILL_RIGHT: Color = Color::rgb(50, 56, 80); // #323850

    // ═══════════════════════════════════════════════════════════════════════════
    // BORDER VARIANTS - For adaptive width styling
    // ═══════════════════════════════════════════════════════════════════════════

    /// Subtle border - for narrow terminals
    pub const BORDER_MINIMAL: Color = Color::rgb(45, 50, 72); // #2d3248

    /// Standard border - normal terminals
    pub const BORDER_STANDARD: Color = Color::rgb(59, 66, 97); // #3b4261 (same as BORDER)

    /// Emphasized border - for wide terminals
    pub const BORDER_EMPHASIZED: Color = Color::rgb(75, 85, 120); // #4b5578
}

/// Complete styling for a message role (user, assistant, tool, system).
#[derive(Clone, Copy)]
pub struct RoleTheme {
    /// Foreground (text) color
    pub fg: PackedRgba,
    /// Background tint (subtle)
    pub bg: PackedRgba,
    /// Border/accent color
    pub border: PackedRgba,
    /// Badge/indicator color
    pub badge: PackedRgba,
}

/// Gradient shades for simulating depth effects in headers/pills.
#[derive(Clone, Copy)]
pub struct GradientShades {
    /// Darkest shade (top/edges)
    pub dark: PackedRgba,
    /// Mid-tone shade
    pub mid: PackedRgba,
    /// Lightest shade (center/bottom)
    pub light: PackedRgba,
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
            Style::new().bg(self.dark),
            Style::new().bg(self.mid),
            Style::new().bg(self.light),
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
    pub fn border_color(self) -> PackedRgba {
        match self {
            Self::Narrow => colors::BORDER_MINIMAL,
            Self::Normal => colors::BORDER_STANDARD,
            Self::Wide => colors::BORDER_EMPHASIZED,
        }
    }

    /// Get border style for this width
    pub fn border_style(self) -> Style {
        Style::new().fg(self.border_color())
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
    pub color: PackedRgba,
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
            style: Style::new().fg(color),
            use_double: matches!(width_class, TerminalWidth::Wide),
            show_corners: width_class.show_decorations(),
        }
    }

    /// Create borders for focused/active elements
    pub fn focused(cols: u16) -> Self {
        let mut borders = Self::for_width(cols);
        borders.color = colors::BORDER_FOCUS;
        borders.style = Style::new().fg(colors::BORDER_FOCUS);
        borders
    }
}

#[derive(Clone, Copy)]
pub struct PaneTheme {
    pub bg: PackedRgba,
    pub fg: PackedRgba,
    pub accent: PackedRgba,
}

#[derive(Clone, Copy)]
pub struct ThemePalette {
    pub accent: PackedRgba,
    pub accent_alt: PackedRgba,
    pub bg: PackedRgba,
    pub fg: PackedRgba,
    pub surface: PackedRgba,
    pub hint: PackedRgba,
    pub border: PackedRgba,
    pub user: PackedRgba,
    pub agent: PackedRgba,
    pub tool: PackedRgba,
    pub system: PackedRgba,
    /// Alternating stripe colors for zebra-striping results (sux.6.3)
    pub stripe_even: PackedRgba,
    pub stripe_odd: PackedRgba,
}

impl ThemePalette {
    /// Light theme - clean, minimal, professional
    pub fn light() -> Self {
        Self {
            accent: PackedRgba::rgb(47, 107, 231),       // Rich blue
            accent_alt: PackedRgba::rgb(124, 93, 198),   // Purple
            bg: PackedRgba::rgb(250, 250, 252),          // Off-white
            fg: PackedRgba::rgb(36, 41, 46),             // Near-black
            surface: PackedRgba::rgb(240, 241, 245),     // Light gray
            hint: PackedRgba::rgb(125, 134, 144),        // Medium gray (higher contrast)
            border: PackedRgba::rgb(216, 222, 228),      // Border gray
            user: PackedRgba::rgb(45, 138, 72),          // Forest green
            agent: PackedRgba::rgb(47, 107, 231),        // Rich blue
            tool: PackedRgba::rgb(207, 107, 44),         // Warm orange
            system: PackedRgba::rgb(177, 133, 41),       // Amber
            stripe_even: PackedRgba::rgb(250, 250, 252), // Same as bg
            stripe_odd: PackedRgba::rgb(240, 241, 245),  // Slightly darker
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
            stripe_even: colors::BG_DEEP,            // #1a1b26
            stripe_odd: PackedRgba::rgb(30, 32, 48), // #1e2030 - slightly lighter
        }
    }

    /// Title style - accent colored with bold modifier
    pub fn title(self) -> Style {
        Style::new().fg(self.accent).bold()
    }

    /// Subtle title style - less prominent headers
    pub fn title_subtle(self) -> Style {
        Style::new().fg(self.fg).bold()
    }

    /// Hint text style - for secondary/muted information
    pub fn hint_style(self) -> Style {
        Style::new().fg(self.hint)
    }

    /// Border style - for unfocused elements
    pub fn border_style(self) -> Style {
        Style::new().fg(self.border)
    }

    /// Focused border style - for active elements (theme-aware)
    pub fn border_focus_style(self) -> Style {
        Style::new().fg(self.accent)
    }

    /// Surface style - for cards, modals, elevated content
    pub fn surface_style(self) -> Style {
        Style::new().bg(self.surface)
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
            "aider" => (colors::AGENT_AIDER_BG, PackedRgba::rgb(64, 224, 208)), // Turquoise accent
            "cursor" => (colors::AGENT_CURSOR_BG, PackedRgba::rgb(236, 72, 153)), // Pink accent
            "chatgpt" => (colors::AGENT_CHATGPT_BG, PackedRgba::rgb(16, 163, 127)), // ChatGPT green
            "opencode" => (colors::AGENT_OPENCODE_BG, colors::ROLE_USER), // Neutral/sage
            "pi_agent" => (colors::AGENT_CODEX_BG, PackedRgba::rgb(255, 140, 0)), // Orange for pi
            "factory" | "droid" => (colors::AGENT_FACTORY_BG, PackedRgba::rgb(230, 176, 60)), // Amber
            "clawdbot" => (colors::AGENT_CLAWDBOT_BG, PackedRgba::rgb(140, 130, 240)), // Indigo
            "vibe" | "mistral" => (colors::AGENT_VIBE_BG, PackedRgba::rgb(220, 100, 160)), // Rose
            "openclaw" => (colors::AGENT_OPENCLAW_BG, PackedRgba::rgb(130, 190, 210)), // Slate blue
            _ => (colors::BG_DEEP, colors::ACCENT_PRIMARY),
        };

        PaneTheme {
            bg,
            fg: colors::TEXT_PRIMARY, // Consistent, legible text
            accent,
        }
    }

    /// Returns a small, legible icon for the given agent slug.
    /// Icons favor deterministic single-width glyphs to avoid layout jitter and
    /// emoji fallback artifacts in terminal renderers.
    pub fn agent_icon(agent: &str) -> &'static str {
        match agent.to_lowercase().as_str() {
            "codex" => "◆",
            "claude_code" | "claude" => "●",
            "gemini" | "gemini_cli" => "◇",
            "cline" => "■",
            "amp" => "▲",
            "aider" => "▼",
            "cursor" => "◈",
            "chatgpt" => "○",
            "opencode" => "□",
            "pi_agent" => "△",
            "factory" | "droid" => "▣",
            "clawdbot" => "⬢",
            "vibe" | "mistral" => "✦",
            "openclaw" => "⬡",
            _ => "•",
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
        Style::new().fg(color)
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
        Style::new()
            .fg(self.bg) // Dark text on light bg, light text on dark bg
            .bg(self.accent) // Accent color background for high visibility
            .bold()
    }

    /// Selected item style - for list selections (theme-aware)
    pub fn selected_style(self) -> Style {
        Style::new().bg(self.surface).bold()
    }

    /// Code block background style (theme-aware)
    pub fn code_style(self) -> Style {
        Style::new().bg(self.surface).fg(self.hint)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// STYLE HELPERS - Common style patterns
// ═══════════════════════════════════════════════════════════════════════════════

/// Creates a subtle badge/chip style for filter indicators
pub fn chip_style(palette: ThemePalette) -> Style {
    Style::new().fg(palette.accent_alt).bold()
}

/// Creates a keyboard shortcut style (for help text)
pub fn kbd_style(palette: ThemePalette) -> Style {
    Style::new().fg(palette.accent).bold()
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

    let base = Style::new().fg(color);
    if score >= 8.0 {
        base.bold()
    } else if score < 5.0 {
        base.dim()
    } else {
        base
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// CONTRAST UTILITIES - WCAG compliance helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Calculate relative luminance of an RGB color per WCAG 2.1.
/// Returns a value from 0.0 (black) to 1.0 (white).
pub fn relative_luminance(color: PackedRgba) -> f64 {
    let (r, g, b) = (color.r(), color.g(), color.b());

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
pub fn contrast_ratio(fg: PackedRgba, bg: PackedRgba) -> f64 {
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
pub fn check_contrast(fg: PackedRgba, bg: PackedRgba) -> ContrastLevel {
    ContrastLevel::from_ratio(contrast_ratio(fg, bg))
}

/// Ensure a color meets minimum contrast against a background.
/// If the color doesn't meet the requirement, returns a suggested alternative.
pub fn ensure_contrast(fg: PackedRgba, bg: PackedRgba, min_level: ContrastLevel) -> PackedRgba {
    let level = check_contrast(fg, bg);
    if level.meets(min_level) {
        return fg;
    }

    // Try lightening or darkening the foreground
    let bg_lum = relative_luminance(bg);
    if bg_lum > 0.5 {
        // Light background, use black for maximum contrast
        PackedRgba::BLACK
    } else {
        // Dark background, use white for maximum contrast
        PackedRgba::WHITE
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
    TokyoNight,
    /// Light theme - clean, minimal, professional
    Daylight,
    /// Catppuccin Mocha - warm, pastel colors
    Catppuccin,
    /// Dracula - purple-tinted dark theme
    Dracula,
    /// Nord - arctic, cool blue tones
    Nord,
    /// Solarized Dark
    SolarizedDark,
    /// Solarized Light
    SolarizedLight,
    /// Monokai
    Monokai,
    /// Gruvbox Dark
    GruvboxDark,
    /// One Dark
    OneDark,
    /// Rosé Pine
    RosePine,
    /// Everforest
    Everforest,
    /// Kanagawa
    Kanagawa,
    /// Ayu Mirage
    AyuMirage,
    /// Nightfox
    Nightfox,
    /// Cyberpunk Aurora
    CyberpunkAurora,
    /// Synthwave '84
    Synthwave84,
    /// High Contrast - maximum contrast for accessibility (WCAG AAA)
    HighContrast,
    /// Colorblind - deuteranopia/protanopia accessible variant of Tokyo Night
    /// Replaces green/orange with blue/yellow for red-green colorblind users
    Colorblind,
}

impl ThemePreset {
    const ALL: [Self; 19] = [
        Self::TokyoNight,
        Self::Daylight,
        Self::Catppuccin,
        Self::Dracula,
        Self::Nord,
        Self::SolarizedDark,
        Self::SolarizedLight,
        Self::Monokai,
        Self::GruvboxDark,
        Self::OneDark,
        Self::RosePine,
        Self::Everforest,
        Self::Kanagawa,
        Self::AyuMirage,
        Self::Nightfox,
        Self::CyberpunkAurora,
        Self::Synthwave84,
        Self::HighContrast,
        Self::Colorblind,
    ];

    /// Get the display name for this theme preset
    pub fn name(self) -> &'static str {
        match self {
            Self::TokyoNight => "Tokyo Night",
            Self::Daylight => "Daylight",
            Self::Catppuccin => "Catppuccin Mocha",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::SolarizedDark => "Solarized Dark",
            Self::SolarizedLight => "Solarized Light",
            Self::Monokai => "Monokai",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::OneDark => "One Dark",
            Self::RosePine => "Ros\u{e9} Pine",
            Self::Everforest => "Everforest",
            Self::Kanagawa => "Kanagawa",
            Self::AyuMirage => "Ayu Mirage",
            Self::Nightfox => "Nightfox",
            Self::CyberpunkAurora => "Cyberpunk Aurora",
            Self::Synthwave84 => "Synthwave '84",
            Self::HighContrast => "High Contrast",
            Self::Colorblind => "Colorblind",
        }
    }

    /// Cycle to the next theme preset
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Cycle to the previous theme preset
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Convert this preset to its `ThemePalette`
    pub fn to_palette(self) -> ThemePalette {
        match self {
            Self::TokyoNight => ThemePalette::dark(),
            Self::Daylight => ThemePalette::light(),
            Self::Catppuccin => ThemePalette::catppuccin(),
            Self::Dracula => ThemePalette::dracula(),
            Self::Nord => ThemePalette::nord(),
            Self::SolarizedDark => ThemePalette::solarized_dark(),
            Self::SolarizedLight => ThemePalette::solarized_light(),
            Self::Monokai => ThemePalette::monokai(),
            Self::GruvboxDark => ThemePalette::gruvbox_dark(),
            Self::OneDark => ThemePalette::one_dark(),
            Self::RosePine => ThemePalette::rose_pine(),
            Self::Everforest => ThemePalette::everforest(),
            Self::Kanagawa => ThemePalette::kanagawa(),
            Self::AyuMirage => ThemePalette::ayu_mirage(),
            Self::Nightfox => ThemePalette::nightfox(),
            Self::CyberpunkAurora => ThemePalette::cyberpunk_aurora(),
            Self::Synthwave84 => ThemePalette::synthwave_84(),
            Self::HighContrast => ThemePalette::high_contrast(),
            Self::Colorblind => ThemePalette::colorblind(),
        }
    }

    /// List all available presets
    pub fn all() -> &'static [Self] {
        &Self::ALL
    }
}

impl ThemePalette {
    /// Catppuccin Mocha theme - warm, pastel colors
    /// <https://github.com/catppuccin/catppuccin>
    pub fn catppuccin() -> Self {
        Self {
            // Catppuccin Mocha palette
            accent: PackedRgba::rgb(137, 180, 250),     // Blue
            accent_alt: PackedRgba::rgb(203, 166, 247), // Mauve
            bg: PackedRgba::rgb(30, 30, 46),            // Base
            fg: PackedRgba::rgb(205, 214, 244),         // Text
            surface: PackedRgba::rgb(49, 50, 68),       // Surface0
            hint: PackedRgba::rgb(127, 132, 156),       // Overlay1
            border: PackedRgba::rgb(69, 71, 90),        // Surface1
            user: PackedRgba::rgb(166, 227, 161),       // Green
            agent: PackedRgba::rgb(137, 180, 250),      // Blue
            tool: PackedRgba::rgb(250, 179, 135),       // Peach
            system: PackedRgba::rgb(249, 226, 175),     // Yellow
            stripe_even: PackedRgba::rgb(30, 30, 46),   // Base
            stripe_odd: PackedRgba::rgb(36, 36, 54),    // Slightly lighter
        }
    }

    /// Dracula theme - purple-tinted dark theme
    /// <https://draculatheme.com>/
    pub fn dracula() -> Self {
        Self {
            // Dracula palette
            accent: PackedRgba::rgb(189, 147, 249), // Purple
            accent_alt: PackedRgba::rgb(255, 121, 198), // Pink
            bg: PackedRgba::rgb(40, 42, 54),        // Background
            fg: PackedRgba::rgb(248, 248, 242),     // Foreground
            surface: PackedRgba::rgb(68, 71, 90),   // Current Line
            hint: PackedRgba::rgb(155, 165, 200), // Lightened from Dracula comment for WCAG AA-large on surface
            border: PackedRgba::rgb(68, 71, 90),  // Current Line
            user: PackedRgba::rgb(80, 250, 123),  // Green
            agent: PackedRgba::rgb(189, 147, 249), // Purple
            tool: PackedRgba::rgb(255, 184, 108), // Orange
            system: PackedRgba::rgb(241, 250, 140), // Yellow
            stripe_even: PackedRgba::rgb(40, 42, 54), // Background
            stripe_odd: PackedRgba::rgb(48, 50, 64), // Slightly lighter
        }
    }

    /// Nord theme - arctic, cool blue tones
    /// <https://www.nordtheme.com>/
    pub fn nord() -> Self {
        Self {
            // Nord palette
            accent: PackedRgba::rgb(136, 192, 208), // Nord8 (frost cyan)
            accent_alt: PackedRgba::rgb(180, 142, 173), // Nord15 (aurora purple)
            bg: PackedRgba::rgb(46, 52, 64),        // Nord0 (polar night)
            fg: PackedRgba::rgb(236, 239, 244),     // Nord6 (snow storm)
            surface: PackedRgba::rgb(59, 66, 82),   // Nord1
            hint: PackedRgba::rgb(145, 155, 180), // Lightened from Nord3 for WCAG AA-large on surface
            border: PackedRgba::rgb(67, 76, 94),  // Nord2
            user: PackedRgba::rgb(163, 190, 140), // Nord14 (aurora green)
            agent: PackedRgba::rgb(136, 192, 208), // Nord8 (frost cyan)
            tool: PackedRgba::rgb(208, 135, 112), // Nord12 (aurora orange)
            system: PackedRgba::rgb(235, 203, 139), // Nord13 (aurora yellow)
            stripe_even: PackedRgba::rgb(46, 52, 64), // Nord0
            stripe_odd: PackedRgba::rgb(52, 58, 72), // Slightly lighter
        }
    }

    /// High Contrast theme - maximum contrast for accessibility
    ///
    /// Designed to meet WCAG AAA standards (7:1 contrast ratio).
    /// Uses pure black/white with saturated accent colors for maximum visibility.
    pub fn high_contrast() -> Self {
        Self {
            accent: PackedRgba::rgb(0, 191, 255),
            accent_alt: PackedRgba::rgb(255, 105, 180),
            bg: PackedRgba::BLACK,
            fg: PackedRgba::WHITE,
            surface: PackedRgba::rgb(28, 28, 28),
            hint: PackedRgba::rgb(180, 180, 180),
            border: PackedRgba::WHITE,
            user: PackedRgba::rgb(0, 255, 127),
            agent: PackedRgba::rgb(0, 191, 255),
            tool: PackedRgba::rgb(255, 165, 0),
            system: PackedRgba::rgb(255, 255, 0),
            stripe_even: PackedRgba::BLACK,
            stripe_odd: PackedRgba::rgb(24, 24, 24),
        }
    }

    /// Colorblind-accessible theme - Tokyo Night base with deuteranopia/protanopia-safe colors.
    ///
    /// Replaces green (#9ece6a) with blue (#7aa2f7) and orange (#ff9e64) with yellow (#e0af68)
    /// so that role colors remain distinguishable for red-green colorblind users.
    /// Red (#f7768e) is replaced with magenta/purple (#bb9af7).
    /// Background, text, and accent colors are unchanged from Tokyo Night.
    pub fn colorblind() -> Self {
        Self {
            accent: colors::ACCENT_PRIMARY,          // #7aa2f7 (unchanged)
            accent_alt: colors::ACCENT_SECONDARY,    // #bb9af7 (unchanged)
            bg: colors::BG_DEEP,                     // #1a1b26 (unchanged)
            fg: colors::TEXT_PRIMARY,                // #c0caf5 (unchanged)
            surface: colors::BG_SURFACE,             // #24283b (unchanged)
            hint: colors::TEXT_MUTED,                // #696e9e (unchanged)
            border: colors::BORDER,                  // #3b4261 (unchanged)
            user: PackedRgba::rgb(125, 207, 255), // #7dcfff cyan (was green #9ece6a — distinct from agent blue)
            agent: colors::ROLE_AGENT,            // #7aa2f7 blue (unchanged)
            tool: PackedRgba::rgb(224, 175, 104), // #e0af68 yellow (was orange #ff9e64)
            system: PackedRgba::rgb(208, 154, 247), // #d09af7 light magenta (was amber #e0af68 — distinct from accent_alt/error)
            stripe_even: colors::BG_DEEP,           // #1a1b26
            stripe_odd: PackedRgba::rgb(30, 32, 48), // #1e2030
        }
    }

    pub fn solarized_dark() -> Self {
        Self {
            accent: PackedRgba::rgb(38, 139, 210),      // #268bd2 blue
            accent_alt: PackedRgba::rgb(108, 113, 196), // #6c71c4 violet
            bg: PackedRgba::rgb(0, 43, 54),             // #002b36 base03
            fg: PackedRgba::rgb(147, 161, 161),         // #93a1a1 base1 (WCAG AA on surface)
            surface: PackedRgba::rgb(7, 54, 66),        // #073642 base02
            hint: PackedRgba::rgb(105, 127, 134), // lightened base00 (WCAG AA-large on surface)
            border: PackedRgba::rgb(88, 110, 117), // #586e75 base01
            user: PackedRgba::rgb(133, 153, 0),   // #859900 green
            agent: PackedRgba::rgb(38, 139, 210), // #268bd2 blue
            tool: PackedRgba::rgb(203, 75, 22),   // #cb4b16 orange
            system: PackedRgba::rgb(181, 137, 0), // #b58900 yellow
            stripe_even: PackedRgba::rgb(0, 43, 54),
            stripe_odd: PackedRgba::rgb(7, 54, 66),
        }
    }

    pub fn solarized_light() -> Self {
        Self {
            accent: PackedRgba::rgb(38, 139, 210),
            accent_alt: PackedRgba::rgb(108, 113, 196),
            bg: PackedRgba::rgb(253, 246, 227), // #fdf6e3 base3
            fg: PackedRgba::rgb(86, 108, 116),  // darkened base01 (WCAG AA on surface+bg)
            surface: PackedRgba::rgb(238, 232, 213), // #eee8d5 base2
            hint: PackedRgba::rgb(115, 132, 134), // darkened base0 (WCAG AA-large on surface+bg)
            border: PackedRgba::rgb(147, 161, 161), // #93a1a1 base1
            user: PackedRgba::rgb(128, 148, 0), // darkened green (WCAG AA-large on bg)
            agent: PackedRgba::rgb(38, 139, 210),
            tool: PackedRgba::rgb(203, 75, 22),
            system: PackedRgba::rgb(177, 133, 0), // darkened yellow (WCAG AA-large on bg)
            stripe_even: PackedRgba::rgb(253, 246, 227),
            stripe_odd: PackedRgba::rgb(238, 232, 213),
        }
    }

    pub fn monokai() -> Self {
        Self {
            accent: PackedRgba::rgb(166, 226, 46),      // #a6e22e green
            accent_alt: PackedRgba::rgb(174, 129, 255), // #ae81ff purple
            bg: PackedRgba::rgb(39, 40, 34),            // #272822
            fg: PackedRgba::rgb(248, 248, 242),         // #f8f8f2
            surface: PackedRgba::rgb(53, 54, 45),       // #35362d
            hint: PackedRgba::rgb(150, 155, 140),       // #969b8c
            border: PackedRgba::rgb(73, 72, 62),        // #49483e
            user: PackedRgba::rgb(166, 226, 46),        // green
            agent: PackedRgba::rgb(102, 217, 239),      // #66d9ef cyan
            tool: PackedRgba::rgb(253, 151, 31),        // #fd971f orange
            system: PackedRgba::rgb(230, 219, 116),     // #e6db74 yellow
            stripe_even: PackedRgba::rgb(39, 40, 34),
            stripe_odd: PackedRgba::rgb(48, 49, 42),
        }
    }

    pub fn gruvbox_dark() -> Self {
        Self {
            accent: PackedRgba::rgb(250, 189, 47),      // #fabd2f yellow
            accent_alt: PackedRgba::rgb(211, 134, 155), // #d3869b purple
            bg: PackedRgba::rgb(40, 40, 40),            // #282828
            fg: PackedRgba::rgb(235, 219, 178),         // #ebdbb2
            surface: PackedRgba::rgb(50, 48, 47),       // #32302f
            hint: PackedRgba::rgb(146, 131, 116),       // #928374
            border: PackedRgba::rgb(80, 73, 69),        // #504945
            user: PackedRgba::rgb(184, 187, 38),        // #b8bb26 green
            agent: PackedRgba::rgb(131, 165, 152),      // #83a598 aqua
            tool: PackedRgba::rgb(254, 128, 25),        // #fe8019 orange
            system: PackedRgba::rgb(250, 189, 47),      // #fabd2f yellow
            stripe_even: PackedRgba::rgb(40, 40, 40),
            stripe_odd: PackedRgba::rgb(50, 48, 47),
        }
    }

    pub fn one_dark() -> Self {
        Self {
            accent: PackedRgba::rgb(97, 175, 239),      // #61afef blue
            accent_alt: PackedRgba::rgb(198, 120, 221), // #c678dd purple
            bg: PackedRgba::rgb(40, 44, 52),            // #282c34
            fg: PackedRgba::rgb(171, 178, 191),         // #abb2bf
            surface: PackedRgba::rgb(49, 53, 63),       // #31353f
            hint: PackedRgba::rgb(118, 128, 150), // lightened #636d83 (WCAG AA-large on bg+surface)
            border: PackedRgba::rgb(62, 68, 81),  // #3e4451
            user: PackedRgba::rgb(152, 195, 121), // #98c379 green
            agent: PackedRgba::rgb(97, 175, 239), // #61afef blue
            tool: PackedRgba::rgb(229, 192, 123), // #e5c07b yellow
            system: PackedRgba::rgb(224, 108, 117), // #e06c75 red
            stripe_even: PackedRgba::rgb(40, 44, 52),
            stripe_odd: PackedRgba::rgb(49, 53, 63),
        }
    }

    pub fn rose_pine() -> Self {
        Self {
            accent: PackedRgba::rgb(235, 188, 186),     // #ebbcba rose
            accent_alt: PackedRgba::rgb(196, 167, 231), // #c4a7e7 iris
            bg: PackedRgba::rgb(25, 23, 36),            // #191724
            fg: PackedRgba::rgb(224, 222, 244),         // #e0def4
            surface: PackedRgba::rgb(38, 35, 53),       // #26233a
            hint: PackedRgba::rgb(114, 110, 138), // lightened #6e6a86 (WCAG AA-large on surface)
            border: PackedRgba::rgb(57, 53, 82),  // #393552
            user: PackedRgba::rgb(156, 207, 216), // #9ccfd8 foam
            agent: PackedRgba::rgb(196, 167, 231), // #c4a7e7 iris
            tool: PackedRgba::rgb(246, 193, 119), // #f6c177 gold
            system: PackedRgba::rgb(235, 111, 146), // #eb6f92 love
            stripe_even: PackedRgba::rgb(25, 23, 36),
            stripe_odd: PackedRgba::rgb(33, 30, 46),
        }
    }

    pub fn everforest() -> Self {
        Self {
            accent: PackedRgba::rgb(167, 192, 128),     // #a7c080 green
            accent_alt: PackedRgba::rgb(214, 153, 182), // #d699b6 purple
            bg: PackedRgba::rgb(39, 46, 34),            // #272e22
            fg: PackedRgba::rgb(211, 198, 170),         // #d3c6aa
            surface: PackedRgba::rgb(47, 55, 42),       // #2f372a
            hint: PackedRgba::rgb(135, 127, 110), // lightened #7d7564 (WCAG AA-large on surface)
            border: PackedRgba::rgb(68, 77, 60),  // #444d3c
            user: PackedRgba::rgb(131, 192, 146), // #83c092 aqua
            agent: PackedRgba::rgb(124, 195, 210), // #7cc3d2 blue
            tool: PackedRgba::rgb(219, 188, 127), // #dbbc7f yellow
            system: PackedRgba::rgb(230, 126, 128), // #e67e80 red
            stripe_even: PackedRgba::rgb(39, 46, 34),
            stripe_odd: PackedRgba::rgb(47, 55, 42),
        }
    }

    pub fn kanagawa() -> Self {
        Self {
            accent: PackedRgba::rgb(126, 156, 216), // #7e9cd8 crystal blue
            accent_alt: PackedRgba::rgb(149, 127, 184), // #957fb8 oniviolet
            bg: PackedRgba::rgb(31, 31, 40),        // #1f1f28
            fg: PackedRgba::rgb(220, 215, 186),     // #dcd7ba
            surface: PackedRgba::rgb(42, 42, 54),   // #2a2a36
            hint: PackedRgba::rgb(119, 118, 110),   // lightened #727169 (WCAG AA-large on surface)
            border: PackedRgba::rgb(84, 84, 109),   // #54546d
            user: PackedRgba::rgb(152, 187, 108),   // #98bb6c spring green
            agent: PackedRgba::rgb(127, 180, 202),  // #7fb4ca wave blue
            tool: PackedRgba::rgb(255, 169, 98),    // #ffa962 surimi orange
            system: PackedRgba::rgb(195, 64, 67),   // #c34043 autumn red
            stripe_even: PackedRgba::rgb(31, 31, 40),
            stripe_odd: PackedRgba::rgb(42, 42, 54),
        }
    }

    pub fn ayu_mirage() -> Self {
        Self {
            accent: PackedRgba::rgb(115, 210, 222),     // #73d2de
            accent_alt: PackedRgba::rgb(217, 155, 243), // #d99bf3
            bg: PackedRgba::rgb(36, 42, 54),            // #242a36
            fg: PackedRgba::rgb(204, 204, 194),         // #cccac2
            surface: PackedRgba::rgb(44, 51, 64),       // #2c3340
            hint: PackedRgba::rgb(119, 126, 140), // lightened #6b7280 (WCAG AA-large on bg+surface)
            border: PackedRgba::rgb(60, 68, 82),  // #3c4452
            user: PackedRgba::rgb(135, 213, 134), // #87d586
            agent: PackedRgba::rgb(115, 210, 222), // #73d2de
            tool: PackedRgba::rgb(255, 213, 109), // #ffd56d
            system: PackedRgba::rgb(240, 113, 120), // #f07178
            stripe_even: PackedRgba::rgb(36, 42, 54),
            stripe_odd: PackedRgba::rgb(44, 51, 64),
        }
    }

    pub fn nightfox() -> Self {
        Self {
            accent: PackedRgba::rgb(129, 180, 243),     // #81b4f3
            accent_alt: PackedRgba::rgb(174, 140, 211), // #ae8cd3
            bg: PackedRgba::rgb(18, 21, 31),            // #12151f
            fg: PackedRgba::rgb(205, 207, 216),         // #cdcfd8
            surface: PackedRgba::rgb(29, 33, 46),       // #1d212e
            hint: PackedRgba::rgb(106, 108, 122),       // #6a6c7a
            border: PackedRgba::rgb(48, 54, 71),        // #303647
            user: PackedRgba::rgb(129, 200, 152),       // #81c898
            agent: PackedRgba::rgb(129, 180, 243),      // #81b4f3
            tool: PackedRgba::rgb(218, 167, 89),        // #daa759
            system: PackedRgba::rgb(201, 101, 120),     // #c96578
            stripe_even: PackedRgba::rgb(18, 21, 31),
            stripe_odd: PackedRgba::rgb(29, 33, 46),
        }
    }

    pub fn cyberpunk_aurora() -> Self {
        Self {
            accent: PackedRgba::rgb(255, 0, 128),     // #ff0080 neon pink
            accent_alt: PackedRgba::rgb(0, 255, 255), // #00ffff cyan
            bg: PackedRgba::rgb(13, 2, 33),           // #0d0221
            fg: PackedRgba::rgb(224, 210, 255),       // #e0d2ff
            surface: PackedRgba::rgb(22, 10, 48),     // #160a30
            hint: PackedRgba::rgb(120, 100, 160),     // #7864a0
            border: PackedRgba::rgb(60, 30, 100),     // #3c1e64
            user: PackedRgba::rgb(0, 255, 163),       // #00ffa3 neon green
            agent: PackedRgba::rgb(0, 200, 255),      // #00c8ff
            tool: PackedRgba::rgb(255, 213, 0),       // #ffd500
            system: PackedRgba::rgb(255, 51, 102),    // #ff3366
            stripe_even: PackedRgba::rgb(13, 2, 33),
            stripe_odd: PackedRgba::rgb(22, 10, 48),
        }
    }

    pub fn synthwave_84() -> Self {
        Self {
            accent: PackedRgba::rgb(255, 123, 213),     // #ff7bd5 hot pink
            accent_alt: PackedRgba::rgb(114, 241, 223), // #72f1df mint
            bg: PackedRgba::rgb(34, 20, 54),            // #221436
            fg: PackedRgba::rgb(241, 233, 255),         // #f1e9ff
            surface: PackedRgba::rgb(44, 28, 68),       // #2c1c44
            hint: PackedRgba::rgb(130, 115, 165),       // #8273a5
            border: PackedRgba::rgb(70, 45, 100),       // #462d64
            user: PackedRgba::rgb(114, 241, 223),       // #72f1df mint
            agent: PackedRgba::rgb(54, 245, 253),       // #36f5fd
            tool: PackedRgba::rgb(254, 215, 102),       // #fed766
            system: PackedRgba::rgb(254, 73, 99),       // #fe4963
            stripe_even: PackedRgba::rgb(34, 20, 54),
            stripe_odd: PackedRgba::rgb(44, 28, 68),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== TerminalWidth tests ====================

    #[test]
    fn test_terminal_width_from_cols_narrow() {
        assert_eq!(TerminalWidth::from_cols(40), TerminalWidth::Narrow);
        assert_eq!(TerminalWidth::from_cols(79), TerminalWidth::Narrow);
    }

    #[test]
    fn test_terminal_width_from_cols_normal() {
        assert_eq!(TerminalWidth::from_cols(80), TerminalWidth::Normal);
        assert_eq!(TerminalWidth::from_cols(100), TerminalWidth::Normal);
        assert_eq!(TerminalWidth::from_cols(120), TerminalWidth::Normal);
    }

    #[test]
    fn test_terminal_width_from_cols_wide() {
        assert_eq!(TerminalWidth::from_cols(121), TerminalWidth::Wide);
        assert_eq!(TerminalWidth::from_cols(200), TerminalWidth::Wide);
    }

    #[test]
    fn test_terminal_width_border_color() {
        assert_eq!(TerminalWidth::Narrow.border_color(), colors::BORDER_MINIMAL);
        assert_eq!(
            TerminalWidth::Normal.border_color(),
            colors::BORDER_STANDARD
        );
        assert_eq!(
            TerminalWidth::Wide.border_color(),
            colors::BORDER_EMPHASIZED
        );
    }

    #[test]
    fn test_terminal_width_show_decorations() {
        assert!(!TerminalWidth::Narrow.show_decorations());
        assert!(TerminalWidth::Normal.show_decorations());
        assert!(TerminalWidth::Wide.show_decorations());
    }

    #[test]
    fn test_terminal_width_show_extended_info() {
        assert!(!TerminalWidth::Narrow.show_extended_info());
        assert!(!TerminalWidth::Normal.show_extended_info());
        assert!(TerminalWidth::Wide.show_extended_info());
    }

    // ==================== GradientShades tests ====================

    #[test]
    fn test_gradient_shades_header() {
        let shades = GradientShades::header();
        assert_eq!(shades.dark, colors::GRADIENT_HEADER_TOP);
        assert_eq!(shades.mid, colors::GRADIENT_HEADER_MID);
        assert_eq!(shades.light, colors::GRADIENT_HEADER_BOT);
    }

    #[test]
    fn test_gradient_shades_pill() {
        let shades = GradientShades::pill();
        assert_eq!(shades.dark, colors::GRADIENT_PILL_LEFT);
        assert_eq!(shades.mid, colors::GRADIENT_PILL_CENTER);
        assert_eq!(shades.light, colors::GRADIENT_PILL_RIGHT);
    }

    #[test]
    fn test_gradient_shades_styles() {
        let shades = GradientShades::header();
        let (dark, mid, light) = shades.styles();
        assert_eq!(dark.bg, Some(shades.dark));
        assert_eq!(mid.bg, Some(shades.mid));
        assert_eq!(light.bg, Some(shades.light));
    }

    // ==================== AdaptiveBorders tests ====================

    #[test]
    fn test_adaptive_borders_for_width_narrow() {
        let borders = AdaptiveBorders::for_width(60);
        assert_eq!(borders.width_class, TerminalWidth::Narrow);
        assert!(!borders.use_double);
        assert!(!borders.show_corners);
    }

    #[test]
    fn test_adaptive_borders_for_width_normal() {
        let borders = AdaptiveBorders::for_width(100);
        assert_eq!(borders.width_class, TerminalWidth::Normal);
        assert!(!borders.use_double);
        assert!(borders.show_corners);
    }

    #[test]
    fn test_adaptive_borders_for_width_wide() {
        let borders = AdaptiveBorders::for_width(150);
        assert_eq!(borders.width_class, TerminalWidth::Wide);
        assert!(borders.use_double);
        assert!(borders.show_corners);
    }

    #[test]
    fn test_adaptive_borders_focused() {
        let borders = AdaptiveBorders::focused(100);
        assert_eq!(borders.color, colors::BORDER_FOCUS);
    }

    // ==================== ThemePalette tests ====================

    #[test]
    fn test_theme_palette_light() {
        let palette = ThemePalette::light();
        // Light theme should have a light background
        assert_eq!(palette.bg, PackedRgba::rgb(250, 250, 252));
        // And dark foreground
        assert_eq!(palette.fg, PackedRgba::rgb(36, 41, 46));
    }

    #[test]
    fn test_theme_palette_dark() {
        let palette = ThemePalette::dark();
        // Dark theme should have a dark background
        assert_eq!(palette.bg, colors::BG_DEEP);
        // And light foreground
        assert_eq!(palette.fg, colors::TEXT_PRIMARY);
    }

    #[test]
    fn test_theme_palette_catppuccin() {
        let palette = ThemePalette::catppuccin();
        // Check specific Catppuccin colors
        assert_eq!(palette.bg, PackedRgba::rgb(30, 30, 46));
    }

    #[test]
    fn test_theme_palette_dracula() {
        let palette = ThemePalette::dracula();
        assert_eq!(palette.bg, PackedRgba::rgb(40, 42, 54));
    }

    #[test]
    fn test_theme_palette_nord() {
        let palette = ThemePalette::nord();
        assert_eq!(palette.bg, PackedRgba::rgb(46, 52, 64));
    }

    #[test]
    fn test_theme_palette_high_contrast() {
        let palette = ThemePalette::high_contrast();
        // High contrast should use pure black and white
        assert_eq!(palette.bg, PackedRgba::rgb(0, 0, 0));
        assert_eq!(palette.fg, PackedRgba::rgb(255, 255, 255));
    }

    #[test]
    fn test_theme_palette_agent_pane_known_agents() {
        // Test known agent color mappings
        let claude = ThemePalette::agent_pane("claude_code");
        assert_eq!(claude.bg, colors::AGENT_CLAUDE_BG);

        let codex = ThemePalette::agent_pane("codex");
        assert_eq!(codex.bg, colors::AGENT_CODEX_BG);

        let gemini = ThemePalette::agent_pane("gemini_cli");
        assert_eq!(gemini.bg, colors::AGENT_GEMINI_BG);

        let chatgpt = ThemePalette::agent_pane("chatgpt");
        assert_eq!(chatgpt.bg, colors::AGENT_CHATGPT_BG);
    }

    #[test]
    fn test_theme_palette_agent_pane_unknown_agent() {
        let unknown = ThemePalette::agent_pane("unknown_agent");
        assert_eq!(unknown.bg, colors::BG_DEEP);
    }

    #[test]
    fn test_theme_palette_agent_icon() {
        assert_eq!(ThemePalette::agent_icon("codex"), "◆");
        assert_eq!(ThemePalette::agent_icon("claude_code"), "●");
        assert_eq!(ThemePalette::agent_icon("gemini"), "◇");
        assert_eq!(ThemePalette::agent_icon("chatgpt"), "○");
        assert_eq!(ThemePalette::agent_icon("unknown"), "•");
    }

    #[test]
    fn test_theme_palette_role_theme() {
        let palette = ThemePalette::dark();

        let user_theme = palette.role_theme("user");
        assert_eq!(user_theme.fg, palette.user);

        let agent_theme = palette.role_theme("assistant");
        assert_eq!(agent_theme.fg, palette.agent);

        let tool_theme = palette.role_theme("tool");
        assert_eq!(tool_theme.fg, palette.tool);

        let system_theme = palette.role_theme("system");
        assert_eq!(system_theme.fg, palette.system);
    }

    // ==================== ContrastLevel tests ====================

    #[test]
    fn test_contrast_level_from_ratio() {
        assert_eq!(ContrastLevel::from_ratio(2.0), ContrastLevel::Fail);
        assert_eq!(ContrastLevel::from_ratio(3.5), ContrastLevel::AALarge);
        assert_eq!(ContrastLevel::from_ratio(5.0), ContrastLevel::AA);
        assert_eq!(ContrastLevel::from_ratio(8.0), ContrastLevel::AAA);
    }

    #[test]
    fn test_contrast_level_meets() {
        assert!(ContrastLevel::AAA.meets(ContrastLevel::AA));
        assert!(ContrastLevel::AA.meets(ContrastLevel::AALarge));
        assert!(!ContrastLevel::Fail.meets(ContrastLevel::AA));
    }

    #[test]
    fn test_contrast_level_name() {
        assert_eq!(ContrastLevel::AAA.name(), "AAA");
        assert_eq!(ContrastLevel::AA.name(), "AA");
        assert_eq!(ContrastLevel::Fail.name(), "Fail");
    }

    // ==================== Luminance/Contrast tests ====================

    #[test]
    fn test_relative_luminance_black() {
        let lum = relative_luminance(PackedRgba::rgb(0, 0, 0));
        assert!((lum - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_relative_luminance_white() {
        let lum = relative_luminance(PackedRgba::rgb(255, 255, 255));
        assert!((lum - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_relative_luminance_named_colors() {
        // Black should have low luminance
        let black_lum = relative_luminance(PackedRgba::BLACK);
        assert!(black_lum < 0.01);

        // White should have high luminance
        let white_lum = relative_luminance(PackedRgba::WHITE);
        assert!(white_lum > 0.99);
    }

    #[test]
    fn test_contrast_ratio_black_white() {
        let ratio = contrast_ratio(PackedRgba::rgb(255, 255, 255), PackedRgba::rgb(0, 0, 0));
        // Maximum contrast is 21:1
        assert!(ratio > 20.0);
    }

    #[test]
    fn test_contrast_ratio_same_color() {
        let ratio = contrast_ratio(
            PackedRgba::rgb(128, 128, 128),
            PackedRgba::rgb(128, 128, 128),
        );
        // Same color = 1:1 contrast
        assert!((ratio - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_check_contrast() {
        // High contrast pair
        let level = check_contrast(PackedRgba::rgb(255, 255, 255), PackedRgba::rgb(0, 0, 0));
        assert_eq!(level, ContrastLevel::AAA);

        // Low contrast pair (similar grays)
        let level = check_contrast(
            PackedRgba::rgb(100, 100, 100),
            PackedRgba::rgb(120, 120, 120),
        );
        assert_eq!(level, ContrastLevel::Fail);
    }

    #[test]
    fn test_ensure_contrast_already_sufficient() {
        let bg = PackedRgba::rgb(0, 0, 0);
        let fg = PackedRgba::rgb(255, 255, 255);
        let result = ensure_contrast(fg, bg, ContrastLevel::AA);
        // Should return original since contrast is already good
        assert_eq!(result, fg);
    }

    // ==================== ThemePreset tests ====================

    #[test]
    fn test_theme_preset_default() {
        let preset = ThemePreset::default();
        assert_eq!(preset, ThemePreset::TokyoNight);
    }

    #[test]
    fn test_theme_preset_name() {
        assert_eq!(ThemePreset::TokyoNight.name(), "Tokyo Night");
        assert_eq!(ThemePreset::Daylight.name(), "Daylight");
        assert_eq!(ThemePreset::Catppuccin.name(), "Catppuccin Mocha");
        assert_eq!(ThemePreset::Dracula.name(), "Dracula");
        assert_eq!(ThemePreset::Nord.name(), "Nord");
        assert_eq!(ThemePreset::HighContrast.name(), "High Contrast");
    }

    #[test]
    fn test_theme_preset_next_cycles() {
        let mut preset = ThemePreset::TokyoNight;
        preset = preset.next();
        assert_eq!(preset, ThemePreset::Daylight);
        preset = preset.next();
        assert_eq!(preset, ThemePreset::Catppuccin);
        // Cycle through all 19 and verify wrap
        let mut p = ThemePreset::Colorblind;
        p = p.next();
        assert_eq!(p, ThemePreset::TokyoNight);
    }

    #[test]
    fn test_theme_preset_prev_cycles() {
        let mut preset = ThemePreset::TokyoNight;
        preset = preset.prev();
        assert_eq!(preset, ThemePreset::Colorblind);
        preset = preset.prev();
        assert_eq!(preset, ThemePreset::HighContrast);
    }

    #[test]
    fn test_theme_preset_to_palette() {
        let palette = ThemePreset::TokyoNight.to_palette();
        assert_eq!(palette.bg, ThemePalette::dark().bg);

        let palette = ThemePreset::Daylight.to_palette();
        assert_eq!(palette.bg, ThemePalette::light().bg);
    }

    #[test]
    fn test_theme_preset_all() {
        let all = ThemePreset::all();
        assert_eq!(all.len(), 19);
        assert!(all.contains(&ThemePreset::TokyoNight));
        assert!(all.contains(&ThemePreset::Daylight));
    }

    // ==================== Style helper tests ====================

    #[test]
    fn test_chip_style() {
        let palette = ThemePalette::dark();
        let style = chip_style(palette);
        assert_eq!(style.fg, Some(palette.accent_alt));
    }

    #[test]
    fn test_kbd_style() {
        let palette = ThemePalette::dark();
        let style = kbd_style(palette);
        assert_eq!(style.fg, Some(palette.accent));
    }

    #[test]
    fn test_score_style_high() {
        let palette = ThemePalette::dark();
        let style = score_style(9.0, palette);
        assert_eq!(style.fg, Some(colors::STATUS_SUCCESS));
    }

    #[test]
    fn test_score_style_medium() {
        let palette = ThemePalette::dark();
        let style = score_style(6.0, palette);
        assert_eq!(style.fg, Some(palette.accent));
    }

    #[test]
    fn test_score_style_low() {
        let palette = ThemePalette::dark();
        let style = score_style(3.0, palette);
        assert_eq!(style.fg, Some(palette.hint));
    }

    // ==================== RoleTheme tests ====================

    #[test]
    fn test_role_theme_has_all_fields() {
        let palette = ThemePalette::dark();
        let theme = palette.role_theme("user");
        // Verify all fields are set
        assert_ne!(theme.fg, PackedRgba::TRANSPARENT);
        assert_ne!(theme.bg, PackedRgba::TRANSPARENT);
        assert_ne!(theme.border, PackedRgba::TRANSPARENT);
        assert_ne!(theme.badge, PackedRgba::TRANSPARENT);
    }

    // ==================== PaneTheme tests ====================

    #[test]
    fn test_pane_theme_has_all_fields() {
        let pane = ThemePalette::agent_pane("claude");
        assert_ne!(pane.fg, PackedRgba::TRANSPARENT);
        assert_ne!(pane.bg, PackedRgba::TRANSPARENT);
        assert_ne!(pane.accent, PackedRgba::TRANSPARENT);
    }

    // -- agent/role coherence tests (2dccg.10.2) --

    const KNOWN_AGENTS: &[&str] = &[
        "claude_code",
        "codex",
        "cline",
        "gemini",
        "amp",
        "aider",
        "cursor",
        "chatgpt",
        "opencode",
        "pi_agent",
    ];

    #[test]
    fn agent_accent_colors_are_pairwise_distinct() {
        let accents: Vec<(&str, PackedRgba)> = KNOWN_AGENTS
            .iter()
            .map(|a| (*a, ThemePalette::agent_pane(a).accent))
            .collect();

        for i in 0..accents.len() {
            for j in (i + 1)..accents.len() {
                let (name_a, color_a) = accents[i];
                let (name_b, color_b) = accents[j];
                assert_ne!(
                    color_a, color_b,
                    "Agents {name_a} and {name_b} have identical accent colors — \
                     users cannot distinguish them"
                );
            }
        }
    }

    #[test]
    fn agent_background_colors_are_pairwise_distinct() {
        let bgs: Vec<(&str, PackedRgba)> = KNOWN_AGENTS
            .iter()
            .map(|a| (*a, ThemePalette::agent_pane(a).bg))
            .collect();

        for i in 0..bgs.len() {
            for j in (i + 1)..bgs.len() {
                let (name_a, bg_a) = bgs[i];
                let (name_b, bg_b) = bgs[j];
                // codex and pi_agent intentionally share AGENT_CODEX_BG
                if (name_a == "codex" && name_b == "pi_agent")
                    || (name_a == "pi_agent" && name_b == "codex")
                {
                    continue;
                }
                assert_ne!(
                    bg_a, bg_b,
                    "Agents {name_a} and {name_b} have identical background colors"
                );
            }
        }
    }

    #[test]
    fn agent_icons_are_pairwise_distinct() {
        let icons: Vec<(&str, &str)> = KNOWN_AGENTS
            .iter()
            .map(|a| (*a, ThemePalette::agent_icon(a)))
            .collect();

        for i in 0..icons.len() {
            for j in (i + 1)..icons.len() {
                let (name_a, icon_a) = icons[i];
                let (name_b, icon_b) = icons[j];
                assert_ne!(
                    icon_a, icon_b,
                    "Agents {name_a} and {name_b} have identical icons"
                );
            }
        }
    }

    #[test]
    fn agent_icons_are_single_char_glyphs() {
        for agent in KNOWN_AGENTS {
            let icon = ThemePalette::agent_icon(agent);
            assert_eq!(
                icon.chars().count(),
                1,
                "Agent {agent} icon should be a single-width glyph for layout stability"
            );
        }
    }

    #[test]
    fn unknown_agent_falls_back_gracefully() {
        let pane = ThemePalette::agent_pane("nonexistent_agent");
        // Should not panic and should produce a usable theme.
        assert_ne!(pane.fg, PackedRgba::TRANSPARENT);
        assert_ne!(pane.bg, PackedRgba::TRANSPARENT);
        assert_ne!(pane.accent, PackedRgba::TRANSPARENT);

        let icon = ThemePalette::agent_icon("nonexistent_agent");
        assert!(!icon.is_empty(), "unknown agent should get a fallback icon");
    }

    #[test]
    fn role_colors_are_pairwise_distinct_in_palette() {
        let palette = ThemePalette::dark();
        let roles = [
            ("user", palette.user),
            ("agent", palette.agent),
            ("tool", palette.tool),
            ("system", palette.system),
        ];
        for i in 0..roles.len() {
            for j in (i + 1)..roles.len() {
                let (name_a, color_a) = roles[i];
                let (name_b, color_b) = roles[j];
                assert_ne!(
                    color_a, color_b,
                    "ThemePalette::dark() role {name_a} and {name_b} have identical colors"
                );
            }
        }
    }
}
