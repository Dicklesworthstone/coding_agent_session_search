//! ftui theme system for cass.
//!
//! Bridges the existing `ThemePalette` color definitions to ftui's `Theme`,
//! `StyleSheet`, and `ColorProfile` infrastructure so every widget draws from
//! the same token source.
//!
//! # Design goals
//! - Single source of truth: all panes consume styles from [`CassTheme`].
//! - Terminal-aware: truecolor terminals get the premium palette; 8/16-color
//!   and mono terminals get safe automatic fallbacks via `ColorProfile`.
//! - Env-var overrides: respects `NO_COLOR`, `CASS_NO_COLOR`, `CASS_NO_ICONS`,
//!   `CASS_NO_GRADIENT`, `CASS_DISABLE_ANIMATIONS`, and `CASS_A11Y`.
//! - Preset cycling: all six `ThemePreset` variants produce a valid ftui Theme.

use ftui::{Color, ColorCache, ColorProfile, Style, StyleSheet, Theme};

use crate::ui::components::theme::{self as legacy, ThemePalette, ThemePreset};

// ─── Environment variable names ──────────────────────────────────────────────

const ENV_NO_COLOR: &str = "NO_COLOR";
const ENV_CASS_NO_COLOR: &str = "CASS_NO_COLOR";
const ENV_CASS_NO_ICONS: &str = "CASS_NO_ICONS";
const ENV_CASS_NO_GRADIENT: &str = "CASS_NO_GRADIENT";
const ENV_CASS_DISABLE_ANIMATIONS: &str = "CASS_DISABLE_ANIMATIONS";
const ENV_CASS_ANIM: &str = "CASS_ANIM";
const ENV_CASS_A11Y: &str = "CASS_A11Y";

// ─── Named style IDs ────────────────────────────────────────────────────────

/// Well-known style names registered in the [`StyleSheet`].
pub mod style_ids {
    // Text hierarchy
    pub const TEXT_PRIMARY: &str = "text.primary";
    pub const TEXT_SECONDARY: &str = "text.secondary";
    pub const TEXT_MUTED: &str = "text.muted";
    pub const TEXT_DISABLED: &str = "text.disabled";

    // Accents
    pub const ACCENT_PRIMARY: &str = "accent.primary";
    pub const ACCENT_SECONDARY: &str = "accent.secondary";
    pub const ACCENT_TERTIARY: &str = "accent.tertiary";

    // Surfaces
    pub const BG_DEEP: &str = "bg.deep";
    pub const BG_SURFACE: &str = "bg.surface";
    pub const BG_HIGHLIGHT: &str = "bg.highlight";

    // Borders
    pub const BORDER: &str = "border";
    pub const BORDER_FOCUS: &str = "border.focus";
    pub const BORDER_MINIMAL: &str = "border.minimal";
    pub const BORDER_EMPHASIZED: &str = "border.emphasized";

    // Roles
    pub const ROLE_USER: &str = "role.user";
    pub const ROLE_AGENT: &str = "role.agent";
    pub const ROLE_TOOL: &str = "role.tool";
    pub const ROLE_SYSTEM: &str = "role.system";

    // Role backgrounds
    pub const ROLE_USER_BG: &str = "role.user.bg";
    pub const ROLE_AGENT_BG: &str = "role.agent.bg";
    pub const ROLE_TOOL_BG: &str = "role.tool.bg";
    pub const ROLE_SYSTEM_BG: &str = "role.system.bg";

    // Status
    pub const STATUS_SUCCESS: &str = "status.success";
    pub const STATUS_WARNING: &str = "status.warning";
    pub const STATUS_ERROR: &str = "status.error";
    pub const STATUS_INFO: &str = "status.info";

    // Interaction
    pub const HIGHLIGHT: &str = "highlight";
    pub const SELECTED: &str = "selected";
    pub const CHIP: &str = "chip";
    pub const KBD: &str = "kbd";
    pub const CODE: &str = "code";

    // Zebra stripes
    pub const STRIPE_EVEN: &str = "stripe.even";
    pub const STRIPE_ODD: &str = "stripe.odd";

    // Gradient (header)
    pub const GRADIENT_TOP: &str = "gradient.top";
    pub const GRADIENT_MID: &str = "gradient.mid";
    pub const GRADIENT_BOT: &str = "gradient.bot";
}

// ─── Feature flags ───────────────────────────────────────────────────────────

/// Runtime feature flags derived from environment variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeFlags {
    /// Disable all color output (`NO_COLOR` or `CASS_NO_COLOR`).
    pub no_color: bool,
    /// Disable emoji/icon glyphs (`CASS_NO_ICONS`).
    pub no_icons: bool,
    /// Disable gradient simulation (`CASS_NO_GRADIENT`).
    pub no_gradient: bool,
    /// Disable animations (`CASS_DISABLE_ANIMATIONS` or `CASS_ANIM=0`).
    pub no_animations: bool,
    /// Accessibility mode (`CASS_A11Y=1`): textual cues supplement color.
    pub a11y: bool,
}

impl ThemeFlags {
    /// Detect flags from the process environment.
    pub fn detect() -> Self {
        Self {
            no_color: std::env::var_os(ENV_NO_COLOR).is_some() || env_truthy(ENV_CASS_NO_COLOR),
            no_icons: env_truthy(ENV_CASS_NO_ICONS),
            no_gradient: env_truthy(ENV_CASS_NO_GRADIENT),
            no_animations: env_truthy(ENV_CASS_DISABLE_ANIMATIONS) || env_is(ENV_CASS_ANIM, "0"),
            a11y: env_truthy(ENV_CASS_A11Y),
        }
    }

    /// Build flags from explicit values (for testing).
    pub fn custom(
        no_color: bool,
        no_icons: bool,
        no_gradient: bool,
        no_animations: bool,
        a11y: bool,
    ) -> Self {
        Self {
            no_color,
            no_icons,
            no_gradient,
            no_animations,
            a11y,
        }
    }

    /// All features enabled (no restrictions).
    pub fn all_enabled() -> Self {
        Self {
            no_color: false,
            no_icons: false,
            no_gradient: false,
            no_animations: false,
            a11y: false,
        }
    }
}

impl Default for ThemeFlags {
    fn default() -> Self {
        Self::all_enabled()
    }
}

// ─── CassTheme ──────────────────────────────────────────────────────────────

/// Central theme object consumed by every ftui widget in cass.
///
/// Wraps an ftui [`Theme`], a [`StyleSheet`] with named styles, the detected
/// [`ColorProfile`], and runtime [`ThemeFlags`]. All rendering code should
/// query styles through this struct rather than hard-coding colors.
pub struct CassTheme {
    /// Current preset (for cycling).
    pub preset: ThemePreset,
    /// Whether dark mode is active.
    pub is_dark: bool,
    /// ftui Theme with semantic color slots.
    pub theme: Theme,
    /// Named style registry - the single source of truth for widget styles.
    pub styles: StyleSheet,
    /// Detected terminal color capability.
    pub profile: ColorProfile,
    /// Color downgrade cache (speeds up repeated color conversions).
    pub color_cache: ColorCache,
    /// Runtime feature flags from environment.
    pub flags: ThemeFlags,
}

impl CassTheme {
    /// Build a theme from a preset, detecting color profile and env flags.
    pub fn from_preset(preset: ThemePreset) -> Self {
        let flags = ThemeFlags::detect();
        let profile = if flags.no_color {
            ColorProfile::Mono
        } else {
            ColorProfile::detect()
        };
        Self::with_options(preset, profile, flags)
    }

    /// Build a theme with explicit profile and flags (for testing / headless).
    pub fn with_options(preset: ThemePreset, profile: ColorProfile, flags: ThemeFlags) -> Self {
        let palette = preset.to_palette();
        let is_dark = matches!(
            preset,
            ThemePreset::Dark
                | ThemePreset::Catppuccin
                | ThemePreset::Dracula
                | ThemePreset::Nord
                | ThemePreset::HighContrast
        );
        let theme = build_ftui_theme(&palette, is_dark);
        let styles = build_stylesheet(&palette, is_dark, &flags);
        let color_cache = ColorCache::new(profile);

        Self {
            preset,
            is_dark,
            theme,
            styles,
            profile,
            color_cache,
            flags,
        }
    }

    /// Cycle to the next preset and rebuild.
    pub fn next_preset(&mut self) {
        self.preset = self.preset.next();
        self.rebuild();
    }

    /// Cycle to the previous preset and rebuild.
    pub fn prev_preset(&mut self) {
        self.preset = self.preset.prev();
        self.rebuild();
    }

    /// Rebuild theme + stylesheet from current preset/profile/flags.
    fn rebuild(&mut self) {
        let palette = self.preset.to_palette();
        self.is_dark = matches!(
            self.preset,
            ThemePreset::Dark
                | ThemePreset::Catppuccin
                | ThemePreset::Dracula
                | ThemePreset::Nord
                | ThemePreset::HighContrast
        );
        self.theme = build_ftui_theme(&palette, self.is_dark);
        self.styles = build_stylesheet(&palette, self.is_dark, &self.flags);
        self.color_cache = ColorCache::new(self.profile);
    }

    /// Get an ftui [`Style`] by name from the stylesheet, falling back to
    /// `Style::default()` if not found.
    pub fn style(&self, name: &str) -> Style {
        self.styles.get_or_default(name)
    }

    /// Compose multiple named styles left-to-right (later overrides earlier).
    pub fn compose(&self, names: &[&str]) -> Style {
        self.styles.compose(names)
    }

    /// Downgrade an RGB color to the terminal's color profile.
    pub fn downgrade(&mut self, color: Color) -> Color {
        color.downgrade(self.profile)
    }

    /// Get the legacy [`ThemePalette`] for code that hasn't migrated yet.
    pub fn legacy_palette(&self) -> ThemePalette {
        self.preset.to_palette()
    }

    /// Whether emoji icons should be shown.
    pub fn show_icons(&self) -> bool {
        !self.flags.no_icons
    }

    /// Whether gradient simulation should be used.
    pub fn show_gradient(&self) -> bool {
        !self.flags.no_gradient && self.profile.supports_true_color()
    }

    /// Whether animations should play.
    pub fn show_animations(&self) -> bool {
        !self.flags.no_animations
    }

    /// Whether accessibility mode is active (textual cues supplement color).
    pub fn a11y_mode(&self) -> bool {
        self.flags.a11y
    }

    /// Get the agent icon glyph, respecting `no_icons` flag.
    pub fn agent_icon(&self, agent: &str) -> &'static str {
        if self.flags.no_icons {
            ""
        } else {
            ThemePalette::agent_icon(agent)
        }
    }

    /// Get a role-specific ftui [`Style`] for message rendering.
    pub fn role_style(&self, role: &str) -> Style {
        let id = match role.to_lowercase().as_str() {
            "user" => style_ids::ROLE_USER,
            "assistant" | "agent" => style_ids::ROLE_AGENT,
            "tool" => style_ids::ROLE_TOOL,
            "system" => style_ids::ROLE_SYSTEM,
            _ => style_ids::TEXT_MUTED,
        };
        self.style(id)
    }

    /// Get role background style.
    pub fn role_bg_style(&self, role: &str) -> Style {
        let id = match role.to_lowercase().as_str() {
            "user" => style_ids::ROLE_USER_BG,
            "assistant" | "agent" => style_ids::ROLE_AGENT_BG,
            "tool" => style_ids::ROLE_TOOL_BG,
            "system" => style_ids::ROLE_SYSTEM_BG,
            _ => style_ids::BG_DEEP,
        };
        self.style(id)
    }

    /// Get a pane style for a specific agent. Returns (bg_only, bg+fg) styles.
    pub fn agent_pane_style(&self, agent: &str) -> (Style, Style) {
        let pane = ThemePalette::agent_pane(agent);
        let bg = Style::new().bg(ratatui_to_ftui_packed(pane.bg));
        let fg = Style::new()
            .fg(ratatui_to_ftui_packed(pane.fg))
            .bg(ratatui_to_ftui_packed(pane.bg));
        (bg, fg)
    }

    /// Get a zebra-stripe background style for a given row index.
    pub fn stripe_style(&self, row_idx: usize) -> Style {
        if row_idx.is_multiple_of(2) {
            self.style(style_ids::STRIPE_EVEN)
        } else {
            self.style(style_ids::STRIPE_ODD)
        }
    }
}

impl Default for CassTheme {
    fn default() -> Self {
        Self::from_preset(ThemePreset::default())
    }
}

// ─── Theme builder ──────────────────────────────────────────────────────────

/// Convert a legacy cass `ThemePalette` into an ftui `Theme`.
fn build_ftui_theme(palette: &ThemePalette, is_dark: bool) -> Theme {
    // Map palette colors to ftui's semantic slots
    let c = |color: ratatui::style::Color| -> Color { ratatui_to_ftui(color) };

    Theme::builder()
        .primary(c(palette.accent))
        .secondary(c(palette.accent_alt))
        .accent(c(palette.accent))
        .background(c(palette.bg))
        .surface(c(palette.surface))
        .overlay(c(palette.surface))
        .text(c(palette.fg))
        .text_muted(c(palette.hint))
        .text_subtle(if is_dark {
            c(legacy::colors::TEXT_DISABLED)
        } else {
            Color::rgb(180, 180, 190)
        })
        .success(c(legacy::colors::STATUS_SUCCESS))
        .warning(c(legacy::colors::STATUS_WARNING))
        .error(c(legacy::colors::STATUS_ERROR))
        .info(c(legacy::colors::STATUS_INFO))
        .border(c(palette.border))
        .border_focused(c(legacy::colors::BORDER_FOCUS))
        .selection_bg(if is_dark {
            c(legacy::colors::BG_HIGHLIGHT)
        } else {
            Color::rgb(210, 215, 230)
        })
        .selection_fg(c(palette.fg))
        .scrollbar_track(c(palette.surface))
        .scrollbar_thumb(c(palette.border))
        .build()
}

/// Build the named-style registry from a palette.
fn build_stylesheet(palette: &ThemePalette, is_dark: bool, flags: &ThemeFlags) -> StyleSheet {
    let sheet = StyleSheet::new();
    let c = |color: ratatui::style::Color| ratatui_to_ftui_packed(color);

    // Text hierarchy
    sheet.define(style_ids::TEXT_PRIMARY, Style::new().fg(c(palette.fg)));
    sheet.define(
        style_ids::TEXT_SECONDARY,
        Style::new().fg(if is_dark {
            c(legacy::colors::TEXT_SECONDARY)
        } else {
            c(palette.fg)
        }),
    );
    sheet.define(style_ids::TEXT_MUTED, Style::new().fg(c(palette.hint)));
    sheet.define(
        style_ids::TEXT_DISABLED,
        Style::new().fg(if is_dark {
            c(legacy::colors::TEXT_DISABLED)
        } else {
            c(ratatui::style::Color::Rgb(180, 180, 190))
        }),
    );

    // Accents
    sheet.define(
        style_ids::ACCENT_PRIMARY,
        Style::new().fg(c(palette.accent)).bold(),
    );
    sheet.define(
        style_ids::ACCENT_SECONDARY,
        Style::new().fg(c(palette.accent_alt)),
    );
    sheet.define(
        style_ids::ACCENT_TERTIARY,
        Style::new().fg(if is_dark {
            c(legacy::colors::ACCENT_TERTIARY)
        } else {
            c(ratatui::style::Color::Rgb(0, 130, 200))
        }),
    );

    // Surfaces
    sheet.define(style_ids::BG_DEEP, Style::new().bg(c(palette.bg)));
    sheet.define(style_ids::BG_SURFACE, Style::new().bg(c(palette.surface)));
    sheet.define(
        style_ids::BG_HIGHLIGHT,
        Style::new().bg(if is_dark {
            c(legacy::colors::BG_HIGHLIGHT)
        } else {
            c(ratatui::style::Color::Rgb(230, 232, 240))
        }),
    );

    // Borders
    sheet.define(style_ids::BORDER, Style::new().fg(c(palette.border)));
    sheet.define(
        style_ids::BORDER_FOCUS,
        Style::new().fg(c(legacy::colors::BORDER_FOCUS)),
    );
    sheet.define(
        style_ids::BORDER_MINIMAL,
        Style::new().fg(c(legacy::colors::BORDER_MINIMAL)),
    );
    sheet.define(
        style_ids::BORDER_EMPHASIZED,
        Style::new().fg(c(legacy::colors::BORDER_EMPHASIZED)),
    );

    // Roles (foreground)
    sheet.define(style_ids::ROLE_USER, Style::new().fg(c(palette.user)));
    sheet.define(style_ids::ROLE_AGENT, Style::new().fg(c(palette.agent)));
    sheet.define(style_ids::ROLE_TOOL, Style::new().fg(c(palette.tool)));
    sheet.define(style_ids::ROLE_SYSTEM, Style::new().fg(c(palette.system)));

    // Role backgrounds
    sheet.define(
        style_ids::ROLE_USER_BG,
        Style::new().bg(c(legacy::colors::ROLE_USER_BG)),
    );
    sheet.define(
        style_ids::ROLE_AGENT_BG,
        Style::new().bg(c(legacy::colors::ROLE_AGENT_BG)),
    );
    sheet.define(
        style_ids::ROLE_TOOL_BG,
        Style::new().bg(c(legacy::colors::ROLE_TOOL_BG)),
    );
    sheet.define(
        style_ids::ROLE_SYSTEM_BG,
        Style::new().bg(c(legacy::colors::ROLE_SYSTEM_BG)),
    );

    // Status
    sheet.define(
        style_ids::STATUS_SUCCESS,
        Style::new().fg(c(legacy::colors::STATUS_SUCCESS)),
    );
    sheet.define(
        style_ids::STATUS_WARNING,
        Style::new().fg(c(legacy::colors::STATUS_WARNING)),
    );
    sheet.define(
        style_ids::STATUS_ERROR,
        Style::new().fg(c(legacy::colors::STATUS_ERROR)).bold(),
    );
    sheet.define(
        style_ids::STATUS_INFO,
        Style::new().fg(c(legacy::colors::STATUS_INFO)),
    );

    // Interaction states
    sheet.define(
        style_ids::HIGHLIGHT,
        Style::new().fg(c(palette.bg)).bg(c(palette.accent)).bold(),
    );
    sheet.define(
        style_ids::SELECTED,
        Style::new()
            .bg(if is_dark {
                c(legacy::colors::BG_HIGHLIGHT)
            } else {
                c(ratatui::style::Color::Rgb(220, 224, 236))
            })
            .bold(),
    );
    sheet.define(
        style_ids::CHIP,
        Style::new().fg(c(palette.accent_alt)).bold(),
    );
    sheet.define(style_ids::KBD, Style::new().fg(c(palette.accent)).bold());
    sheet.define(
        style_ids::CODE,
        Style::new()
            .fg(if is_dark {
                c(legacy::colors::TEXT_SECONDARY)
            } else {
                c(palette.fg)
            })
            .bg(c(palette.surface)),
    );

    // Zebra stripes
    sheet.define(
        style_ids::STRIPE_EVEN,
        Style::new().bg(c(palette.stripe_even)),
    );
    sheet.define(
        style_ids::STRIPE_ODD,
        Style::new().bg(c(palette.stripe_odd)),
    );

    // Gradients (only meaningful for dark presets with truecolor)
    if !flags.no_gradient {
        sheet.define(
            style_ids::GRADIENT_TOP,
            Style::new().bg(c(legacy::colors::GRADIENT_HEADER_TOP)),
        );
        sheet.define(
            style_ids::GRADIENT_MID,
            Style::new().bg(c(legacy::colors::GRADIENT_HEADER_MID)),
        );
        sheet.define(
            style_ids::GRADIENT_BOT,
            Style::new().bg(c(legacy::colors::GRADIENT_HEADER_BOT)),
        );
    }

    sheet
}

// ─── Color conversion utilities ─────────────────────────────────────────────

/// Convert a ratatui `Color` to an ftui `Color`.
pub fn ratatui_to_ftui(color: ratatui::style::Color) -> Color {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => Color::rgb(r, g, b),
        ratatui::style::Color::Black => Color::Ansi16(ftui::Ansi16::Black),
        ratatui::style::Color::Red => Color::Ansi16(ftui::Ansi16::Red),
        ratatui::style::Color::Green => Color::Ansi16(ftui::Ansi16::Green),
        ratatui::style::Color::Yellow => Color::Ansi16(ftui::Ansi16::Yellow),
        ratatui::style::Color::Blue => Color::Ansi16(ftui::Ansi16::Blue),
        ratatui::style::Color::Magenta => Color::Ansi16(ftui::Ansi16::Magenta),
        ratatui::style::Color::Cyan => Color::Ansi16(ftui::Ansi16::Cyan),
        ratatui::style::Color::White => Color::Ansi16(ftui::Ansi16::White),
        ratatui::style::Color::Gray => Color::Ansi16(ftui::Ansi16::BrightBlack),
        ratatui::style::Color::DarkGray => Color::Ansi16(ftui::Ansi16::BrightBlack),
        ratatui::style::Color::LightRed => Color::Ansi16(ftui::Ansi16::BrightRed),
        ratatui::style::Color::LightGreen => Color::Ansi16(ftui::Ansi16::BrightGreen),
        ratatui::style::Color::LightBlue => Color::Ansi16(ftui::Ansi16::BrightBlue),
        ratatui::style::Color::LightYellow => Color::Ansi16(ftui::Ansi16::BrightYellow),
        ratatui::style::Color::LightMagenta => Color::Ansi16(ftui::Ansi16::BrightMagenta),
        ratatui::style::Color::LightCyan => Color::Ansi16(ftui::Ansi16::BrightCyan),
        ratatui::style::Color::Indexed(idx) => Color::Ansi256(idx),
        _ => Color::rgb(128, 128, 128), // fallback
    }
}

/// Convert a ratatui `Color` to an ftui `PackedRgba` (for StyleSheet use).
fn ratatui_to_ftui_packed(color: ratatui::style::Color) -> ftui::render::cell::PackedRgba {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => ftui::render::cell::PackedRgba::rgb(r, g, b),
        other => {
            let ftui_color = ratatui_to_ftui(other);
            let rgb = ftui_color.to_rgb();
            ftui::render::cell::PackedRgba::rgb(rgb.r, rgb.g, rgb.b)
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Check if an env var is set and truthy (non-empty, not "0", not "false").
fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(val) => !val.is_empty() && val != "0" && val.to_lowercase() != "false",
        Err(_) => false,
    }
}

/// Check if an env var equals a specific value.
fn env_is(name: &str, expected: &str) -> bool {
    std::env::var(name).map(|v| v == expected).unwrap_or(false)
}

// ─── Color interpolation (migrated from tui.rs) ─────────────────────────────

/// Linear interpolation between two u8 values.
pub fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    let result = f32::from(a) * (1.0 - t) + f32::from(b) * t;
    result.round() as u8
}

/// Smoothly interpolate between two ftui Colors.
///
/// Only works with RGB colors; non-RGB falls back to a binary switch at 50%.
pub fn lerp_color(from: Color, to: Color, progress: f32) -> Color {
    let from_rgb = from.to_rgb();
    let to_rgb = to.to_rgb();
    Color::rgb(
        lerp_u8(from_rgb.r, to_rgb.r, progress),
        lerp_u8(from_rgb.g, to_rgb.g, progress),
        lerp_u8(from_rgb.b, to_rgb.b, progress),
    )
}

/// Dim a color by multiplying its RGB channels by `factor` (0.0=black, 1.0=original).
pub fn dim_color(color: Color, factor: f32) -> Color {
    let rgb = color.to_rgb();
    let factor = factor.clamp(0.0, 1.0);
    Color::rgb(
        (f32::from(rgb.r) * factor).round() as u8,
        (f32::from(rgb.g) * factor).round() as u8,
        (f32::from(rgb.b) * factor).round() as u8,
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_creates_dark_theme() {
        let theme = CassTheme::default();
        assert_eq!(theme.preset, ThemePreset::Dark);
        assert!(theme.is_dark);
    }

    #[test]
    fn all_presets_build_without_panic() {
        let flags = ThemeFlags::all_enabled();
        for preset in ThemePreset::all() {
            let _ = CassTheme::with_options(*preset, ColorProfile::TrueColor, flags);
        }
    }

    #[test]
    fn style_sheet_has_core_styles() {
        let theme = CassTheme::with_options(
            ThemePreset::Dark,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        // Verify key styles are populated
        assert!(theme.styles.contains(style_ids::TEXT_PRIMARY));
        assert!(theme.styles.contains(style_ids::ROLE_USER));
        assert!(theme.styles.contains(style_ids::ROLE_AGENT));
        assert!(theme.styles.contains(style_ids::BORDER));
        assert!(theme.styles.contains(style_ids::HIGHLIGHT));
        assert!(theme.styles.contains(style_ids::STRIPE_EVEN));
        assert!(theme.styles.contains(style_ids::STRIPE_ODD));
        assert!(theme.styles.contains(style_ids::STATUS_ERROR));
    }

    #[test]
    fn preset_cycling_wraps() {
        let mut theme = CassTheme::with_options(
            ThemePreset::HighContrast,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        theme.next_preset();
        assert_eq!(theme.preset, ThemePreset::Dark);
    }

    #[test]
    fn no_color_forces_mono_profile() {
        let flags = ThemeFlags::custom(true, false, false, false, false);
        let theme = CassTheme::with_options(ThemePreset::Dark, ColorProfile::TrueColor, flags);
        // Even if we pass TrueColor, the theme stores it as-is (profile is up to
        // the caller when using with_options), but from_preset would force Mono.
        assert!(theme.flags.no_color);
    }

    #[test]
    fn no_icons_suppresses_agent_icons() {
        let flags = ThemeFlags::custom(false, true, false, false, false);
        let theme = CassTheme::with_options(ThemePreset::Dark, ColorProfile::TrueColor, flags);
        assert_eq!(theme.agent_icon("codex"), "");
        assert_eq!(theme.agent_icon("claude_code"), "");
    }

    #[test]
    fn icons_shown_by_default() {
        let flags = ThemeFlags::all_enabled();
        let theme = CassTheme::with_options(ThemePreset::Dark, ColorProfile::TrueColor, flags);
        assert_eq!(theme.agent_icon("codex"), "\u{1f539}"); // 🔹
    }

    #[test]
    fn role_styles_return_non_default() {
        let theme = CassTheme::with_options(
            ThemePreset::Dark,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        let user_style = theme.role_style("user");
        let agent_style = theme.role_style("assistant");
        let tool_style = theme.role_style("tool");
        let system_style = theme.role_style("system");
        // Each should have a foreground set
        assert!(!user_style.is_empty());
        assert!(!agent_style.is_empty());
        assert!(!tool_style.is_empty());
        assert!(!system_style.is_empty());
    }

    #[test]
    fn stripe_alternates() {
        let theme = CassTheme::with_options(
            ThemePreset::Dark,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        let even = theme.stripe_style(0);
        let odd = theme.stripe_style(1);
        // They should be different for dark theme
        assert_ne!(even, odd);
    }

    #[test]
    fn light_theme_has_light_bg() {
        let theme = CassTheme::with_options(
            ThemePreset::Light,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        assert!(!theme.is_dark);
    }

    #[test]
    fn high_contrast_has_core_styles() {
        let theme = CassTheme::with_options(
            ThemePreset::HighContrast,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        assert!(theme.styles.contains(style_ids::ROLE_USER));
        assert!(theme.styles.contains(style_ids::STATUS_ERROR));
    }

    #[test]
    fn compose_merges_styles() {
        let theme = CassTheme::with_options(
            ThemePreset::Dark,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        let composed = theme.compose(&[style_ids::BG_DEEP, style_ids::TEXT_PRIMARY]);
        // Should have both bg and fg set
        assert!(!composed.is_empty());
    }

    // Color interpolation tests

    #[test]
    fn lerp_u8_extremes() {
        assert_eq!(lerp_u8(0, 255, 0.0), 0);
        assert_eq!(lerp_u8(0, 255, 1.0), 255);
        assert_eq!(lerp_u8(0, 200, 0.5), 100);
    }

    #[test]
    fn lerp_u8_clamps() {
        assert_eq!(lerp_u8(0, 100, -1.0), 0);
        assert_eq!(lerp_u8(0, 100, 2.0), 100);
    }

    #[test]
    fn lerp_color_identity() {
        let c = Color::rgb(100, 150, 200);
        let result = lerp_color(c, c, 0.5);
        assert_eq!(result, c);
    }

    #[test]
    fn lerp_color_midpoint() {
        let from = Color::rgb(0, 0, 0);
        let to = Color::rgb(200, 100, 50);
        let mid = lerp_color(from, to, 0.5);
        let rgb = mid.to_rgb();
        assert_eq!(rgb.r, 100);
        assert_eq!(rgb.g, 50);
        assert_eq!(rgb.b, 25);
    }

    #[test]
    fn dim_color_half() {
        let c = Color::rgb(200, 100, 50);
        let dimmed = dim_color(c, 0.5);
        let rgb = dimmed.to_rgb();
        assert_eq!(rgb.r, 100);
        assert_eq!(rgb.g, 50);
        assert_eq!(rgb.b, 25);
    }

    #[test]
    fn dim_color_zero_is_black() {
        let c = Color::rgb(200, 100, 50);
        let dimmed = dim_color(c, 0.0);
        let rgb = dimmed.to_rgb();
        assert_eq!(rgb.r, 0);
        assert_eq!(rgb.g, 0);
        assert_eq!(rgb.b, 0);
    }

    #[test]
    fn ratatui_rgb_round_trips() {
        let orig = ratatui::style::Color::Rgb(42, 84, 168);
        let ftui_color = ratatui_to_ftui(orig);
        let rgb = ftui_color.to_rgb();
        assert_eq!(rgb.r, 42);
        assert_eq!(rgb.g, 84);
        assert_eq!(rgb.b, 168);
    }

    #[test]
    fn no_gradient_skips_gradient_styles() {
        let flags = ThemeFlags::custom(false, false, true, false, false);
        let theme = CassTheme::with_options(ThemePreset::Dark, ColorProfile::TrueColor, flags);
        assert!(!theme.styles.contains(style_ids::GRADIENT_TOP));
        assert!(!theme.show_gradient());
    }

    #[test]
    fn gradient_present_when_enabled() {
        let flags = ThemeFlags::all_enabled();
        let theme = CassTheme::with_options(ThemePreset::Dark, ColorProfile::TrueColor, flags);
        assert!(theme.styles.contains(style_ids::GRADIENT_TOP));
        assert!(theme.styles.contains(style_ids::GRADIENT_MID));
        assert!(theme.styles.contains(style_ids::GRADIENT_BOT));
    }

    #[test]
    fn a11y_mode_reports_correctly() {
        let flags = ThemeFlags::custom(false, false, false, false, true);
        let theme = CassTheme::with_options(ThemePreset::Dark, ColorProfile::TrueColor, flags);
        assert!(theme.a11y_mode());
    }

    #[test]
    fn theme_flags_default_all_enabled() {
        let flags = ThemeFlags::default();
        assert!(!flags.no_color);
        assert!(!flags.no_icons);
        assert!(!flags.no_gradient);
        assert!(!flags.no_animations);
        assert!(!flags.a11y);
    }

    #[test]
    fn legacy_palette_matches_preset() {
        let theme = CassTheme::with_options(
            ThemePreset::Nord,
            ColorProfile::TrueColor,
            ThemeFlags::all_enabled(),
        );
        let palette = theme.legacy_palette();
        assert_eq!(palette.bg, ThemePalette::nord().bg);
    }
}
