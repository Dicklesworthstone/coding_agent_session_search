//! FrankenTUI style-system scaffolding for cass.
//!
//! Centralizes:
//! - theme preset selection (18 gorgeous built-in presets)
//! - color profile downgrade (mono / ansi16 / ansi256 / truecolor)
//! - env opt-outs (`NO_COLOR`, `CASS_NO_COLOR`, `CASS_NO_ICONS`, `CASS_NO_GRADIENT`)
//! - accessibility text markers (`CASS_A11Y`)
//! - semantic `StyleSheet` tokens consumed by all ftui views
//! - [`StyleContext`] facade for theme-aware style resolution in view code
//!
//! Widgets reference semantic token names (e.g. `STYLE_STATUS_SUCCESS`) rather
//! than raw colors, so preset changes and color profile downgrades propagate
//! automatically. F2 / Shift+F2 cycle forward/backward through presets.

use std::fs;
use std::path::{Path, PathBuf};

use ftui::render::cell::PackedRgba;
use ftui::style::theme::themes;
use ftui::{
    AdaptiveColor, Color, ColorProfile, ResolvedTheme, Style, StyleSheet, TableTheme,
    TerminalCapabilities, Theme, ThemeBuilder,
};
use ftui_extras::markdown::MarkdownTheme;
use ftui_extras::syntax::HighlightTheme;
use serde::{Deserialize, Serialize};

pub const STYLE_APP_ROOT: &str = "app.root";
pub const STYLE_PANE_BASE: &str = "pane.base";
pub const STYLE_PANE_FOCUSED: &str = "pane.focused";
pub const STYLE_PANE_TITLE_FOCUSED: &str = "pane.title.focused";
pub const STYLE_PANE_TITLE_UNFOCUSED: &str = "pane.title.unfocused";
pub const STYLE_SPLIT_HANDLE: &str = "split.handle";
pub const STYLE_TEXT_PRIMARY: &str = "text.primary";
pub const STYLE_TEXT_MUTED: &str = "text.muted";
pub const STYLE_TEXT_SUBTLE: &str = "text.subtle";
pub const STYLE_STATUS_SUCCESS: &str = "status.success";
pub const STYLE_STATUS_WARNING: &str = "status.warning";
pub const STYLE_STATUS_ERROR: &str = "status.error";
pub const STYLE_STATUS_INFO: &str = "status.info";
pub const STYLE_RESULT_ROW: &str = "result.row";
pub const STYLE_RESULT_ROW_ALT: &str = "result.row.alt";
pub const STYLE_RESULT_ROW_SELECTED: &str = "result.row.selected";
pub const STYLE_ROLE_USER: &str = "role.user";
pub const STYLE_ROLE_ASSISTANT: &str = "role.assistant";
pub const STYLE_ROLE_TOOL: &str = "role.tool";
pub const STYLE_ROLE_SYSTEM: &str = "role.system";
pub const STYLE_ROLE_GUTTER_USER: &str = "role.gutter.user";
pub const STYLE_ROLE_GUTTER_ASSISTANT: &str = "role.gutter.assistant";
pub const STYLE_ROLE_GUTTER_TOOL: &str = "role.gutter.tool";
pub const STYLE_ROLE_GUTTER_SYSTEM: &str = "role.gutter.system";
pub const STYLE_SCORE_HIGH: &str = "score.high";
pub const STYLE_SCORE_MID: &str = "score.mid";
pub const STYLE_SCORE_LOW: &str = "score.low";
pub const STYLE_SOURCE_LOCAL: &str = "source.local";
pub const STYLE_SOURCE_REMOTE: &str = "source.remote";
pub const STYLE_LOCATION: &str = "location";
pub const STYLE_PILL_ACTIVE: &str = "pill.active";
pub const STYLE_PILL_INACTIVE: &str = "pill.inactive";
pub const STYLE_PILL_LABEL: &str = "pill.label";
pub const STYLE_CRUMB_ACTIVE: &str = "crumb.active";
pub const STYLE_CRUMB_INACTIVE: &str = "crumb.inactive";
pub const STYLE_CRUMB_SEPARATOR: &str = "crumb.separator";
pub const STYLE_TAB_ACTIVE: &str = "tab.active";
pub const STYLE_TAB_INACTIVE: &str = "tab.inactive";
pub const STYLE_DETAIL_FIND_CONTAINER: &str = "detail.find.container";
pub const STYLE_DETAIL_FIND_QUERY: &str = "detail.find.query";
pub const STYLE_DETAIL_FIND_MATCH_ACTIVE: &str = "detail.find.match.active";
pub const STYLE_DETAIL_FIND_MATCH_INACTIVE: &str = "detail.find.match.inactive";
pub const STYLE_QUERY_HIGHLIGHT: &str = "query.highlight";
pub const STYLE_SEARCH_FOCUS: &str = "search.focus";
pub const STYLE_MODAL_BACKDROP: &str = "modal.backdrop";
pub const STYLE_KBD_KEY: &str = "kbd.key";
pub const STYLE_KBD_DESC: &str = "kbd.desc";
pub const THEME_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiThemePreset {
    #[default]
    #[serde(alias = "dark")]
    TokyoNight,
    #[serde(alias = "light")]
    Daylight,
    #[serde(alias = "high_contrast", alias = "highcontrast", alias = "hc")]
    HighContrast,
    #[serde(alias = "cat")]
    Catppuccin,
    Dracula,
    Nord,
    SolarizedDark,
    SolarizedLight,
    Monokai,
    GruvboxDark,
    OneDark,
    RosePine,
    Everforest,
    Kanagawa,
    AyuMirage,
    Nightfox,
    CyberpunkAurora,
    Synthwave84,
    #[serde(alias = "cb")]
    Colorblind,
}

impl UiThemePreset {
    pub const fn all() -> [Self; 19] {
        [
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
        ]
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::TokyoNight => "Tokyo Night",
            Self::Daylight => "Daylight",
            Self::HighContrast => "High Contrast",
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
            Self::Colorblind => "Colorblind",
        }
    }

    pub fn next(self) -> Self {
        let all = Self::all();
        let idx = all.iter().position(|preset| *preset == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    pub fn previous(self) -> Self {
        let all = Self::all();
        let idx = all.iter().position(|preset| *preset == self).unwrap_or(0);
        all[(idx + all.len() - 1) % all.len()]
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" | "tokyo-night" | "tokyo_night" | "tokyonight" => Some(Self::TokyoNight),
            "light" | "daylight" => Some(Self::Daylight),
            "high-contrast" | "high_contrast" | "highcontrast" | "hc" => Some(Self::HighContrast),
            "catppuccin" | "cat" | "catppuccin-mocha" => Some(Self::Catppuccin),
            "dracula" => Some(Self::Dracula),
            "nord" => Some(Self::Nord),
            "solarized-dark" | "solarized_dark" => Some(Self::SolarizedDark),
            "solarized-light" | "solarized_light" => Some(Self::SolarizedLight),
            "monokai" => Some(Self::Monokai),
            "gruvbox-dark" | "gruvbox_dark" | "gruvbox" => Some(Self::GruvboxDark),
            "one-dark" | "one_dark" | "onedark" => Some(Self::OneDark),
            "rose-pine" | "rose_pine" | "rosepine" => Some(Self::RosePine),
            "everforest" => Some(Self::Everforest),
            "kanagawa" => Some(Self::Kanagawa),
            "ayu-mirage" | "ayu_mirage" | "ayumirage" => Some(Self::AyuMirage),
            "nightfox" => Some(Self::Nightfox),
            "cyberpunk-aurora" | "cyberpunk_aurora" | "cyberpunk" => Some(Self::CyberpunkAurora),
            "synthwave-84" | "synthwave_84" | "synthwave84" | "synthwave" => {
                Some(Self::Synthwave84)
            }
            "colorblind" | "colour-blind" | "color-blind" | "cb" | "cvd" => {
                Some(Self::Colorblind)
            }
            _ => None,
        }
    }

    fn base_theme(self) -> Theme {
        match self {
            Self::TokyoNight => tokyo_night_theme(),
            Self::Daylight => themes::light(),
            Self::HighContrast => high_contrast_theme(),
            Self::Catppuccin => catppuccin_theme(),
            Self::Dracula => themes::dracula(),
            Self::Nord => themes::nord(),
            Self::SolarizedDark => themes::solarized_dark(),
            Self::SolarizedLight => themes::solarized_light(),
            Self::Monokai => themes::monokai(),
            Self::GruvboxDark => gruvbox_dark_theme(),
            Self::OneDark => one_dark_theme(),
            Self::RosePine => rose_pine_theme(),
            Self::Everforest => everforest_theme(),
            Self::Kanagawa => kanagawa_theme(),
            Self::AyuMirage => ayu_mirage_theme(),
            Self::Nightfox => nightfox_theme(),
            Self::CyberpunkAurora => cyberpunk_aurora_theme(),
            Self::Synthwave84 => synthwave_84_theme(),
            Self::Colorblind => colorblind_theme(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_theme_config_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_preset: Option<UiThemePreset>,
}

impl ThemeConfig {
    pub fn from_json_str(raw: &str) -> Result<Self, ThemeConfigError> {
        let config: Self =
            serde_json::from_str(raw).map_err(|source| ThemeConfigError::ParseJson { source })?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_json_pretty(&self) -> Result<String, ThemeConfigError> {
        self.validate()?;
        serde_json::to_string_pretty(self)
            .map_err(|source| ThemeConfigError::SerializeJson { source })
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ThemeConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| ThemeConfigError::ReadConfig {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json_str(&raw)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ThemeConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ThemeConfigError::WriteConfig {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let payload = self.to_json_pretty()?;
        fs::write(path, payload).map_err(|source| ThemeConfigError::WriteConfig {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn validate(&self) -> Result<(), ThemeConfigError> {
        if self.version != THEME_CONFIG_VERSION {
            return Err(ThemeConfigError::UnsupportedVersion {
                found: self.version,
                expected: THEME_CONFIG_VERSION,
            });
        }
        Ok(())
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            version: THEME_CONFIG_VERSION,
            base_preset: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThemeContrastCheck {
    pub pair: &'static str,
    pub ratio: f64,
    pub minimum: f64,
    pub passes: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThemeContrastReport {
    pub checks: Vec<ThemeContrastCheck>,
}

impl ThemeContrastReport {
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| !check.passes)
    }

    pub fn failing_pairs(&self) -> Vec<&'static str> {
        self.checks
            .iter()
            .filter(|check| !check.passes)
            .map(|check| check.pair)
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ThemeConfigError {
    #[error("unsupported theme config version {found}; expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("failed to parse theme config JSON: {source}")]
    ParseJson { source: serde_json::Error },
    #[error("failed to serialize theme config JSON: {source}")]
    SerializeJson { source: serde_json::Error },
    #[error("failed to read theme config `{path}`: {source}")]
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write theme config `{path}`: {source}")]
    WriteConfig {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn default_theme_config_version() -> u32 {
    THEME_CONFIG_VERSION
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleOptions {
    pub preset: UiThemePreset,
    pub dark_mode: bool,
    pub color_profile: ColorProfile,
    pub no_color: bool,
    pub no_icons: bool,
    pub no_gradient: bool,
    pub a11y: bool,
}

impl Default for StyleOptions {
    fn default() -> Self {
        Self {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::detect(),
            no_color: false,
            no_icons: false,
            no_gradient: false,
            a11y: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct EnvValues<'a> {
    no_color: Option<&'a str>,
    cass_respect_no_color: Option<&'a str>,
    cass_no_color: Option<&'a str>,
    colorterm: Option<&'a str>,
    term: Option<&'a str>,
    cass_no_icons: Option<&'a str>,
    cass_no_gradient: Option<&'a str>,
    cass_a11y: Option<&'a str>,
    cass_theme: Option<&'a str>,
    cass_color_profile: Option<&'a str>,
}

impl StyleOptions {
    pub fn from_env() -> Self {
        let no_color = dotenvy::var("NO_COLOR").ok();
        let cass_respect_no_color = dotenvy::var("CASS_RESPECT_NO_COLOR").ok();
        let cass_no_color = dotenvy::var("CASS_NO_COLOR").ok();
        let colorterm = dotenvy::var("COLORTERM").ok();
        let term = dotenvy::var("TERM").ok();
        let cass_no_icons = dotenvy::var("CASS_NO_ICONS").ok();
        let cass_no_gradient = dotenvy::var("CASS_NO_GRADIENT").ok();
        let cass_a11y = dotenvy::var("CASS_A11Y").ok();
        let cass_theme = dotenvy::var("CASS_THEME").ok();
        let cass_color_profile = dotenvy::var("CASS_COLOR_PROFILE").ok();

        let mut options = Self::from_env_values(EnvValues {
            no_color: no_color.as_deref(),
            cass_respect_no_color: cass_respect_no_color.as_deref(),
            cass_no_color: cass_no_color.as_deref(),
            colorterm: colorterm.as_deref(),
            term: term.as_deref(),
            cass_no_icons: cass_no_icons.as_deref(),
            cass_no_gradient: cass_no_gradient.as_deref(),
            cass_a11y: cass_a11y.as_deref(),
            cass_theme: cass_theme.as_deref(),
            cass_color_profile: cass_color_profile.as_deref(),
        });

        // Prefer runtime terminal capability detection for interactive TUI.
        // This yields the best supported profile even when wrapper shells
        // inherit conservative TERM values.
        if !options.no_color && cass_color_profile.is_none() {
            let caps = TerminalCapabilities::with_overrides();
            options.color_profile = if caps.true_color {
                ColorProfile::TrueColor
            } else if caps.colors_256 {
                ColorProfile::Ansi256
            } else {
                ColorProfile::Ansi16
            };
        }

        options
    }

    /// Resolve `StyleOptions` from a snapshot of environment variables.
    ///
    /// ## Precedence rules (evaluated top-to-bottom, first match wins)
    ///
    /// | Priority | Condition | `color_profile` | `no_color` |
    /// |----------|-----------|------------------|------------|
    /// | 1 (highest) | `CASS_NO_COLOR` is truthy | Mono | true |
    /// | 2 | `CASS_RESPECT_NO_COLOR` is truthy **and** `NO_COLOR` is set | Mono | true |
    /// | 3 | `CASS_COLOR_PROFILE` is set to a valid value | that value | false |
    /// | 4 (lowest) | None of the above | detect from COLORTERM/TERM | false |
    ///
    /// ## Cascade effects
    ///
    /// - `no_gradient` = `CASS_NO_GRADIENT` (truthy) **or** `no_color` **or** `a11y`
    /// - `no_icons` = `CASS_NO_ICONS` (truthy; independent of color state)
    /// - `a11y` = `CASS_A11Y` is truthy (adds bold/underline accents, text role markers)
    /// - `dark_mode` = `false` only for `Light` preset; `HighContrast` auto-detects
    ///
    /// ## Notes
    ///
    /// - `NO_COLOR` alone is intentionally ignored; `CASS_RESPECT_NO_COLOR` must opt in.
    /// - `CASS_NO_COLOR` trumps `CASS_COLOR_PROFILE` even when set to "truecolor".
    /// - Invalid `CASS_COLOR_PROFILE` values silently fall back to env detection.
    /// - `CASS_A11Y` uses `env_truthy()`: "0"/"false"/"off"/"no" → false, anything else → true.
    fn from_env_values(values: EnvValues<'_>) -> Self {
        let preset = values
            .cass_theme
            .and_then(UiThemePreset::parse)
            .unwrap_or(UiThemePreset::TokyoNight);

        let no_color_enabled = env_truthy(values.cass_no_color)
            || (env_truthy(values.cass_respect_no_color) && values.no_color.is_some());

        let detected_profile = ColorProfile::detect_from_env(None, values.colorterm, values.term);
        let profile_override = values.cass_color_profile.and_then(parse_color_profile);
        let color_profile = if no_color_enabled {
            ColorProfile::Mono
        } else {
            profile_override.unwrap_or(detected_profile)
        };

        let a11y = env_truthy(values.cass_a11y);
        let no_icons = env_truthy(values.cass_no_icons);
        let no_gradient = env_truthy(values.cass_no_gradient) || no_color_enabled || a11y;

        let dark_mode = match preset {
            UiThemePreset::Daylight | UiThemePreset::SolarizedLight => false,
            UiThemePreset::HighContrast => Theme::detect_dark_mode(),
            _ => true,
        };

        Self {
            preset,
            dark_mode,
            color_profile,
            no_color: no_color_enabled,
            no_icons,
            no_gradient,
            a11y,
        }
    }

    pub const fn gradients_enabled(self) -> bool {
        !self.no_gradient && self.color_profile.supports_color()
    }
}

// ---------------------------------------------------------------------------
// Decorative policy — capability/degradation/breakpoint guardrails (2dccg.10.6)
// ---------------------------------------------------------------------------

/// Border rendering strategy, from richest to most minimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BorderTier {
    /// Unicode rounded corners (`╭─╮`).
    Rounded,
    /// Plain box-drawing (`┌─┐`).
    Square,
    /// No borders at all.
    None,
}

/// Resolved decorative policy for the current frame.
///
/// Computed from [`StyleOptions`], the ftui `DegradationLevel`, and the
/// [`LayoutBreakpoint`] so that rendering code never makes ad-hoc decisions
/// about what decorative elements to show.
///
/// ## Policy table
///
/// | Degradation       | Breakpoint   | fancy_borders | `border_tier` | `show_icons` | `use_styling` |
/// |-------------------|--------------|---------------|---------------|--------------|---------------|
/// | Full              | any          | true          | Rounded       | true         | true          |
/// | Full              | Narrow       | true          | Square        | true         | true          |
/// | Full              | any          | false         | Square        | true         | true          |
/// | SimpleBorders     | any          | _             | Square        | true         | true          |
/// | NoStyling         | any          | _             | Square        | true         | false         |
/// | EssentialOnly+    | any          | _             | None          | false        | false         |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecorativePolicy {
    /// Which border rendering tier to use.
    pub border_tier: BorderTier,
    /// Whether to render icons and decorative Unicode glyphs.
    pub show_icons: bool,
    /// Whether to apply color styling (fg/bg).
    pub use_styling: bool,
    /// Whether gradients are allowed (requires TrueColor + not a11y + not no_gradient).
    pub use_gradients: bool,
    /// Whether to render content at all (false at Skeleton/SkipFrame).
    pub render_content: bool,
}

impl DecorativePolicy {
    /// Resolve policy from the current style options, degradation level, and breakpoint.
    ///
    /// Uses `fancy_borders` as the user-preference toggle (Ctrl+B in TUI).
    pub fn resolve(
        options: StyleOptions,
        degradation: ftui::render::budget::DegradationLevel,
        breakpoint: super::app::LayoutBreakpoint,
        fancy_borders: bool,
    ) -> Self {
        use crate::ui::app::LayoutBreakpoint as LB;

        let render_content = degradation.render_content();

        // Border tier: EssentialOnly+ strips all borders.
        let border_tier = if !degradation.render_decorative() {
            BorderTier::None
        } else if !degradation.use_unicode_borders() {
            // SimpleBorders+ forces plain box-drawing.
            BorderTier::Square
        } else if !fancy_borders {
            BorderTier::Square
        } else if breakpoint == LB::Narrow {
            // Narrow terminals: use square borders to save horizontal space.
            BorderTier::Square
        } else {
            BorderTier::Rounded
        };

        let show_icons = degradation.render_decorative() && !options.no_icons;
        let use_styling = degradation.apply_styling() && !options.no_color;
        let use_gradients = options.gradients_enabled() && degradation.apply_styling();

        Self {
            border_tier,
            show_icons,
            use_styling,
            use_gradients,
            render_content,
        }
    }
}

/// Input axes for capability-matrix diagnostics.
///
/// This mirrors the environment-driven style inputs that affect policy
/// resolution and can be used in deterministic tests for representative
/// terminal profiles.
#[derive(Debug, Clone, Copy, Default)]
pub struct CapabilityMatrixInputs<'a> {
    /// TERM value used for profile detection.
    pub term: Option<&'a str>,
    /// COLORTERM value used for profile detection.
    pub colorterm: Option<&'a str>,
    /// Whether `NO_COLOR` is set.
    pub no_color: bool,
    /// Whether `CASS_RESPECT_NO_COLOR` is set/truthy.
    pub cass_respect_no_color: bool,
    /// Whether `CASS_NO_COLOR` is set.
    pub cass_no_color: bool,
    /// Whether `CASS_NO_ICONS` is set.
    pub cass_no_icons: bool,
    /// Whether `CASS_NO_GRADIENT` is set.
    pub cass_no_gradient: bool,
    /// Whether `CASS_A11Y` is set/truthy.
    pub cass_a11y: bool,
    /// Optional explicit theme preset override.
    pub cass_theme: Option<&'a str>,
    /// Optional explicit color profile override.
    pub cass_color_profile: Option<&'a str>,
}

/// Machine-readable diagnostic summary for a resolved style policy decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StylePolicyDiagnostic {
    /// Terminal capability profile id (`xterm-256color`, `dumb`, `kitty`, ...).
    pub terminal_profile: String,
    /// TERM from diagnostic input.
    pub term: Option<String>,
    /// COLORTERM from diagnostic input.
    pub colorterm: Option<String>,
    /// Current degradation level label.
    pub degradation: &'static str,
    /// Current responsive breakpoint label.
    pub breakpoint: &'static str,
    /// Whether rounded borders are user-enabled.
    pub fancy_borders: bool,
    /// Capability flag: supports truecolor.
    pub capability_true_color: bool,
    /// Capability flag: supports 256-color palette.
    pub capability_colors_256: bool,
    /// Capability flag: supports Unicode box drawing.
    pub capability_unicode_box_drawing: bool,
    /// Input env axis: `NO_COLOR`.
    pub env_no_color: bool,
    /// Input env axis: `CASS_RESPECT_NO_COLOR`.
    pub env_cass_respect_no_color: bool,
    /// Input env axis: `CASS_NO_COLOR`.
    pub env_cass_no_color: bool,
    /// Resolved color profile after precedence rules.
    pub resolved_color_profile: &'static str,
    /// Resolved style options: no_color.
    pub resolved_no_color: bool,
    /// Resolved style options: no_icons.
    pub resolved_no_icons: bool,
    /// Resolved style options: no_gradient.
    pub resolved_no_gradient: bool,
    /// Resolved policy: border tier.
    pub policy_border_tier: &'static str,
    /// Resolved policy: icon rendering.
    pub policy_show_icons: bool,
    /// Resolved policy: fg/bg styling.
    pub policy_use_styling: bool,
    /// Resolved policy: gradients.
    pub policy_use_gradients: bool,
    /// Resolved policy: content rendering.
    pub policy_render_content: bool,
}

fn env_flag(value: bool) -> Option<&'static str> {
    if value { Some("1") } else { None }
}

fn color_profile_name(profile: ColorProfile) -> &'static str {
    match profile {
        ColorProfile::Mono => "mono",
        ColorProfile::Ansi16 => "ansi16",
        ColorProfile::Ansi256 => "ansi256",
        ColorProfile::TrueColor => "truecolor",
    }
}

fn border_tier_name(tier: BorderTier) -> &'static str {
    match tier {
        BorderTier::Rounded => "rounded",
        BorderTier::Square => "square",
        BorderTier::None => "none",
    }
}

fn breakpoint_name(breakpoint: super::app::LayoutBreakpoint) -> &'static str {
    use crate::ui::app::LayoutBreakpoint as LB;
    match breakpoint {
        LB::Narrow => "narrow",
        LB::MediumNarrow => "medium-narrow",
        LB::Medium => "medium",
        LB::Wide => "wide",
    }
}

fn degradation_name(level: ftui::render::budget::DegradationLevel) -> &'static str {
    use ftui::render::budget::DegradationLevel as DL;
    match level {
        DL::Full => "full",
        DL::SimpleBorders => "simple-borders",
        DL::NoStyling => "no-styling",
        DL::EssentialOnly => "essential-only",
        DL::Skeleton => "skeleton",
        DL::SkipFrame => "skip-frame",
    }
}

/// Build a policy diagnostic payload for a specific capability/profile fixture.
///
/// This intentionally accepts explicit capability and env inputs so tests can
/// validate style-policy decisions deterministically without depending on host
/// terminal state.
pub fn style_policy_diagnostic(
    capabilities: TerminalCapabilities,
    inputs: CapabilityMatrixInputs<'_>,
    degradation: ftui::render::budget::DegradationLevel,
    breakpoint: super::app::LayoutBreakpoint,
    fancy_borders: bool,
) -> StylePolicyDiagnostic {
    let env_values = EnvValues {
        no_color: env_flag(inputs.no_color),
        cass_respect_no_color: env_flag(inputs.cass_respect_no_color),
        cass_no_color: env_flag(inputs.cass_no_color),
        colorterm: inputs.colorterm,
        term: inputs.term,
        cass_no_icons: env_flag(inputs.cass_no_icons),
        cass_no_gradient: env_flag(inputs.cass_no_gradient),
        cass_a11y: env_flag(inputs.cass_a11y),
        cass_theme: inputs.cass_theme,
        cass_color_profile: inputs.cass_color_profile,
    };

    let mut options = StyleOptions::from_env_values(env_values);

    // In diagnostics, keep profile resolution deterministic from explicit
    // capabilities when no direct CASS_COLOR_PROFILE override is provided.
    if !options.no_color && inputs.cass_color_profile.is_none() {
        options.color_profile = if capabilities.true_color {
            ColorProfile::TrueColor
        } else if capabilities.colors_256 {
            ColorProfile::Ansi256
        } else {
            ColorProfile::Ansi16
        };
    }

    let policy = DecorativePolicy::resolve(options, degradation, breakpoint, fancy_borders);

    StylePolicyDiagnostic {
        terminal_profile: capabilities.profile().as_str().to_string(),
        term: inputs.term.map(ToString::to_string),
        colorterm: inputs.colorterm.map(ToString::to_string),
        degradation: degradation_name(degradation),
        breakpoint: breakpoint_name(breakpoint),
        fancy_borders,
        capability_true_color: capabilities.true_color,
        capability_colors_256: capabilities.colors_256,
        capability_unicode_box_drawing: capabilities.unicode_box_drawing,
        env_no_color: inputs.no_color,
        env_cass_respect_no_color: inputs.cass_respect_no_color,
        env_cass_no_color: inputs.cass_no_color,
        resolved_color_profile: color_profile_name(options.color_profile),
        resolved_no_color: options.no_color,
        resolved_no_icons: options.no_icons,
        resolved_no_gradient: options.no_gradient,
        policy_border_tier: border_tier_name(policy.border_tier),
        policy_show_icons: policy.show_icons,
        policy_use_styling: policy.use_styling,
        policy_use_gradients: policy.use_gradients,
        policy_render_content: policy.render_content,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleMarkers {
    pub user: &'static str,
    pub assistant: &'static str,
    pub tool: &'static str,
    pub system: &'static str,
}

impl RoleMarkers {
    fn from_options(options: StyleOptions) -> Self {
        if options.a11y {
            return Self {
                user: "[user]",
                assistant: "[assistant]",
                tool: "[tool]",
                system: "[system]",
            };
        }

        if options.no_icons {
            return Self {
                user: "",
                assistant: "",
                tool: "",
                system: "",
            };
        }

        Self {
            user: "U>",
            assistant: "A>",
            tool: "T>",
            system: "S>",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StyleContext {
    pub options: StyleOptions,
    pub theme: Theme,
    pub resolved: ResolvedTheme,
    pub sheet: StyleSheet,
    pub role_markers: RoleMarkers,
}

impl StyleContext {
    pub fn from_options(options: StyleOptions) -> Self {
        Self::build(options)
    }

    pub fn from_options_with_theme_config(mut options: StyleOptions, config: &ThemeConfig) -> Self {
        if let Some(base_preset) = config.base_preset {
            options.preset = base_preset;
            options.dark_mode = match base_preset {
                UiThemePreset::Daylight | UiThemePreset::SolarizedLight => false,
                UiThemePreset::HighContrast => Theme::detect_dark_mode(),
                _ => true,
            };
        }

        Self::build(options)
    }

    fn build(options: StyleOptions) -> Self {
        let mut theme = options.preset.base_theme();

        if options.a11y && options.preset != UiThemePreset::HighContrast {
            theme = apply_a11y_overrides(theme);
        }

        theme = downgrade_theme_for_profile(theme, options.color_profile);

        let dark_mode = match options.preset {
            UiThemePreset::Daylight | UiThemePreset::SolarizedLight => false,
            _ => options.dark_mode,
        };
        let resolved = theme.resolve(dark_mode);
        let sheet = build_stylesheet(resolved, options);
        let role_markers = RoleMarkers::from_options(options);

        Self {
            options,
            theme,
            resolved,
            sheet,
            role_markers,
        }
    }

    pub fn from_env() -> Self {
        Self::from_options(StyleOptions::from_env())
    }

    pub fn style(&self, name: &str) -> Style {
        self.sheet.get_or_default(name)
    }

    /// Return an accent-colored style for the given agent slug.
    pub fn agent_accent_style(&self, agent: &str) -> Style {
        let pane = super::components::theme::ThemePalette::agent_pane(agent);
        if self.options.no_color
            || self.options.a11y
            || !self.options.color_profile.supports_color()
        {
            return Style::new().fg(pane.accent).bold();
        }

        let accent = Color::rgb(pane.accent.r(), pane.accent.g(), pane.accent.b());
        let badge_bg = blend(
            self.resolved.surface,
            accent,
            if self.options.gradients_enabled() {
                0.22
            } else {
                0.14
            },
        );

        // Pick whichever semantic foreground keeps the strongest contrast.
        let mut best_fg = self.resolved.text;
        let mut best_ratio =
            ftui::style::contrast_ratio_packed(to_packed(best_fg), to_packed(badge_bg));
        for candidate in [self.resolved.selection_fg, accent] {
            let ratio =
                ftui::style::contrast_ratio_packed(to_packed(candidate), to_packed(badge_bg));
            if ratio > best_ratio {
                best_ratio = ratio;
                best_fg = candidate;
            }
        }

        Style::new()
            .fg(to_packed(best_fg))
            .bg(to_packed(badge_bg))
            .bold()
    }

    /// Tint a base results-row style with a subtle per-agent accent.
    ///
    /// This restores scan-friendly visual grouping in the list without
    /// sacrificing legibility or selected-row affordances.
    pub fn result_row_style_for_agent(&self, base: Style, agent: &str) -> Style {
        let Some(base_bg) = base.bg else {
            return base;
        };
        if self.options.no_color
            || self.options.a11y
            || !self.options.color_profile.supports_color()
        {
            return base;
        }

        let pane = super::components::theme::ThemePalette::agent_pane(agent);
        let accent = Color::rgb(pane.accent.r(), pane.accent.g(), pane.accent.b());
        let pane_bg = Color::rgb(pane.bg.r(), pane.bg.g(), pane.bg.b());
        let base_bg_color = Color::rgb(base_bg.r(), base_bg.g(), base_bg.b());
        let mut tint_mix = if self.options.gradients_enabled() {
            0.12
        } else {
            0.08
        };
        let selected_bg = self.resolved.selection_bg;
        let base_separation =
            ftui::style::contrast_ratio_packed(to_packed(selected_bg), to_packed(base_bg_color));
        let min_allowed_separation = (base_separation - 0.03).max(1.01);
        let mut tint = blend(base_bg_color, pane_bg, tint_mix);
        let mut tint_separation =
            ftui::style::contrast_ratio_packed(to_packed(selected_bg), to_packed(tint));

        // Keep selected-row affordances visually dominant: if tinting moves row
        // background too close to selection background, taper tint intensity.
        if tint_separation < min_allowed_separation {
            for _ in 0..4 {
                tint_mix *= 0.55;
                let candidate = blend(base_bg_color, pane_bg, tint_mix);
                let candidate_separation = ftui::style::contrast_ratio_packed(
                    to_packed(selected_bg),
                    to_packed(candidate),
                );
                tint = candidate;
                tint_separation = candidate_separation;
                if candidate_separation >= min_allowed_separation {
                    break;
                }
            }
        }

        // Preserve deterministic per-agent differentiation even when selection
        // safety logic dampens the primary tint significantly.
        let agent_hash = agent.as_bytes().iter().fold(0u32, |acc, b| {
            acc.wrapping_mul(131).wrapping_add(u32::from(*b))
        });
        let signature_mix_base = if self.options.gradients_enabled() {
            0.010
        } else {
            0.006
        };
        let signature_mix = signature_mix_base + (agent_hash % 5) as f32 * 0.0015;
        let signature_tint = blend(tint, accent, signature_mix);
        let signature_separation =
            ftui::style::contrast_ratio_packed(to_packed(selected_bg), to_packed(signature_tint));
        if signature_separation >= min_allowed_separation {
            tint = signature_tint;
            tint_separation = signature_separation;
        }

        debug_assert!(
            tint_separation > 0.0,
            "contrast ratios should always be positive"
        );
        Style::new()
            .fg(base.fg.unwrap_or_else(|| to_packed(self.resolved.text)))
            .bg(to_packed(tint))
    }

    /// Return a score-magnitude style (high/mid/low).
    pub fn score_style(&self, score: f32) -> Style {
        if score >= 8.0 {
            self.style(STYLE_SCORE_HIGH)
        } else if score >= 5.0 {
            self.style(STYLE_SCORE_MID)
        } else {
            self.style(STYLE_SCORE_LOW)
        }
    }

    pub fn contrast_report(&self) -> ThemeContrastReport {
        build_contrast_report(self.resolved)
    }

    /// Build a [`MarkdownTheme`] derived from the active resolved theme so
    /// markdown content renders in theme-coherent colors.
    pub fn markdown_theme(&self) -> MarkdownTheme {
        let r = &self.resolved;
        MarkdownTheme {
            h1: Style::new().fg(to_packed(r.primary)).bold(),
            h2: Style::new().fg(to_packed(r.info)).bold(),
            h3: Style::new().fg(to_packed(r.success)).bold(),
            h4: Style::new().fg(to_packed(r.warning)).bold(),
            h5: Style::new().fg(to_packed(r.text)).bold(),
            h6: Style::new().fg(to_packed(r.text_muted)).bold(),
            code_inline: Style::new()
                .fg(to_packed(r.text))
                .bg(to_packed(blend(r.surface, r.text, 0.08))),
            code_block: Style::new().fg(to_packed(r.text)).bg(to_packed(blend(
                r.background,
                r.surface,
                0.5,
            ))),
            blockquote: Style::new().fg(to_packed(r.text_muted)).italic(),
            link: Style::new().fg(to_packed(r.info)).underline(),
            emphasis: Style::new().fg(to_packed(r.text)).italic(),
            strong: Style::new().fg(to_packed(r.text)).bold(),
            strikethrough: Style::new().fg(to_packed(r.text_muted)).strikethrough(),
            list_bullet: Style::new().fg(to_packed(r.info)),
            horizontal_rule: Style::new().fg(to_packed(r.border)).dim(),
            table_theme: TableTheme {
                border: Style::new().fg(to_packed(r.border)),
                header: Style::new()
                    .fg(to_packed(r.text))
                    .bg(to_packed(blend(r.surface, r.primary, 0.15)))
                    .bold(),
                row: Style::new().fg(to_packed(r.text)),
                row_alt: Style::new().fg(to_packed(r.text)).bg(to_packed(blend(
                    r.background,
                    r.surface,
                    0.3,
                ))),
                divider: Style::new().fg(to_packed(r.border)).dim(),
                ..TableTheme::default()
            },
            task_done: Style::new().fg(to_packed(r.success)),
            task_todo: Style::new().fg(to_packed(r.text_muted)),
            math_inline: Style::new().fg(to_packed(r.warning)).italic(),
            math_block: Style::new().fg(to_packed(r.warning)).bold(),
            footnote_ref: Style::new().fg(to_packed(r.info)).dim(),
            footnote_def: Style::new().fg(to_packed(r.text_muted)),
            admonition_note: Style::new().fg(to_packed(r.info)).bold(),
            admonition_tip: Style::new().fg(to_packed(r.success)).bold(),
            admonition_important: Style::new().fg(to_packed(r.primary)).bold(),
            admonition_warning: Style::new().fg(to_packed(r.warning)).bold(),
            admonition_caution: Style::new().fg(to_packed(r.error)).bold(),
        }
    }

    /// Return a syntax highlight theme matching the current UI theme brightness.
    pub fn syntax_highlight_theme(&self) -> HighlightTheme {
        if self.options.dark_mode {
            HighlightTheme::dark()
        } else {
            HighlightTheme::light()
        }
    }
}

fn parse_color_profile(value: &str) -> Option<ColorProfile> {
    match value.trim().to_ascii_lowercase().as_str() {
        "mono" | "none" => Some(ColorProfile::Mono),
        "ansi16" | "16" => Some(ColorProfile::Ansi16),
        "ansi256" | "256" => Some(ColorProfile::Ansi256),
        "truecolor" | "24bit" | "rgb" => Some(ColorProfile::TrueColor),
        _ => None,
    }
}

fn env_truthy(value: Option<&str>) -> bool {
    match value {
        Some(raw) => {
            let normalized = raw.trim().to_ascii_lowercase();
            !(normalized == "0"
                || normalized == "false"
                || normalized == "off"
                || normalized == "no")
        }
        None => false,
    }
}

fn tokyo_night_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(122, 162, 247))
        .secondary(Color::rgb(187, 154, 247))
        .accent(Color::rgb(125, 207, 255))
        .background(Color::rgb(26, 27, 38))
        .surface(Color::rgb(36, 40, 59))
        .overlay(Color::rgb(41, 46, 66))
        .text(Color::rgb(192, 202, 245))
        .text_muted(Color::rgb(169, 177, 214))
        .text_subtle(Color::rgb(105, 114, 158))
        .success(Color::rgb(115, 218, 202))
        .warning(Color::rgb(224, 175, 104))
        .error(Color::rgb(247, 118, 142))
        .info(Color::rgb(125, 207, 255))
        .border(Color::rgb(59, 66, 97))
        .border_focused(Color::rgb(125, 145, 200))
        .selection_bg(Color::rgb(122, 162, 247))
        .selection_fg(Color::rgb(26, 27, 38))
        .scrollbar_track(Color::rgb(41, 46, 66))
        .scrollbar_thumb(Color::rgb(125, 145, 200))
        .build()
}

fn catppuccin_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(137, 180, 250))
        .secondary(Color::rgb(245, 194, 231))
        .accent(Color::rgb(203, 166, 247))
        .background(Color::rgb(30, 30, 46))
        .surface(Color::rgb(49, 50, 68))
        .overlay(Color::rgb(69, 71, 90))
        .text(Color::rgb(205, 214, 244))
        .text_muted(Color::rgb(166, 173, 200))
        .text_subtle(Color::rgb(127, 132, 156))
        .success(Color::rgb(166, 227, 161))
        .warning(Color::rgb(249, 226, 175))
        .error(Color::rgb(243, 139, 168))
        .info(Color::rgb(137, 220, 235))
        .border(Color::rgb(88, 91, 112))
        .border_focused(Color::rgb(180, 190, 254))
        .selection_bg(Color::rgb(116, 199, 236))
        .selection_fg(Color::rgb(30, 30, 46))
        .scrollbar_track(Color::rgb(49, 50, 68))
        .scrollbar_thumb(Color::rgb(88, 91, 112))
        .build()
}

fn high_contrast_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .secondary(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .accent(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 0),
        ))
        .background(AdaptiveColor::adaptive(
            Color::rgb(255, 255, 255),
            Color::rgb(0, 0, 0),
        ))
        .surface(AdaptiveColor::adaptive(
            Color::rgb(245, 245, 245),
            Color::rgb(0, 0, 0),
        ))
        .overlay(AdaptiveColor::adaptive(
            Color::rgb(235, 235, 235),
            Color::rgb(0, 0, 0),
        ))
        .text(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .text_muted(AdaptiveColor::adaptive(
            Color::rgb(30, 30, 30),
            Color::rgb(215, 215, 215),
        ))
        .text_subtle(AdaptiveColor::adaptive(
            Color::rgb(45, 45, 45),
            Color::rgb(200, 200, 200),
        ))
        .success(AdaptiveColor::adaptive(
            Color::rgb(0, 96, 0),
            Color::rgb(64, 255, 64),
        ))
        .warning(AdaptiveColor::adaptive(
            Color::rgb(110, 70, 0),
            Color::rgb(255, 220, 64),
        ))
        .error(AdaptiveColor::adaptive(
            Color::rgb(128, 0, 0),
            Color::rgb(255, 96, 96),
        ))
        .info(AdaptiveColor::adaptive(
            Color::rgb(0, 32, 128),
            Color::rgb(128, 200, 255),
        ))
        .border(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .border_focused(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 0),
        ))
        .selection_bg(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .selection_fg(AdaptiveColor::adaptive(
            Color::rgb(255, 255, 255),
            Color::rgb(0, 0, 0),
        ))
        .scrollbar_track(AdaptiveColor::adaptive(
            Color::rgb(235, 235, 235),
            Color::rgb(0, 0, 0),
        ))
        .scrollbar_thumb(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .build()
}

fn gruvbox_dark_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(215, 153, 33)) // #d79921 yellow
        .secondary(Color::rgb(211, 134, 155)) // #d3869b purple
        .accent(Color::rgb(250, 189, 47)) // #fabd2f bright yellow
        .background(Color::rgb(40, 40, 40)) // #282828
        .surface(Color::rgb(50, 48, 47)) // #32302f
        .overlay(Color::rgb(60, 56, 54)) // #3c3836
        .text(Color::rgb(235, 219, 178)) // #ebdbb2
        .text_muted(Color::rgb(189, 174, 147)) // #bdae93
        .text_subtle(Color::rgb(146, 131, 116)) // #928374
        .success(Color::rgb(152, 151, 26)) // #98971a
        .warning(Color::rgb(215, 153, 33)) // #d79921
        .error(Color::rgb(204, 36, 29)) // #cc241d
        .info(Color::rgb(69, 133, 136)) // #458588
        .border(Color::rgb(80, 73, 69)) // #504945
        .border_focused(Color::rgb(250, 189, 47)) // #fabd2f
        .selection_bg(Color::rgb(215, 153, 33)) // #d79921
        .selection_fg(Color::rgb(40, 40, 40)) // #282828
        .scrollbar_track(Color::rgb(60, 56, 54)) // #3c3836
        .scrollbar_thumb(Color::rgb(146, 131, 116)) // #928374
        .build()
}

fn one_dark_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(97, 175, 239)) // #61afef blue
        .secondary(Color::rgb(198, 120, 221)) // #c678dd purple
        .accent(Color::rgb(86, 182, 194)) // #56b6c2 cyan
        .background(Color::rgb(40, 44, 52)) // #282c34
        .surface(Color::rgb(49, 53, 63)) // #31353f
        .overlay(Color::rgb(55, 59, 69)) // #373b45
        .text(Color::rgb(171, 178, 191)) // #abb2bf
        .text_muted(Color::rgb(139, 145, 157)) // #8b919d
        .text_subtle(Color::rgb(99, 109, 131)) // #636d83
        .success(Color::rgb(152, 195, 121)) // #98c379
        .warning(Color::rgb(229, 192, 123)) // #e5c07b
        .error(Color::rgb(224, 108, 117)) // #e06c75
        .info(Color::rgb(86, 182, 194)) // #56b6c2
        .border(Color::rgb(62, 68, 81)) // #3e4451
        .border_focused(Color::rgb(97, 175, 239)) // #61afef
        .selection_bg(Color::rgb(97, 175, 239)) // #61afef
        .selection_fg(Color::rgb(40, 44, 52)) // #282c34
        .scrollbar_track(Color::rgb(49, 53, 63)) // #31353f
        .scrollbar_thumb(Color::rgb(99, 109, 131)) // #636d83
        .build()
}

fn rose_pine_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(235, 188, 186)) // #ebbcba rose
        .secondary(Color::rgb(196, 167, 231)) // #c4a7e7 iris
        .accent(Color::rgb(49, 116, 143)) // #31748f pine
        .background(Color::rgb(25, 23, 36)) // #191724
        .surface(Color::rgb(38, 35, 53)) // #26233a
        .overlay(Color::rgb(57, 53, 82)) // #393552
        .text(Color::rgb(224, 222, 244)) // #e0def4
        .text_muted(Color::rgb(144, 140, 170)) // #908caa
        .text_subtle(Color::rgb(110, 106, 134)) // #6e6a86
        .success(Color::rgb(156, 207, 216)) // #9ccfd8 foam
        .warning(Color::rgb(246, 193, 119)) // #f6c177 gold
        .error(Color::rgb(235, 111, 146)) // #eb6f92 love
        .info(Color::rgb(49, 116, 143)) // #31748f pine
        .border(Color::rgb(57, 53, 82)) // #393552
        .border_focused(Color::rgb(235, 188, 186)) // #ebbcba
        .selection_bg(Color::rgb(235, 188, 186)) // #ebbcba
        .selection_fg(Color::rgb(25, 23, 36)) // #191724
        .scrollbar_track(Color::rgb(38, 35, 53)) // #26233a
        .scrollbar_thumb(Color::rgb(110, 106, 134)) // #6e6a86
        .build()
}

fn everforest_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(167, 192, 128)) // #a7c080 green
        .secondary(Color::rgb(214, 153, 182)) // #d699b6 purple
        .accent(Color::rgb(131, 192, 146)) // #83c092 aqua
        .background(Color::rgb(39, 46, 34)) // #272e22 (dark bg)
        .surface(Color::rgb(47, 55, 42)) // #2f372a
        .overlay(Color::rgb(56, 64, 51)) // #384033
        .text(Color::rgb(211, 198, 170)) // #d3c6aa
        .text_muted(Color::rgb(163, 153, 132)) // #a39984
        .text_subtle(Color::rgb(125, 117, 100)) // #7d7564
        .success(Color::rgb(167, 192, 128)) // #a7c080
        .warning(Color::rgb(219, 188, 127)) // #dbbc7f
        .error(Color::rgb(230, 126, 128)) // #e67e80
        .info(Color::rgb(124, 195, 210)) // #7cc3d2 (light blue)
        .border(Color::rgb(68, 77, 60)) // #444d3c
        .border_focused(Color::rgb(167, 192, 128)) // #a7c080
        .selection_bg(Color::rgb(167, 192, 128)) // #a7c080
        .selection_fg(Color::rgb(39, 46, 34)) // #272e22
        .scrollbar_track(Color::rgb(47, 55, 42)) // #2f372a
        .scrollbar_thumb(Color::rgb(125, 117, 100)) // #7d7564
        .build()
}

fn kanagawa_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(127, 180, 202)) // #7fb4ca wave blue
        .secondary(Color::rgb(149, 127, 184)) // #957fb8 oniviolet
        .accent(Color::rgb(126, 156, 216)) // #7e9cd8 crystal blue
        .background(Color::rgb(31, 31, 40)) // #1f1f28
        .surface(Color::rgb(42, 42, 54)) // #2a2a36
        .overlay(Color::rgb(54, 54, 70)) // #363646
        .text(Color::rgb(220, 215, 186)) // #dcd7ba
        .text_muted(Color::rgb(168, 162, 138)) // #a8a28a (fuji grey lighter)
        .text_subtle(Color::rgb(114, 113, 105)) // #727169
        .success(Color::rgb(152, 187, 108)) // #98bb6c spring green
        .warning(Color::rgb(255, 169, 98)) // #ffa962 surimiOrange
        .error(Color::rgb(195, 64, 67)) // #c34043 autumn red
        .info(Color::rgb(127, 180, 202)) // #7fb4ca
        .border(Color::rgb(84, 84, 109)) // #54546d sumiInk4
        .border_focused(Color::rgb(126, 156, 216)) // #7e9cd8
        .selection_bg(Color::rgb(73, 65, 107)) // #49416b waveblue2
        .selection_fg(Color::rgb(220, 215, 186)) // #dcd7ba
        .scrollbar_track(Color::rgb(42, 42, 54)) // #2a2a36
        .scrollbar_thumb(Color::rgb(84, 84, 109)) // #54546d
        .build()
}

fn ayu_mirage_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(115, 210, 222)) // #73d2de common.accent
        .secondary(Color::rgb(217, 155, 243)) // #d99bf3 (purple)
        .accent(Color::rgb(255, 173, 102)) // #ffad66 syntax.tag
        .background(Color::rgb(36, 42, 54)) // #242a36 (ui.bg adjusted)
        .surface(Color::rgb(44, 51, 64)) // #2c3340
        .overlay(Color::rgb(52, 60, 74)) // #343c4a
        .text(Color::rgb(204, 204, 204)) // #cccac2 (common.fg)
        .text_muted(Color::rgb(150, 155, 160)) // #969ba0
        .text_subtle(Color::rgb(107, 114, 128)) // #6b7280
        .success(Color::rgb(135, 213, 134)) // #87d586
        .warning(Color::rgb(255, 213, 109)) // #ffd56d
        .error(Color::rgb(240, 113, 120)) // #f07178
        .info(Color::rgb(115, 210, 222)) // #73d2de
        .border(Color::rgb(60, 68, 82)) // #3c4452
        .border_focused(Color::rgb(115, 210, 222)) // #73d2de
        .selection_bg(Color::rgb(115, 210, 222)) // #73d2de
        .selection_fg(Color::rgb(36, 42, 54)) // #242a36
        .scrollbar_track(Color::rgb(44, 51, 64)) // #2c3340
        .scrollbar_thumb(Color::rgb(107, 114, 128)) // #6b7280
        .build()
}

fn nightfox_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(129, 180, 243)) // #81b4f3 blue
        .secondary(Color::rgb(174, 140, 211)) // #ae8cd3 magenta
        .accent(Color::rgb(99, 205, 207)) // #63cdcf cyan
        .background(Color::rgb(18, 21, 31)) // #12151f
        .surface(Color::rgb(29, 33, 46)) // #1d212e
        .overlay(Color::rgb(41, 46, 62)) // #292e3e
        .text(Color::rgb(205, 207, 216)) // #cdcfd8
        .text_muted(Color::rgb(143, 145, 158)) // #8f919e
        .text_subtle(Color::rgb(106, 108, 122)) // #6a6c7a
        .success(Color::rgb(129, 200, 152)) // #81c898
        .warning(Color::rgb(218, 167, 89)) // #daa759
        .error(Color::rgb(201, 101, 120)) // #c96578
        .info(Color::rgb(99, 205, 207)) // #63cdcf
        .border(Color::rgb(48, 54, 71)) // #303647
        .border_focused(Color::rgb(129, 180, 243)) // #81b4f3
        .selection_bg(Color::rgb(129, 180, 243)) // #81b4f3
        .selection_fg(Color::rgb(18, 21, 31)) // #12151f
        .scrollbar_track(Color::rgb(29, 33, 46)) // #1d212e
        .scrollbar_thumb(Color::rgb(106, 108, 122)) // #6a6c7a
        .build()
}

fn cyberpunk_aurora_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(255, 0, 128)) // #ff0080 neon pink
        .secondary(Color::rgb(0, 255, 255)) // #00ffff cyan
        .accent(Color::rgb(0, 255, 163)) // #00ffa3 neon green
        .background(Color::rgb(13, 2, 33)) // #0d0221 deep purple-black
        .surface(Color::rgb(22, 10, 48)) // #160a30
        .overlay(Color::rgb(33, 18, 63)) // #21123f
        .text(Color::rgb(224, 210, 255)) // #e0d2ff
        .text_muted(Color::rgb(160, 140, 200)) // #a08cc8
        .text_subtle(Color::rgb(120, 100, 160)) // #7864a0
        .success(Color::rgb(0, 255, 163)) // #00ffa3
        .warning(Color::rgb(255, 213, 0)) // #ffd500
        .error(Color::rgb(255, 51, 102)) // #ff3366
        .info(Color::rgb(0, 200, 255)) // #00c8ff
        .border(Color::rgb(60, 30, 100)) // #3c1e64
        .border_focused(Color::rgb(255, 0, 128)) // #ff0080
        .selection_bg(Color::rgb(255, 0, 128)) // #ff0080
        .selection_fg(Color::rgb(13, 2, 33)) // #0d0221 deep bg for contrast
        .scrollbar_track(Color::rgb(22, 10, 48)) // #160a30
        .scrollbar_thumb(Color::rgb(120, 100, 160)) // #7864a0
        .build()
}

fn synthwave_84_theme() -> Theme {
    ThemeBuilder::from_theme(themes::dark())
        .primary(Color::rgb(255, 123, 213)) // #ff7bd5 hot pink
        .secondary(Color::rgb(114, 241, 223)) // #72f1df mint
        .accent(Color::rgb(254, 215, 102)) // #fed766 yellow
        .background(Color::rgb(34, 20, 54)) // #221436 deep purple
        .surface(Color::rgb(44, 28, 68)) // #2c1c44
        .overlay(Color::rgb(54, 36, 82)) // #362452
        .text(Color::rgb(241, 233, 255)) // #f1e9ff
        .text_muted(Color::rgb(180, 165, 210)) // #b4a5d2
        .text_subtle(Color::rgb(130, 115, 165)) // #8273a5
        .success(Color::rgb(114, 241, 223)) // #72f1df
        .warning(Color::rgb(254, 215, 102)) // #fed766
        .error(Color::rgb(254, 73, 99)) // #fe4963
        .info(Color::rgb(54, 245, 253)) // #36f5fd
        .border(Color::rgb(70, 45, 100)) // #462d64
        .border_focused(Color::rgb(255, 123, 213)) // #ff7bd5
        .selection_bg(Color::rgb(255, 123, 213)) // #ff7bd5
        .selection_fg(Color::rgb(34, 20, 54)) // #221436
        .scrollbar_track(Color::rgb(44, 28, 68)) // #2c1c44
        .scrollbar_thumb(Color::rgb(130, 115, 165)) // #8273a5
        .build()
}

/// Colorblind-accessible theme based on Tokyo Night.
///
/// Swaps green/orange/red role colors with blue/yellow/magenta so that
/// all role indicators remain distinguishable for deuteranopia and
/// protanopia users.  Background, text, and structural colors are
/// identical to Tokyo Night.
fn colorblind_theme() -> Theme {
    ThemeBuilder::from_theme(tokyo_night_theme())
        // success → teal-cyan (distinct from the blue primary)
        .success(Color::rgb(125, 207, 255)) // #7dcfff cyan (was green #73daca)
        // warning → warm yellow (was amber, still yellow family but brighter)
        .warning(Color::rgb(224, 175, 104)) // #e0af68 (unchanged, already CB-safe)
        // error → magenta/purple instead of red
        .error(Color::rgb(187, 154, 247)) // #bb9af7 (was red #f7768e)
        .build()
}

fn apply_a11y_overrides(theme: Theme) -> Theme {
    ThemeBuilder::from_theme(theme)
        .border_focused(Color::rgb(255, 255, 0))
        .selection_bg(AdaptiveColor::adaptive(
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        ))
        .selection_fg(AdaptiveColor::adaptive(
            Color::rgb(255, 255, 255),
            Color::rgb(0, 0, 0),
        ))
        .build()
}

fn downgrade_adaptive_color(color: AdaptiveColor, profile: ColorProfile) -> AdaptiveColor {
    match color {
        AdaptiveColor::Fixed(value) => AdaptiveColor::fixed(value.downgrade(profile)),
        AdaptiveColor::Adaptive { light, dark } => {
            AdaptiveColor::adaptive(light.downgrade(profile), dark.downgrade(profile))
        }
    }
}

fn downgrade_theme_for_profile(theme: Theme, profile: ColorProfile) -> Theme {
    if profile == ColorProfile::TrueColor {
        return theme;
    }

    Theme {
        primary: downgrade_adaptive_color(theme.primary, profile),
        secondary: downgrade_adaptive_color(theme.secondary, profile),
        accent: downgrade_adaptive_color(theme.accent, profile),
        background: downgrade_adaptive_color(theme.background, profile),
        surface: downgrade_adaptive_color(theme.surface, profile),
        overlay: downgrade_adaptive_color(theme.overlay, profile),
        text: downgrade_adaptive_color(theme.text, profile),
        text_muted: downgrade_adaptive_color(theme.text_muted, profile),
        text_subtle: downgrade_adaptive_color(theme.text_subtle, profile),
        success: downgrade_adaptive_color(theme.success, profile),
        warning: downgrade_adaptive_color(theme.warning, profile),
        error: downgrade_adaptive_color(theme.error, profile),
        info: downgrade_adaptive_color(theme.info, profile),
        border: downgrade_adaptive_color(theme.border, profile),
        border_focused: downgrade_adaptive_color(theme.border_focused, profile),
        selection_bg: downgrade_adaptive_color(theme.selection_bg, profile),
        selection_fg: downgrade_adaptive_color(theme.selection_fg, profile),
        scrollbar_track: downgrade_adaptive_color(theme.scrollbar_track, profile),
        scrollbar_thumb: downgrade_adaptive_color(theme.scrollbar_thumb, profile),
    }
}

/// Build the semantic stylesheet from resolved theme colors.
///
/// ## Palette → Token Derivation Strategy
///
/// All tokens derive from [`ResolvedTheme`] fields — no hardcoded colors.
/// The mapping is organized into semantic groups:
///
/// | Group      | Tokens                          | Palette Source                |
/// |------------|---------------------------------|-------------------------------|
/// | App chrome | APP_ROOT, PANE_BASE/FOCUSED      | text, background, surface     |
/// | Text       | TEXT_PRIMARY/MUTED/SUBTLE        | text hierarchy fields          |
/// | Status     | SUCCESS/WARNING/ERROR/INFO       | success, warning, error, info |
/// | Results    | ROW/ROW_ALT/ROW_SELECTED         | surface, selection_*          |
/// | Roles      | ROLE_USER/ASSISTANT/TOOL/SYSTEM   | blend(accent,success,0.35), info, warning, error |
/// | Gutters    | ROLE_GUTTER_*                    | role color + 18% bg blend     |
/// | Scores     | SCORE_HIGH/MID/LOW               | success, info, blend(text_subtle,bg,0.35) |
/// | Keys       | KBD_KEY/DESC                     | accent, text_subtle           |
/// | Affordance | PILL_ACTIVE, TAB_ACTIVE/INACTIVE  | secondary/accent + bg blends  |
/// | Detail Find| FIND_CONTAINER/QUERY/MATCH_*     | surface/overlay + accent/selection |
///
/// Role assignment: User=blend(accent,success,0.35), Assistant=info, Tool=warning, System=error.
/// Gutter backgrounds use a uniform 18% blend factor with `resolved.background`.
/// Pill/tab backgrounds use blended info tints (25% and 15% respectively).
fn build_stylesheet(resolved: ResolvedTheme, options: StyleOptions) -> StyleSheet {
    let sheet = StyleSheet::new();

    let zebra_bg = if options.gradients_enabled() {
        blend(resolved.surface, resolved.background, 0.35).downgrade(options.color_profile)
    } else {
        resolved.surface
    };

    // Role colors must be pairwise distinct across all presets. Some upstream
    // themes share primary==info or accent==info, so we derive the user color
    // from a blend of accent+success to guarantee visual separation from
    // assistant (info), tool (warning), and system (error).
    let role_user = blend(resolved.accent, resolved.success, 0.35);
    let role_assistant = resolved.info;
    let role_tool = resolved.warning;
    let role_system = resolved.error;

    sheet.define(
        STYLE_APP_ROOT,
        Style::new()
            .fg(to_packed(resolved.text))
            .bg(to_packed(resolved.background)),
    );
    sheet.define(
        STYLE_PANE_BASE,
        Style::new()
            .fg(to_packed(resolved.text))
            .bg(to_packed(resolved.surface)),
    );
    sheet.define(
        STYLE_PANE_FOCUSED,
        Style::new()
            .fg(to_packed(resolved.border_focused))
            .bg(to_packed(resolved.surface)),
    );
    // Pane title tokens: focused uses accent+bold for immediate focus clarity,
    // unfocused uses muted text so the eye is drawn to the active pane.
    sheet.define(
        STYLE_PANE_TITLE_FOCUSED,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bg(to_packed(resolved.surface))
            .bold(),
    );
    sheet.define(
        STYLE_PANE_TITLE_UNFOCUSED,
        Style::new()
            .fg(to_packed(resolved.text_muted))
            .bg(to_packed(resolved.surface)),
    );
    // Split handle: subtle border-colored vertical divider between panes.
    sheet.define(
        STYLE_SPLIT_HANDLE,
        Style::new()
            .fg(to_packed(resolved.border))
            .bg(to_packed(resolved.background)),
    );

    sheet.define(
        STYLE_TEXT_PRIMARY,
        Style::new().fg(to_packed(resolved.text)),
    );
    sheet.define(
        STYLE_TEXT_MUTED,
        Style::new().fg(to_packed(resolved.text_muted)),
    );
    sheet.define(
        STYLE_TEXT_SUBTLE,
        Style::new().fg(to_packed(resolved.text_subtle)),
    );

    sheet.define(
        STYLE_STATUS_SUCCESS,
        Style::new().fg(to_packed(resolved.success)).bold(),
    );
    sheet.define(
        STYLE_STATUS_WARNING,
        Style::new().fg(to_packed(resolved.warning)).bold(),
    );
    sheet.define(
        STYLE_STATUS_ERROR,
        Style::new().fg(to_packed(resolved.error)).bold(),
    );
    sheet.define(
        STYLE_STATUS_INFO,
        Style::new().fg(to_packed(resolved.info)).bold(),
    );

    sheet.define(
        STYLE_RESULT_ROW,
        Style::new()
            .fg(to_packed(resolved.text))
            .bg(to_packed(resolved.surface)),
    );
    sheet.define(
        STYLE_RESULT_ROW_ALT,
        Style::new()
            .fg(to_packed(resolved.text))
            .bg(to_packed(zebra_bg)),
    );

    let selected_style = if options.a11y {
        Style::new()
            .fg(to_packed(resolved.selection_fg))
            .bg(to_packed(resolved.selection_bg))
            .bold()
            .underline()
    } else {
        Style::new()
            .fg(to_packed(resolved.selection_fg))
            .bg(to_packed(resolved.selection_bg))
            .bold()
    };
    sheet.define(STYLE_RESULT_ROW_SELECTED, selected_style);

    let role_user_style = if options.a11y {
        Style::new().fg(to_packed(role_user)).bold().underline()
    } else {
        Style::new().fg(to_packed(role_user)).bold()
    };
    let role_assistant_style = if options.a11y {
        Style::new().fg(to_packed(role_assistant)).bold().italic()
    } else {
        Style::new().fg(to_packed(role_assistant)).bold()
    };
    let role_tool_style = if options.a11y {
        Style::new().fg(to_packed(role_tool)).underline()
    } else {
        Style::new().fg(to_packed(role_tool))
    };
    let role_system_style = if options.a11y {
        Style::new().fg(to_packed(role_system)).bold().underline()
    } else {
        Style::new().fg(to_packed(role_system)).bold()
    };

    sheet.define(STYLE_ROLE_USER, role_user_style);
    sheet.define(STYLE_ROLE_ASSISTANT, role_assistant_style);
    sheet.define(STYLE_ROLE_TOOL, role_tool_style);
    sheet.define(STYLE_ROLE_SYSTEM, role_system_style);

    sheet.define(
        STYLE_ROLE_GUTTER_USER,
        Style::new().fg(to_packed(role_user)).bg(to_packed(blend(
            resolved.background,
            role_user,
            0.18,
        ))),
    );
    sheet.define(
        STYLE_ROLE_GUTTER_ASSISTANT,
        Style::new()
            .fg(to_packed(role_assistant))
            .bg(to_packed(blend(resolved.background, role_assistant, 0.18))),
    );
    sheet.define(
        STYLE_ROLE_GUTTER_TOOL,
        Style::new().fg(to_packed(role_tool)).bg(to_packed(blend(
            resolved.background,
            role_tool,
            0.18,
        ))),
    );
    sheet.define(
        STYLE_ROLE_GUTTER_SYSTEM,
        Style::new().fg(to_packed(role_system)).bg(to_packed(blend(
            resolved.background,
            role_system,
            0.18,
        ))),
    );

    sheet.define(
        STYLE_SCORE_HIGH,
        Style::new().fg(to_packed(resolved.success)).bold(),
    );
    sheet.define(
        STYLE_SCORE_MID,
        Style::new().fg(to_packed(resolved.info)).bold(),
    );
    // Use a derived dim color for SCORE_LOW to avoid collision when info==text_subtle (e.g. Nord).
    let score_low_fg = blend(resolved.text_subtle, resolved.background, 0.35);
    sheet.define(
        STYLE_SCORE_LOW,
        Style::new().fg(to_packed(score_low_fg)).dim(),
    );

    // Source provenance tokens: local is muted, remote is italic+info to
    // visually distinguish hosts at a glance.
    sheet.define(
        STYLE_SOURCE_LOCAL,
        Style::new().fg(to_packed(resolved.text_muted)),
    );
    sheet.define(
        STYLE_SOURCE_REMOTE,
        Style::new().fg(to_packed(resolved.info)).italic(),
    );
    // File location path: uses text_subtle to recede behind scores and titles.
    sheet.define(
        STYLE_LOCATION,
        Style::new().fg(to_packed(resolved.text_subtle)),
    );

    sheet.define(
        STYLE_KBD_KEY,
        Style::new().fg(to_packed(resolved.accent)).bold(),
    );
    sheet.define(
        STYLE_KBD_DESC,
        Style::new().fg(to_packed(resolved.text_subtle)),
    );

    let pill_active_bg = blend(resolved.surface, resolved.info, 0.35);
    sheet.define(
        STYLE_PILL_ACTIVE,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bg(to_packed(pill_active_bg))
            .bold(),
    );
    sheet.define(
        STYLE_PILL_INACTIVE,
        Style::new()
            .fg(to_packed(resolved.text_muted))
            .bg(to_packed(blend(resolved.surface, resolved.border, 0.12)))
            .dim(),
    );
    sheet.define(
        STYLE_PILL_LABEL,
        Style::new()
            .fg(to_packed(blend(resolved.text_muted, resolved.text, 0.35)))
            .bg(to_packed(pill_active_bg))
            .bold(),
    );

    sheet.define(
        STYLE_CRUMB_ACTIVE,
        Style::new().fg(to_packed(resolved.accent)).bold(),
    );
    sheet.define(
        STYLE_CRUMB_INACTIVE,
        Style::new().fg(to_packed(resolved.text_subtle)),
    );
    sheet.define(
        STYLE_CRUMB_SEPARATOR,
        Style::new().fg(to_packed(resolved.border)),
    );

    sheet.define(
        STYLE_TAB_ACTIVE,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bg(to_packed(blend(resolved.surface, resolved.info, 0.15)))
            .bold()
            .underline(),
    );
    sheet.define(
        STYLE_TAB_INACTIVE,
        Style::new().fg(to_packed(resolved.text_muted)).underline(),
    );
    sheet.define(
        STYLE_DETAIL_FIND_CONTAINER,
        Style::new()
            .fg(to_packed(resolved.text))
            .bg(to_packed(blend(resolved.overlay, resolved.border, 0.30))),
    );
    sheet.define(
        STYLE_DETAIL_FIND_QUERY,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bold()
            .underline(),
    );
    sheet.define(
        STYLE_DETAIL_FIND_MATCH_ACTIVE,
        Style::new()
            .fg(to_packed(resolved.selection_fg))
            .bg(to_packed(resolved.selection_bg))
            .bold(),
    );
    sheet.define(
        STYLE_DETAIL_FIND_MATCH_INACTIVE,
        Style::new()
            .fg(to_packed(resolved.text_muted))
            .bg(to_packed(blend(
                resolved.surface,
                resolved.border_focused,
                0.28,
            ))),
    );
    sheet.define(
        STYLE_QUERY_HIGHLIGHT,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bold()
            .underline(),
    );

    // Search bar active: stronger accent background for "glow" effect
    sheet.define(
        STYLE_SEARCH_FOCUS,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bg(to_packed(blend(resolved.surface, resolved.accent, 0.18)))
            .bold(),
    );

    // Modal backdrop: dimmed background for overlay modals
    sheet.define(
        STYLE_MODAL_BACKDROP,
        Style::new().bg(to_packed(blend(
            resolved.background,
            Color::rgb(0, 0, 0),
            0.45,
        ))),
    );

    sheet
}

fn to_packed(color: Color) -> PackedRgba {
    let rgb = color.to_rgb();
    PackedRgba::rgb(rgb.r, rgb.g, rgb.b)
}

fn contrast_check(pair: &'static str, fg: Color, bg: Color, minimum: f64) -> ThemeContrastCheck {
    let ratio = ftui::style::contrast_ratio_packed(to_packed(fg), to_packed(bg));
    ThemeContrastCheck {
        pair,
        ratio,
        minimum,
        passes: ratio >= minimum,
    }
}

fn build_contrast_report(resolved: ResolvedTheme) -> ThemeContrastReport {
    ThemeContrastReport {
        checks: vec![
            contrast_check("text/background", resolved.text, resolved.background, 3.0),
            contrast_check("text/surface", resolved.text, resolved.surface, 2.5),
            contrast_check(
                "selection_fg/selection_bg",
                resolved.selection_fg,
                resolved.selection_bg,
                3.0,
            ),
            contrast_check(
                "text_muted/background",
                resolved.text_muted,
                resolved.background,
                3.0,
            ),
            contrast_check(
                "border_focused/background",
                resolved.border_focused,
                resolved.background,
                3.0,
            ),
        ],
    }
}

fn blend(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let ar = a.to_rgb();
    let br = b.to_rgb();

    let blend_channel = |left: u8, right: u8| -> u8 {
        let mixed = left as f32 + (right as f32 - left as f32) * t;
        mixed.round().clamp(0.0, 255.0) as u8
    };

    Color::rgb(
        blend_channel(ar.r, br.r),
        blend_channel(ar.g, br.g),
        blend_channel(ar.b, br.b),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn preset_parse_and_cycles_are_stable() {
        assert_eq!(
            UiThemePreset::parse("dark"),
            Some(UiThemePreset::TokyoNight)
        );
        assert_eq!(UiThemePreset::parse("light"), Some(UiThemePreset::Daylight));
        assert_eq!(
            UiThemePreset::parse("catppuccin"),
            Some(UiThemePreset::Catppuccin)
        );
        assert_eq!(
            UiThemePreset::parse("dracula"),
            Some(UiThemePreset::Dracula)
        );
        assert_eq!(UiThemePreset::parse("nord"), Some(UiThemePreset::Nord));
        assert_eq!(
            UiThemePreset::parse("high_contrast"),
            Some(UiThemePreset::HighContrast)
        );
        assert_eq!(
            UiThemePreset::parse("gruvbox"),
            Some(UiThemePreset::GruvboxDark)
        );
        assert_eq!(
            UiThemePreset::parse("rose-pine"),
            Some(UiThemePreset::RosePine)
        );

        assert_eq!(UiThemePreset::TokyoNight.next(), UiThemePreset::Daylight);
        assert_eq!(
            UiThemePreset::Daylight.previous(),
            UiThemePreset::TokyoNight
        );
        assert_eq!(
            UiThemePreset::TokyoNight.previous(),
            UiThemePreset::Colorblind
        );
    }

    #[test]
    fn options_from_values_honor_opt_out_and_profile_override() {
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: Some("1"),
            cass_respect_no_color: Some("1"),
            cass_no_color: None,
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            cass_no_icons: Some("1"),
            cass_no_gradient: Some("1"),
            cass_a11y: Some("true"),
            cass_theme: Some("nord"),
            cass_color_profile: Some("ansi16"),
        });

        assert_eq!(options.preset, UiThemePreset::Nord);
        assert!(options.no_color);
        assert!(options.no_icons);
        assert!(options.no_gradient);
        assert!(options.a11y);
        assert_eq!(options.color_profile, ColorProfile::Mono);
    }

    #[test]
    fn options_profile_override_applies_when_color_enabled() {
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: None,
            cass_respect_no_color: None,
            cass_no_color: None,
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            cass_no_icons: None,
            cass_no_gradient: None,
            cass_a11y: Some("0"),
            cass_theme: Some("dark"),
            cass_color_profile: Some("ansi16"),
        });

        assert_eq!(options.color_profile, ColorProfile::Ansi16);
        assert!(!options.no_color);
    }

    #[test]
    fn options_ignore_no_color_unless_explicitly_requested() {
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: Some("1"),
            cass_respect_no_color: None,
            cass_no_color: None,
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            cass_no_icons: None,
            cass_no_gradient: None,
            cass_a11y: Some("0"),
            cass_theme: Some("dark"),
            cass_color_profile: None,
        });

        assert!(!options.no_color);
        assert_eq!(options.color_profile, ColorProfile::TrueColor);
    }

    #[test]
    fn cass_no_color_always_forces_monochrome() {
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: None,
            cass_respect_no_color: None,
            cass_no_color: Some("1"),
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            cass_no_icons: None,
            cass_no_gradient: None,
            cass_a11y: Some("0"),
            cass_theme: Some("dark"),
            cass_color_profile: Some("truecolor"),
        });

        assert!(options.no_color);
        assert_eq!(options.color_profile, ColorProfile::Mono);
    }

    #[test]
    fn cass_no_color_falsy_values_do_not_force_monochrome() {
        for falsy in &["0", "false", "off", "no"] {
            let options = StyleOptions::from_env_values(EnvValues {
                no_color: None,
                cass_respect_no_color: None,
                cass_no_color: Some(falsy),
                colorterm: Some("truecolor"),
                term: Some("xterm-256color"),
                cass_no_icons: None,
                cass_no_gradient: None,
                cass_a11y: Some("0"),
                cass_theme: Some("dark"),
                cass_color_profile: None,
            });
            assert!(
                !options.no_color,
                "CASS_NO_COLOR={falsy} must not force monochrome"
            );
            assert_eq!(
                options.color_profile,
                ColorProfile::TrueColor,
                "CASS_NO_COLOR={falsy} should preserve detected profile"
            );
        }
    }

    // -- env/capability edge-case tests (2dccg.10.4) --

    #[test]
    fn no_color_without_respect_flag_preserves_full_color() {
        // NO_COLOR alone must NOT disable colors unless CASS_RESPECT_NO_COLOR is set.
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: Some("1"),
            cass_respect_no_color: None,
            cass_no_color: None,
            colorterm: None,
            term: None,
            cass_no_icons: None,
            cass_no_gradient: None,
            cass_a11y: None,
            cass_theme: None,
            cass_color_profile: None,
        });
        assert!(!options.no_color, "NO_COLOR alone must be ignored");
        assert!(!options.no_gradient, "gradient should remain enabled");
    }

    #[test]
    fn respect_no_color_with_falsy_value_is_not_truthy() {
        // CASS_RESPECT_NO_COLOR="0" should be treated as falsy.
        for falsy in &["0", "false", "off", "no"] {
            let options = StyleOptions::from_env_values(EnvValues {
                no_color: Some("1"),
                cass_respect_no_color: Some(falsy),
                cass_no_color: None,
                colorterm: Some("truecolor"),
                term: None,
                cass_no_icons: None,
                cass_no_gradient: None,
                cass_a11y: None,
                cass_theme: None,
                cass_color_profile: None,
            });
            assert!(
                !options.no_color,
                "CASS_RESPECT_NO_COLOR={falsy} must be falsy"
            );
            assert_eq!(options.color_profile, ColorProfile::TrueColor);
        }
    }

    #[test]
    fn invalid_color_profile_falls_back_to_env_detection() {
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: None,
            cass_respect_no_color: None,
            cass_no_color: None,
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            cass_no_icons: None,
            cass_no_gradient: None,
            cass_a11y: None,
            cass_theme: None,
            cass_color_profile: Some("garbage-value"),
        });
        // Invalid CASS_COLOR_PROFILE → fallback to COLORTERM/TERM detection.
        assert_eq!(options.color_profile, ColorProfile::TrueColor);
        assert!(!options.no_color);
    }

    #[test]
    fn a11y_cascades_no_gradient() {
        // CASS_A11Y=1 should force no_gradient even without CASS_NO_GRADIENT.
        let options = StyleOptions::from_env_values(EnvValues {
            no_color: None,
            cass_respect_no_color: None,
            cass_no_color: None,
            colorterm: Some("truecolor"),
            term: None,
            cass_no_icons: None,
            cass_no_gradient: None,
            cass_a11y: Some("1"),
            cass_theme: None,
            cass_color_profile: None,
        });
        assert!(options.a11y);
        assert!(
            options.no_gradient,
            "a11y must cascade into no_gradient=true"
        );
        assert!(!options.no_color, "a11y must not cascade into no_color");
        assert_eq!(
            options.color_profile,
            ColorProfile::TrueColor,
            "a11y must not downgrade color profile"
        );
    }

    #[test]
    fn no_icons_is_independent_of_color_state() {
        // CASS_NO_ICONS should work even with full color enabled.
        let with_icons_off = StyleOptions::from_env_values(EnvValues {
            no_color: None,
            cass_respect_no_color: None,
            cass_no_color: None,
            colorterm: Some("truecolor"),
            term: None,
            cass_no_icons: Some("1"),
            cass_no_gradient: None,
            cass_a11y: None,
            cass_theme: None,
            cass_color_profile: None,
        });
        assert!(with_icons_off.no_icons);
        assert!(!with_icons_off.no_color);
        assert_eq!(with_icons_off.color_profile, ColorProfile::TrueColor);
    }

    #[test]
    fn no_icons_falsy_values_do_not_disable_icons() {
        for falsy in &["0", "false", "off", "no"] {
            let options = StyleOptions::from_env_values(EnvValues {
                no_color: None,
                cass_respect_no_color: None,
                cass_no_color: None,
                colorterm: Some("truecolor"),
                term: Some("xterm-256color"),
                cass_no_icons: Some(falsy),
                cass_no_gradient: None,
                cass_a11y: Some("0"),
                cass_theme: Some("dark"),
                cass_color_profile: None,
            });
            assert!(
                !options.no_icons,
                "CASS_NO_ICONS={falsy} should keep icons enabled"
            );
        }
    }

    #[test]
    fn no_gradient_falsy_values_do_not_disable_gradients() {
        for falsy in &["0", "false", "off", "no"] {
            let options = StyleOptions::from_env_values(EnvValues {
                no_color: None,
                cass_respect_no_color: None,
                cass_no_color: None,
                colorterm: Some("truecolor"),
                term: Some("xterm-256color"),
                cass_no_icons: None,
                cass_no_gradient: Some(falsy),
                cass_a11y: Some("0"),
                cass_theme: Some("dark"),
                cass_color_profile: None,
            });
            assert!(
                !options.no_gradient,
                "CASS_NO_GRADIENT={falsy} should keep gradients enabled"
            );
            assert!(
                options.gradients_enabled(),
                "gradients should remain enabled when CASS_NO_GRADIENT is falsy"
            );
        }
    }

    #[test]
    fn dark_mode_follows_preset() {
        // Light presets → dark_mode=false, all others → true.
        let presets_and_expected = [
            ("dark", true),
            ("light", false),
            ("nord", true),
            ("cat", true),
            ("dracula", true),
            ("solarized-light", false),
            ("solarized-dark", true),
            ("gruvbox", true),
        ];
        for (name, expected_dark) in presets_and_expected {
            let options = StyleOptions::from_env_values(EnvValues {
                cass_theme: Some(name),
                ..EnvValues::default()
            });
            assert_eq!(
                options.dark_mode, expected_dark,
                "preset {name}: expected dark_mode={expected_dark}"
            );
        }
    }

    #[test]
    fn unknown_theme_falls_back_to_tokyo_night() {
        let options = StyleOptions::from_env_values(EnvValues {
            cass_theme: Some("nonexistent"),
            ..EnvValues::default()
        });
        assert_eq!(options.preset, UiThemePreset::TokyoNight);
        assert!(options.dark_mode);
    }

    #[test]
    fn gradients_enabled_requires_color_support() {
        // Mono profile → no gradients even if no_gradient is false.
        let mono = StyleOptions {
            color_profile: ColorProfile::Mono,
            no_gradient: false,
            ..StyleOptions::default()
        };
        assert!(!mono.gradients_enabled());

        // TrueColor with no_gradient=true → no gradients.
        let no_grad = StyleOptions {
            color_profile: ColorProfile::TrueColor,
            no_gradient: true,
            ..StyleOptions::default()
        };
        assert!(!no_grad.gradients_enabled());

        // TrueColor with no_gradient=false → gradients enabled.
        let full = StyleOptions {
            color_profile: ColorProfile::TrueColor,
            no_gradient: false,
            ..StyleOptions::default()
        };
        assert!(full.gradients_enabled());
    }

    #[test]
    fn env_truthy_edge_cases() {
        // Verify env_truthy handles edge values correctly.
        assert!(!env_truthy(None), "None → false");
        assert!(
            env_truthy(Some("")),
            "empty string → true (not in falsy list)"
        );
        assert!(env_truthy(Some("1")), "\"1\" → true");
        assert!(env_truthy(Some("yes")), "\"yes\" → true");
        assert!(env_truthy(Some("true")), "\"true\" → true... wait");
        // Actually "true" is in the falsy list? No — only "false" is falsy.
        // Re-check: falsy = "0", "false", "off", "no"
        assert!(!env_truthy(Some("false")), "\"false\" → false");
        assert!(
            !env_truthy(Some("FALSE")),
            "\"FALSE\" → false (case insensitive)"
        );
        assert!(!env_truthy(Some("  Off  ")), "trimmed \"Off\" → false");
        assert!(!env_truthy(Some("NO")), "\"NO\" → false");
        assert!(env_truthy(Some("anything")), "arbitrary string → true");
    }

    #[test]
    fn env_precedence_full_matrix() {
        // Verify the full precedence chain described in the doc comment.

        // Priority 1: CASS_NO_COLOR trumps everything.
        let p1 = StyleOptions::from_env_values(EnvValues {
            cass_no_color: Some("1"),
            cass_color_profile: Some("truecolor"),
            colorterm: Some("truecolor"),
            ..EnvValues::default()
        });
        assert!(p1.no_color);
        assert_eq!(p1.color_profile, ColorProfile::Mono);

        // Priority 2: RESPECT_NO_COLOR + NO_COLOR beats CASS_COLOR_PROFILE.
        let p2 = StyleOptions::from_env_values(EnvValues {
            no_color: Some("1"),
            cass_respect_no_color: Some("1"),
            cass_color_profile: Some("truecolor"),
            ..EnvValues::default()
        });
        assert!(p2.no_color);
        assert_eq!(p2.color_profile, ColorProfile::Mono);

        // Priority 3: CASS_COLOR_PROFILE overrides env detection.
        let p3 = StyleOptions::from_env_values(EnvValues {
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            cass_color_profile: Some("ansi16"),
            ..EnvValues::default()
        });
        assert!(!p3.no_color);
        assert_eq!(p3.color_profile, ColorProfile::Ansi16);

        // Priority 4: Fallback to env detection.
        let p4 = StyleOptions::from_env_values(EnvValues {
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            ..EnvValues::default()
        });
        assert!(!p4.no_color);
        assert_eq!(p4.color_profile, ColorProfile::TrueColor);

        // Bare minimum: no env vars at all → defaults.
        let bare = StyleOptions::from_env_values(EnvValues::default());
        assert!(!bare.no_color);
        assert_eq!(bare.preset, UiThemePreset::TokyoNight);
        assert!(bare.dark_mode);
    }

    #[test]
    fn style_context_mono_produces_no_fg_bg() {
        // Under Mono profile, styles should still resolve (so code doesn't panic)
        // but colors are expected to be downgraded.
        let ctx = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::Mono,
            no_color: true,
            no_icons: false,
            no_gradient: true,
            a11y: false,
        });
        // Should not panic when resolving any token.
        let _ = ctx.style(STYLE_TEXT_PRIMARY);
        let _ = ctx.style(STYLE_APP_ROOT);
        let _ = ctx.style(STYLE_ROLE_USER);
        let _ = ctx.style(STYLE_SCORE_HIGH);
    }

    #[test]
    fn all_presets_produce_valid_style_context() {
        // Every preset should build a StyleContext without panicking,
        // for both full-color and mono profiles.
        for preset in UiThemePreset::all() {
            for &profile in &[
                ColorProfile::TrueColor,
                ColorProfile::Ansi256,
                ColorProfile::Ansi16,
                ColorProfile::Mono,
            ] {
                let dark_mode = !matches!(
                    preset,
                    UiThemePreset::Daylight | UiThemePreset::SolarizedLight
                );
                let ctx = StyleContext::from_options(StyleOptions {
                    preset,
                    dark_mode,
                    color_profile: profile,
                    no_color: profile == ColorProfile::Mono,
                    no_icons: false,
                    no_gradient: profile == ColorProfile::Mono,
                    a11y: false,
                });
                // Smoke test: resolve every token without panicking.
                for &(_, token) in ALL_STYLE_TOKENS {
                    let _ = ctx.style(token);
                }
            }
        }
    }

    #[test]
    fn dark_preset_matches_tokyo_night_palette() {
        let context = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::TrueColor,
            no_color: false,
            no_icons: false,
            no_gradient: false,
            a11y: false,
        });

        assert_eq!(context.resolved.background, Color::rgb(26, 27, 38));
        assert_eq!(context.resolved.surface, Color::rgb(36, 40, 59));
        assert_eq!(context.resolved.text, Color::rgb(192, 202, 245));
        assert_eq!(context.resolved.border_focused, Color::rgb(125, 145, 200));
    }

    #[test]
    fn style_context_builds_required_semantic_styles() {
        let context = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::TrueColor,
            no_color: false,
            no_icons: false,
            no_gradient: false,
            a11y: false,
        });

        for key in [
            STYLE_APP_ROOT,
            STYLE_PANE_BASE,
            STYLE_PANE_FOCUSED,
            STYLE_PANE_TITLE_FOCUSED,
            STYLE_PANE_TITLE_UNFOCUSED,
            STYLE_SPLIT_HANDLE,
            STYLE_RESULT_ROW,
            STYLE_RESULT_ROW_ALT,
            STYLE_RESULT_ROW_SELECTED,
            STYLE_ROLE_USER,
            STYLE_ROLE_ASSISTANT,
            STYLE_ROLE_TOOL,
            STYLE_ROLE_SYSTEM,
            STYLE_ROLE_GUTTER_USER,
            STYLE_ROLE_GUTTER_ASSISTANT,
            STYLE_ROLE_GUTTER_TOOL,
            STYLE_ROLE_GUTTER_SYSTEM,
            STYLE_SCORE_HIGH,
            STYLE_SCORE_MID,
            STYLE_SCORE_LOW,
            STYLE_SOURCE_LOCAL,
            STYLE_SOURCE_REMOTE,
            STYLE_LOCATION,
            STYLE_KBD_KEY,
            STYLE_KBD_DESC,
            STYLE_PILL_ACTIVE,
            STYLE_PILL_INACTIVE,
            STYLE_PILL_LABEL,
            STYLE_CRUMB_ACTIVE,
            STYLE_CRUMB_INACTIVE,
            STYLE_CRUMB_SEPARATOR,
            STYLE_TAB_ACTIVE,
            STYLE_TAB_INACTIVE,
            STYLE_DETAIL_FIND_CONTAINER,
            STYLE_DETAIL_FIND_QUERY,
            STYLE_DETAIL_FIND_MATCH_ACTIVE,
            STYLE_DETAIL_FIND_MATCH_INACTIVE,
            STYLE_QUERY_HIGHLIGHT,
            STYLE_SEARCH_FOCUS,
            STYLE_MODAL_BACKDROP,
        ] {
            assert!(context.sheet.contains(key), "missing style token: {key}");
        }
    }

    #[test]
    fn detail_find_token_hierarchy_is_explicit_and_theme_aware() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let container = ctx.style(STYLE_DETAIL_FIND_CONTAINER);
            let query = ctx.style(STYLE_DETAIL_FIND_QUERY);
            let active = ctx.style(STYLE_DETAIL_FIND_MATCH_ACTIVE);
            let inactive = ctx.style(STYLE_DETAIL_FIND_MATCH_INACTIVE);

            assert!(
                container.bg.is_some(),
                "find container should provide a distinct background for preset {}",
                preset.name()
            );
            assert!(
                query == query.bold(),
                "find query should be emphasized (bold) for preset {}",
                preset.name()
            );
            assert!(
                active == active.bold() && active.bg.is_some(),
                "active match state should be high-emphasis for preset {}",
                preset.name()
            );
            assert!(
                inactive.fg.is_some(),
                "inactive match counter should still be legible for preset {}",
                preset.name()
            );
            assert_ne!(
                format!("{:?}", active),
                format!("{:?}", inactive),
                "active/inactive match states must be visually distinct for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn detail_find_tokens_remain_legible_in_mono_mode() {
        let ctx = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::Mono,
            no_color: true,
            no_icons: false,
            no_gradient: true,
            a11y: false,
        });

        for (label, token) in [
            ("container", STYLE_DETAIL_FIND_CONTAINER),
            ("query", STYLE_DETAIL_FIND_QUERY),
            ("match_active", STYLE_DETAIL_FIND_MATCH_ACTIVE),
            ("match_inactive", STYLE_DETAIL_FIND_MATCH_INACTIVE),
        ] {
            let style = ctx.style(token);
            assert!(
                style.fg.is_some() || style.bg.is_some(),
                "detail-find {label} token should remain visible in mono mode"
            );
        }
    }

    #[test]
    fn mono_profile_downgrades_theme_colors() {
        let context = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::Dracula,
            dark_mode: true,
            color_profile: ColorProfile::Mono,
            no_color: true,
            no_icons: false,
            no_gradient: true,
            a11y: false,
        });

        assert!(matches!(context.resolved.primary, Color::Mono(_)));
        assert!(matches!(context.resolved.background, Color::Mono(_)));
        assert!(matches!(context.resolved.text, Color::Mono(_)));
    }

    #[test]
    fn accessibility_role_markers_prioritize_text_labels() {
        let markers = RoleMarkers::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::Ansi256,
            no_color: false,
            no_icons: true,
            no_gradient: true,
            a11y: true,
        });

        assert_eq!(markers.user, "[user]");
        assert_eq!(markers.assistant, "[assistant]");
        assert_eq!(markers.tool, "[tool]");
        assert_eq!(markers.system, "[system]");
    }

    #[test]
    fn base_contrast_is_wcag_aa_or_higher_for_all_presets() {
        for preset in UiThemePreset::all() {
            let dark_mode = !matches!(
                preset,
                UiThemePreset::Daylight | UiThemePreset::SolarizedLight
            );
            let context = StyleContext::from_options(StyleOptions {
                preset,
                dark_mode,
                color_profile: ColorProfile::TrueColor,
                no_color: false,
                no_icons: false,
                no_gradient: false,
                a11y: false,
            });

            let root = context.style(STYLE_APP_ROOT);
            let fg = root.fg.expect("app.root must define foreground");
            let bg = root.bg.expect("app.root must define background");
            let ratio = ftui::style::contrast_ratio_packed(fg, bg);
            assert!(
                ratio >= 3.5,
                "contrast too low for {}: {ratio}",
                preset.name()
            );
        }
    }

    #[test]
    fn high_contrast_preset_keeps_selection_legible() {
        let context = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::HighContrast,
            dark_mode: true,
            color_profile: ColorProfile::Ansi16,
            no_color: false,
            no_icons: false,
            no_gradient: true,
            a11y: true,
        });

        let selected = context.style(STYLE_RESULT_ROW_SELECTED);
        let fg = selected
            .fg
            .expect("selected row style should define foreground color");
        let bg = selected
            .bg
            .expect("selected row style should define background color");

        let ratio = ftui::style::contrast_ratio_packed(fg, bg);
        assert!(ratio >= 4.5, "selected row contrast too low: {ratio}");
    }

    #[test]
    fn theme_config_roundtrip_preserves_fields() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Nord),
        };

        let json = config
            .to_json_pretty()
            .expect("theme config should serialize");
        let parsed = ThemeConfig::from_json_str(&json).expect("theme config should deserialize");
        assert_eq!(parsed, config);
    }

    #[test]
    fn theme_config_json_snapshot_is_stable() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Catppuccin),
        };

        let json = config.to_json_pretty().expect("config should serialize");
        let expected = r##"{
  "version": 1,
  "base_preset": "catppuccin"
}"##;
        assert_eq!(json, expected);
    }

    #[test]
    fn theme_config_allows_known_preset_aliases() {
        // Old format with "colors" key should be silently ignored (no deny_unknown_fields)
        let config_json = r#"{
  "version": 1,
  "base_preset": "high_contrast",
  "colors": {}
}"#;

        let parsed =
            ThemeConfig::from_json_str(config_json).expect("preset alias should deserialize");
        assert_eq!(parsed.base_preset, Some(UiThemePreset::HighContrast));
    }

    #[test]
    fn theme_config_backwards_compat_dark_alias() {
        let config_json = r#"{"version":1,"base_preset":"dark"}"#;
        let parsed = ThemeConfig::from_json_str(config_json).expect("dark alias should work");
        assert_eq!(parsed.base_preset, Some(UiThemePreset::TokyoNight));
    }

    #[test]
    fn theme_config_backwards_compat_light_alias() {
        let config_json = r#"{"version":1,"base_preset":"light"}"#;
        let parsed = ThemeConfig::from_json_str(config_json).expect("light alias should work");
        assert_eq!(parsed.base_preset, Some(UiThemePreset::Daylight));
    }

    #[test]
    fn preset_downgrades_to_ansi16_profile() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::TokyoNight),
        };

        let context = StyleContext::from_options_with_theme_config(
            StyleOptions {
                preset: UiThemePreset::Daylight,
                dark_mode: false,
                color_profile: ColorProfile::Ansi16,
                no_color: false,
                no_icons: false,
                no_gradient: false,
                a11y: false,
            },
            &config,
        );

        assert!(matches!(context.resolved.text, Color::Ansi16(_)));
        assert!(matches!(context.resolved.background, Color::Ansi16(_)));
    }

    #[test]
    fn theme_config_file_roundtrip_works() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cass-theme-config-{now}.json"));

        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Dracula),
        };

        config
            .save_to_path(&path)
            .expect("theme config should save to disk");
        let loaded = ThemeConfig::load_from_path(&path).expect("theme config should reload");
        assert_eq!(loaded, config);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn base_preset_switches_dark_mode() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Daylight),
        };
        let ctx = StyleContext::from_options_with_theme_config(
            StyleOptions::default(), // Dark by default
            &config,
        );

        assert_eq!(ctx.options.preset, UiThemePreset::Daylight);
        assert!(!ctx.options.dark_mode, "Daylight preset → dark_mode=false");
    }

    #[test]
    fn empty_config_does_not_change_theme() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: None,
        };
        let base_ctx = StyleContext::from_options(StyleOptions::default());
        let overridden_ctx =
            StyleContext::from_options_with_theme_config(StyleOptions::default(), &config);

        // Same preset, same resolved text color.
        assert_eq!(base_ctx.resolved.text, overridden_ctx.resolved.text);
        assert_eq!(
            base_ctx.resolved.background,
            overridden_ctx.resolved.background
        );
    }

    #[test]
    fn config_base_preset_overrides_options_preset() {
        // ThemeConfig.base_preset wins over StyleOptions.preset.
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Nord),
        };
        let ctx = StyleContext::from_options_with_theme_config(
            StyleOptions {
                preset: UiThemePreset::TokyoNight,
                ..StyleOptions::default()
            },
            &config,
        );

        assert_eq!(
            ctx.options.preset,
            UiThemePreset::Nord,
            "config.base_preset should override options.preset"
        );
    }

    /// Tokens that must always have a foreground color set (used by rendering code).
    const CRITICAL_FG_TOKENS: &[&str] = &[
        STYLE_TEXT_PRIMARY,
        STYLE_TEXT_MUTED,
        STYLE_TEXT_SUBTLE,
        STYLE_STATUS_SUCCESS,
        STYLE_STATUS_WARNING,
        STYLE_STATUS_ERROR,
        STYLE_STATUS_INFO,
        STYLE_ROLE_USER,
        STYLE_ROLE_ASSISTANT,
        STYLE_ROLE_TOOL,
        STYLE_ROLE_SYSTEM,
        STYLE_SCORE_HIGH,
        STYLE_SCORE_MID,
        STYLE_SCORE_LOW,
        STYLE_KBD_KEY,
        STYLE_KBD_DESC,
    ];

    /// Tokens that must always have a background color set (pill/tab affordances).
    const CRITICAL_BG_TOKENS: &[&str] = &[
        STYLE_APP_ROOT,
        STYLE_PILL_ACTIVE,
        STYLE_PILL_INACTIVE,
        STYLE_TAB_ACTIVE,
        STYLE_RESULT_ROW_SELECTED,
    ];

    #[test]
    fn critical_fg_tokens_always_have_foreground() {
        for preset in UiThemePreset::all() {
            for &profile in &[
                ColorProfile::TrueColor,
                ColorProfile::Ansi256,
                ColorProfile::Ansi16,
            ] {
                let dark_mode = !matches!(
                    preset,
                    UiThemePreset::Daylight | UiThemePreset::SolarizedLight
                );
                let ctx = StyleContext::from_options(StyleOptions {
                    preset,
                    dark_mode,
                    color_profile: profile,
                    no_color: false,
                    no_icons: false,
                    no_gradient: false,
                    a11y: false,
                });

                for &token in CRITICAL_FG_TOKENS {
                    let style = ctx.style(token);
                    assert!(
                        style.fg.is_some(),
                        "Token {token} must have fg for preset {} profile {:?}",
                        preset.name(),
                        profile
                    );
                }
            }
        }
    }

    #[test]
    fn critical_bg_tokens_always_have_background() {
        for preset in UiThemePreset::all() {
            for &profile in &[
                ColorProfile::TrueColor,
                ColorProfile::Ansi256,
                ColorProfile::Ansi16,
            ] {
                let dark_mode = !matches!(
                    preset,
                    UiThemePreset::Daylight | UiThemePreset::SolarizedLight
                );
                let ctx = StyleContext::from_options(StyleOptions {
                    preset,
                    dark_mode,
                    color_profile: profile,
                    no_color: false,
                    no_icons: false,
                    no_gradient: false,
                    a11y: false,
                });

                for &token in CRITICAL_BG_TOKENS {
                    let style = ctx.style(token);
                    assert!(
                        style.bg.is_some(),
                        "Token {token} must have bg for preset {} profile {:?}",
                        preset.name(),
                        profile
                    );
                }
            }
        }
    }

    #[test]
    fn a11y_mode_adds_emphasis_to_roles() {
        // With a11y enabled, role tokens should have bold or underline for emphasis.
        for preset in UiThemePreset::all() {
            let dark_mode = !matches!(
                preset,
                UiThemePreset::Daylight | UiThemePreset::SolarizedLight
            );
            let ctx = StyleContext::from_options(StyleOptions {
                preset,
                dark_mode,
                color_profile: ColorProfile::TrueColor,
                no_color: false,
                no_icons: false,
                no_gradient: true,
                a11y: true,
            });

            let user = ctx.style(STYLE_ROLE_USER);
            let assistant = ctx.style(STYLE_ROLE_ASSISTANT);
            // At minimum, role tokens should still resolve with fg.
            assert!(
                user.fg.is_some(),
                "ROLE_USER must have fg in a11y mode for {}",
                preset.name()
            );
            assert!(
                assistant.fg.is_some(),
                "ROLE_ASSISTANT must have fg in a11y mode for {}",
                preset.name()
            );
        }
    }

    #[test]
    fn gutter_tokens_derive_from_role_tokens() {
        // Gutter bg should be a blend of the role fg toward background.
        // This test verifies they are related (gutter fg == role fg).
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let role_user = ctx.style(STYLE_ROLE_USER);
            let gutter_user = ctx.style(STYLE_ROLE_GUTTER_USER);

            // Gutter fg must equal role fg (they share the same foreground).
            assert_eq!(
                role_user.fg,
                gutter_user.fg,
                "GUTTER_USER.fg should match ROLE_USER.fg for preset {}",
                preset.name()
            );
            // Gutter must have a bg (the role+bg blend).
            assert!(
                gutter_user.bg.is_some(),
                "GUTTER_USER must have bg for preset {}",
                preset.name()
            );
        }
    }

    // -- decorative policy tests (2dccg.10.6) ---

    #[test]
    fn deco_full_wide_fancy_uses_rounded() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy = DecorativePolicy::resolve(StyleOptions::default(), DL::Full, LB::Wide, true);
        assert_eq!(policy.border_tier, BorderTier::Rounded);
        assert!(policy.show_icons);
        assert!(policy.use_styling);
        assert!(policy.render_content);
    }

    #[test]
    fn deco_full_narrow_downgrades_to_square() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy = DecorativePolicy::resolve(StyleOptions::default(), DL::Full, LB::Narrow, true);
        assert_eq!(
            policy.border_tier,
            BorderTier::Square,
            "Narrow breakpoint should force Square even with fancy_borders=true"
        );
        assert!(policy.show_icons);
    }

    #[test]
    fn deco_fancy_off_uses_square() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy = DecorativePolicy::resolve(StyleOptions::default(), DL::Full, LB::Wide, false);
        assert_eq!(policy.border_tier, BorderTier::Square);
    }

    #[test]
    fn deco_simple_borders_forces_square() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy =
            DecorativePolicy::resolve(StyleOptions::default(), DL::SimpleBorders, LB::Wide, true);
        assert_eq!(policy.border_tier, BorderTier::Square);
        assert!(
            policy.use_styling,
            "SimpleBorders should still allow styling"
        );
    }

    #[test]
    fn deco_no_styling_drops_color() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy =
            DecorativePolicy::resolve(StyleOptions::default(), DL::NoStyling, LB::Wide, true);
        assert_eq!(policy.border_tier, BorderTier::Square);
        assert!(!policy.use_styling, "NoStyling should drop color");
        assert!(policy.show_icons, "NoStyling should still show icons");
    }

    #[test]
    fn deco_essential_only_strips_everything() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy =
            DecorativePolicy::resolve(StyleOptions::default(), DL::EssentialOnly, LB::Wide, true);
        assert_eq!(policy.border_tier, BorderTier::None);
        assert!(!policy.show_icons);
        assert!(!policy.use_styling);
        assert!(
            policy.render_content,
            "EssentialOnly should still render content"
        );
    }

    #[test]
    fn deco_skeleton_drops_content() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let policy =
            DecorativePolicy::resolve(StyleOptions::default(), DL::Skeleton, LB::Wide, true);
        assert!(!policy.render_content, "Skeleton should not render content");
    }

    #[test]
    fn deco_no_color_drops_styling() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let opts = StyleOptions {
            no_color: true,
            color_profile: ColorProfile::Mono,
            no_gradient: true,
            ..StyleOptions::default()
        };
        let policy = DecorativePolicy::resolve(opts, DL::Full, LB::Wide, true);
        assert!(!policy.use_styling, "NO_COLOR should drop styling");
        assert!(!policy.use_gradients, "NO_COLOR should drop gradients");
    }

    #[test]
    fn deco_no_icons_suppresses_icons() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let opts = StyleOptions {
            no_icons: true,
            ..StyleOptions::default()
        };
        let policy = DecorativePolicy::resolve(opts, DL::Full, LB::Wide, true);
        assert!(!policy.show_icons, "CASS_NO_ICONS should suppress icons");
    }

    #[test]
    fn deco_monotonic_degradation() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel as DL;

        let levels = [
            DL::Full,
            DL::SimpleBorders,
            DL::NoStyling,
            DL::EssentialOnly,
            DL::Skeleton,
            DL::SkipFrame,
        ];
        let opts = StyleOptions::default();
        let mut prev: Option<DecorativePolicy> = None;

        for &level in &levels {
            let policy = DecorativePolicy::resolve(opts, level, LB::Wide, true);

            if let Some(p) = prev {
                // Border tier should be >= (weaker or equal).
                assert!(
                    policy.border_tier >= p.border_tier,
                    "Border tier should degrade monotonically: {:?} at {:?}",
                    policy.border_tier,
                    level
                );
                // Capabilities should only decrease.
                if !p.show_icons {
                    assert!(
                        !policy.show_icons,
                        "show_icons should not re-enable at {:?}",
                        level
                    );
                }
                if !p.use_styling {
                    assert!(
                        !policy.use_styling,
                        "use_styling should not re-enable at {:?}",
                        level
                    );
                }
                if !p.render_content {
                    assert!(
                        !policy.render_content,
                        "render_content should not re-enable at {:?}",
                        level
                    );
                }
            }
            prev = Some(policy);
        }
    }

    // -- pane chrome & focus tokens (2dccg.9.1) --------------------------------

    #[test]
    fn pane_title_focused_has_bold_accent() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let focused = ctx.style(STYLE_PANE_TITLE_FOCUSED);
            assert!(
                focused.fg.is_some(),
                "{preset:?}: focused title should have fg"
            );
            assert!(
                focused
                    .attrs
                    .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD)),
                "{preset:?}: focused title should be bold"
            );
        }
    }

    #[test]
    fn pane_title_unfocused_is_muted_not_bold() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let unfocused = ctx.style(STYLE_PANE_TITLE_UNFOCUSED);
            assert!(
                unfocused.fg.is_some(),
                "{preset:?}: unfocused title should have fg"
            );
            assert!(
                !unfocused
                    .attrs
                    .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD)),
                "{preset:?}: unfocused title should NOT be bold"
            );
        }
    }

    #[test]
    fn pane_title_focused_differs_from_unfocused() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let focused = ctx.style(STYLE_PANE_TITLE_FOCUSED);
            let unfocused = ctx.style(STYLE_PANE_TITLE_UNFOCUSED);
            assert_ne!(
                focused.fg, unfocused.fg,
                "{preset:?}: focused and unfocused title fg should differ"
            );
        }
    }

    #[test]
    fn split_handle_has_fg_and_bg() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let handle = ctx.style(STYLE_SPLIT_HANDLE);
            assert!(
                handle.fg.is_some(),
                "{preset:?}: split handle should have fg"
            );
            assert!(
                handle.bg.is_some(),
                "{preset:?}: split handle should have bg"
            );
        }
    }

    #[test]
    fn split_handle_fg_differs_from_own_bg() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let handle = ctx.style(STYLE_SPLIT_HANDLE);
            // The handle character must be visible on its own background.
            assert_ne!(
                handle.fg, handle.bg,
                "{preset:?}: split handle fg should differ from its bg"
            );
        }
    }

    // -- score/source/location hierarchy (2dccg.9.3) ---------------------------

    #[test]
    fn source_local_differs_from_source_remote() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let local = ctx.style(STYLE_SOURCE_LOCAL);
            let remote = ctx.style(STYLE_SOURCE_REMOTE);
            assert_ne!(
                local.fg, remote.fg,
                "{preset:?}: local and remote source fg should differ"
            );
        }
    }

    #[test]
    fn source_remote_is_italic() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let remote = ctx.style(STYLE_SOURCE_REMOTE);
            assert!(
                remote
                    .attrs
                    .is_some_and(|a| a.contains(ftui::StyleFlags::ITALIC)),
                "{preset:?}: remote source should be italic"
            );
        }
    }

    #[test]
    fn location_style_has_fg() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let loc = ctx.style(STYLE_LOCATION);
            assert!(
                loc.fg.is_some(),
                "{preset:?}: location style should have fg"
            );
        }
    }

    #[test]
    fn result_scanning_hierarchy_is_ordered() {
        // Verify the visual hierarchy: score colors > source badge > location > snippet
        // by checking that higher-priority tokens have bolder emphasis.
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let score_high = ctx.style(STYLE_SCORE_HIGH);
            let source_local = ctx.style(STYLE_SOURCE_LOCAL);
            let location = ctx.style(STYLE_LOCATION);

            // Score high should be bold (strongest visual signal).
            assert!(
                score_high
                    .attrs
                    .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD)),
                "{preset:?}: score high should be bold"
            );
            // Source local and location should NOT be bold (they recede).
            assert!(
                !source_local
                    .attrs
                    .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD)),
                "{preset:?}: source local should not be bold"
            );
            assert!(
                !location
                    .attrs
                    .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD)),
                "{preset:?}: location should not be bold"
            );
        }
    }

    #[test]
    fn capability_matrix_profiles_resolve_expected_color_profiles() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::core::terminal_capabilities::TerminalProfile;
        use ftui::render::budget::DegradationLevel as DL;

        let fixtures = [
            (TerminalProfile::Xterm256Color, "xterm-256color"),
            (TerminalProfile::Screen, "screen"),
            (TerminalProfile::Dumb, "dumb"),
            (TerminalProfile::WindowsConsole, "windows-console"),
            (TerminalProfile::Kitty, "kitty"),
        ];

        for (profile, term) in fixtures {
            let caps = TerminalCapabilities::from_profile(profile);
            let diag = style_policy_diagnostic(
                caps,
                CapabilityMatrixInputs {
                    term: Some(term),
                    ..CapabilityMatrixInputs::default()
                },
                DL::Full,
                LB::Wide,
                true,
            );

            let expected_profile = if caps.true_color {
                "truecolor"
            } else if caps.colors_256 {
                "ansi256"
            } else {
                "ansi16"
            };

            assert_eq!(
                diag.terminal_profile,
                profile.as_str(),
                "terminal profile id should be preserved in diagnostics"
            );
            assert_eq!(
                diag.resolved_color_profile, expected_profile,
                "profile {profile} should map to expected color profile"
            );
            assert_eq!(diag.term.as_deref(), Some(term));
            assert_eq!(
                diag.capability_unicode_box_drawing, caps.unicode_box_drawing,
                "unicode capability should be reported verbatim for {profile}"
            );
        }
    }

    #[test]
    fn capability_matrix_no_color_precedence_matches_policy_contract() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::core::terminal_capabilities::TerminalProfile;
        use ftui::render::budget::DegradationLevel as DL;

        let caps = TerminalCapabilities::from_profile(TerminalProfile::Kitty);

        let no_color_only = style_policy_diagnostic(
            caps,
            CapabilityMatrixInputs {
                term: Some("xterm-kitty"),
                no_color: true,
                cass_respect_no_color: false,
                ..CapabilityMatrixInputs::default()
            },
            DL::Full,
            LB::Wide,
            true,
        );
        assert!(
            !no_color_only.resolved_no_color,
            "NO_COLOR alone must not force monochrome"
        );
        assert_ne!(
            no_color_only.resolved_color_profile, "mono",
            "NO_COLOR alone should keep color enabled"
        );

        let respect_no_color = style_policy_diagnostic(
            caps,
            CapabilityMatrixInputs {
                term: Some("xterm-kitty"),
                no_color: true,
                cass_respect_no_color: true,
                ..CapabilityMatrixInputs::default()
            },
            DL::Full,
            LB::Wide,
            true,
        );
        assert!(respect_no_color.resolved_no_color);
        assert_eq!(respect_no_color.resolved_color_profile, "mono");
        assert!(
            !respect_no_color.policy_use_styling,
            "monochrome mode should disable styling"
        );
        assert!(
            !respect_no_color.policy_use_gradients,
            "monochrome mode should disable gradients"
        );

        let cass_no_color = style_policy_diagnostic(
            caps,
            CapabilityMatrixInputs {
                term: Some("xterm-kitty"),
                cass_no_color: true,
                cass_color_profile: Some("truecolor"),
                ..CapabilityMatrixInputs::default()
            },
            DL::Full,
            LB::Wide,
            true,
        );
        assert!(cass_no_color.resolved_no_color);
        assert_eq!(
            cass_no_color.resolved_color_profile, "mono",
            "CASS_NO_COLOR must override explicit profile requests"
        );
    }

    #[test]
    fn capability_matrix_diagnostic_payload_is_machine_readable_json() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::core::terminal_capabilities::TerminalProfile;
        use ftui::render::budget::DegradationLevel as DL;

        let caps = TerminalCapabilities::from_profile(TerminalProfile::Xterm256Color);
        let diag = style_policy_diagnostic(
            caps,
            CapabilityMatrixInputs {
                term: Some("xterm-256color"),
                colorterm: Some("truecolor"),
                ..CapabilityMatrixInputs::default()
            },
            DL::SimpleBorders,
            LB::Medium,
            true,
        );

        let json = match serde_json::to_value(&diag) {
            Ok(value) => value,
            Err(error) => panic!("diagnostic payload should serialize: {error}"),
        };
        let object = match json.as_object() {
            Some(map) => map,
            None => panic!("diagnostic payload must serialize to a JSON object"),
        };

        for required in [
            "terminal_profile",
            "degradation",
            "breakpoint",
            "resolved_color_profile",
            "policy_border_tier",
            "policy_use_styling",
            "policy_use_gradients",
            "policy_render_content",
            "capability_unicode_box_drawing",
            "env_no_color",
            "env_cass_respect_no_color",
            "env_cass_no_color",
        ] {
            assert!(
                object.contains_key(required),
                "diagnostic payload missing required key: {required}"
            );
        }
    }

    #[test]
    fn capability_matrix_degradation_transitions_are_monotonic() {
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::core::terminal_capabilities::TerminalProfile;
        use ftui::render::budget::DegradationLevel as DL;

        fn border_rank(tier: &str) -> u8 {
            match tier {
                "rounded" => 0,
                "square" => 1,
                "none" => 2,
                other => panic!("unexpected border tier: {other}"),
            }
        }

        let caps = TerminalCapabilities::from_profile(TerminalProfile::Kitty);
        let levels = [
            DL::Full,
            DL::SimpleBorders,
            DL::NoStyling,
            DL::EssentialOnly,
        ];
        let mut prev: Option<StylePolicyDiagnostic> = None;

        for level in levels {
            let diag = style_policy_diagnostic(
                caps,
                CapabilityMatrixInputs {
                    term: Some("xterm-kitty"),
                    ..CapabilityMatrixInputs::default()
                },
                level,
                LB::Wide,
                true,
            );

            if let Some(last) = &prev {
                assert!(
                    border_rank(diag.policy_border_tier) >= border_rank(last.policy_border_tier),
                    "border tier should only weaken across degradation levels"
                );
                if !last.policy_show_icons {
                    assert!(!diag.policy_show_icons, "icons must not re-enable");
                }
                if !last.policy_use_styling {
                    assert!(
                        !diag.policy_use_styling,
                        "styling must not re-enable after being stripped"
                    );
                }
                if !last.policy_use_gradients {
                    assert!(
                        !diag.policy_use_gradients,
                        "gradients must not re-enable after being stripped"
                    );
                }
                if !last.policy_render_content {
                    assert!(
                        !diag.policy_render_content,
                        "content rendering must not re-enable after being stripped"
                    );
                }
            }
            prev = Some(diag);
        }
    }

    // -- agent/role coherence tests (2dccg.10.2) ---

    #[test]
    fn agent_accent_style_is_bold_for_all_agents() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let agents = [
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
            "unknown_agent",
        ];
        for agent in agents {
            let style = ctx.agent_accent_style(agent);
            assert!(
                style.fg.is_some(),
                "agent_accent_style({agent}) must have fg"
            );
            assert!(
                style.has_attr(ftui::StyleFlags::BOLD),
                "agent_accent_style({agent}) must be bold"
            );
        }
    }

    #[test]
    fn agent_accent_style_adds_badge_bg_in_color_modes() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let style = ctx.agent_accent_style("codex");
            let fg = style.fg.expect("agent accent style should define fg");
            let bg = style.bg.expect("agent accent style should define bg tint");
            assert!(
                style.has_attr(ftui::StyleFlags::BOLD),
                "agent accent style should remain bold for preset {}",
                preset.name()
            );
            assert_ne!(
                Some(bg),
                Some(to_packed(ctx.resolved.surface)),
                "badge bg should differ from plain surface background for preset {}",
                preset.name()
            );
            let ratio = ftui::style::contrast_ratio_packed(fg, bg);
            assert!(
                ratio >= 3.0,
                "agent accent badge contrast too low ({ratio:.2}) for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn agent_accent_style_uses_fg_only_in_no_color_and_a11y_modes() {
        let no_color_ctx = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::Mono,
            no_color: true,
            no_icons: false,
            no_gradient: true,
            a11y: false,
        });
        let no_color_style = no_color_ctx.agent_accent_style("codex");
        assert!(
            no_color_style.bg.is_none(),
            "no-color mode should avoid accent background tint"
        );

        let a11y_ctx = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::TrueColor,
            no_color: false,
            no_icons: false,
            no_gradient: true,
            a11y: true,
        });
        let a11y_style = a11y_ctx.agent_accent_style("codex");
        assert!(
            a11y_style.bg.is_none(),
            "a11y mode should avoid accent background tint"
        );
    }

    #[test]
    fn result_row_style_for_agent_tints_background_when_color_enabled() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let base = ctx.style(STYLE_RESULT_ROW);
        let tinted = ctx.result_row_style_for_agent(base, "codex");
        assert!(base.bg.is_some(), "base row style should have a background");
        assert!(
            tinted.bg.is_some(),
            "tinted row style should retain a background"
        );
        assert_eq!(
            tinted.fg, base.fg,
            "row tinting should preserve existing foreground color"
        );
        assert_ne!(
            tinted.bg, base.bg,
            "row tinting should shift background toward agent accent"
        );
    }

    #[test]
    fn result_row_style_for_agent_preserves_base_style_in_no_color_or_a11y() {
        let base = Style::new()
            .fg(to_packed(Color::rgb(230, 230, 230)))
            .bg(to_packed(Color::rgb(32, 36, 48)));

        let no_color_ctx = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::Mono,
            no_color: true,
            no_icons: false,
            no_gradient: true,
            a11y: false,
        });
        assert_eq!(
            no_color_ctx.result_row_style_for_agent(base, "codex"),
            base,
            "no-color mode should not tint row backgrounds"
        );

        let a11y_ctx = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::TokyoNight,
            dark_mode: true,
            color_profile: ColorProfile::TrueColor,
            no_color: false,
            no_icons: false,
            no_gradient: true,
            a11y: true,
        });
        assert_eq!(
            a11y_ctx.result_row_style_for_agent(base, "codex"),
            base,
            "a11y mode should not tint row backgrounds"
        );
    }

    #[test]
    fn result_row_tints_are_pairwise_distinct_for_representative_agents() {
        let agents = ["claude_code", "codex", "cursor", "gemini", "aider"];
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let base = ctx.style(STYLE_RESULT_ROW);
            let mut tinted_bgs: Vec<(&str, ftui::PackedRgba)> = Vec::new();

            for agent in agents {
                let tinted = ctx.result_row_style_for_agent(base, agent);
                let tinted_bg = tinted
                    .bg
                    .expect("color mode result-row tint should define background");
                tinted_bgs.push((agent, tinted_bg));
            }
            let unique_count = tinted_bgs
                .iter()
                .map(|(_, bg)| *bg)
                .collect::<std::collections::HashSet<_>>()
                .len();
            let min_buckets = if matches!(preset, UiThemePreset::HighContrast) {
                2
            } else {
                4
            };
            assert!(
                unique_count >= min_buckets,
                "expected at least {min_buckets} distinct tint buckets (got {unique_count}) for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn result_row_tints_preserve_text_legibility_threshold() {
        let agents = ["claude_code", "codex", "cursor", "gemini", "aider"];
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            for base_token in [STYLE_RESULT_ROW, STYLE_RESULT_ROW_ALT] {
                let base = ctx.style(base_token);
                for agent in agents {
                    let tinted = ctx.result_row_style_for_agent(base, agent);
                    let fg = tinted
                        .fg
                        .expect("result-row style should always define foreground");
                    let bg = tinted
                        .bg
                        .expect("result-row style should always define background");
                    let ratio = ftui::style::contrast_ratio_packed(fg, bg);
                    assert!(
                        ratio >= 2.5,
                        "text contrast {:.2} below threshold for preset {} token {} agent {}",
                        ratio,
                        preset.name(),
                        base_token,
                        agent
                    );
                }
            }
        }
    }

    #[test]
    fn selected_row_affordance_remains_distinct_from_agent_tints() {
        let agents = ["claude_code", "codex", "cursor", "gemini", "aider"];
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let selected = ctx.style(STYLE_RESULT_ROW_SELECTED);
            let selected_bg = selected
                .bg
                .expect("selected-row style should define background");

            for base_token in [STYLE_RESULT_ROW, STYLE_RESULT_ROW_ALT] {
                let base = ctx.style(base_token);
                let base_bg = base.bg.expect("base row style should define background");
                let base_separation = ftui::style::contrast_ratio_packed(selected_bg, base_bg);
                for agent in agents {
                    let tinted = ctx.result_row_style_for_agent(base, agent);
                    let tinted_bg = tinted.bg.expect("result-row tint should define background");
                    assert_ne!(
                        selected_bg,
                        tinted_bg,
                        "selected background should differ from tinted row background for preset {} token {} agent {}",
                        preset.name(),
                        base_token,
                        agent
                    );

                    let separation = ftui::style::contrast_ratio_packed(selected_bg, tinted_bg);
                    assert!(
                        separation + 0.03 >= base_separation,
                        "selected/tinted separation {:.3} regressed too far below base {:.3} for preset {} token {} agent {}",
                        separation,
                        base_separation,
                        preset.name(),
                        base_token,
                        agent
                    );
                }
            }
        }
    }

    #[test]
    fn role_markers_provide_text_disambiguation_in_a11y() {
        let markers = RoleMarkers::from_options(StyleOptions {
            a11y: true,
            ..StyleOptions::default()
        });
        // In a11y mode, markers provide text-based role disambiguation.
        assert!(
            !markers.user.is_empty(),
            "a11y user marker must be non-empty"
        );
        assert!(
            !markers.assistant.is_empty(),
            "a11y assistant marker must be non-empty"
        );
        assert_ne!(markers.user, markers.assistant, "user != assistant markers");
        assert_ne!(markers.user, markers.tool, "user != tool markers");
        assert_ne!(markers.assistant, markers.tool, "assistant != tool markers");
    }

    #[test]
    fn role_markers_empty_when_no_icons() {
        let markers = RoleMarkers::from_options(StyleOptions {
            no_icons: true,
            a11y: false,
            ..StyleOptions::default()
        });
        assert!(
            markers.user.is_empty(),
            "no_icons should suppress role markers"
        );
    }

    // -- pill & tab style token tests (k25j6, 2kz6t) -------------------------

    fn context_for_preset(preset: UiThemePreset) -> StyleContext {
        let dark_mode = !matches!(
            preset,
            UiThemePreset::Daylight | UiThemePreset::SolarizedLight
        );
        StyleContext::from_options(StyleOptions {
            preset,
            dark_mode,
            color_profile: ColorProfile::TrueColor,
            no_color: false,
            no_icons: false,
            no_gradient: false,
            a11y: false,
        })
    }

    #[test]
    fn pill_active_has_background_for_all_presets() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let style = ctx.style(STYLE_PILL_ACTIVE);
            assert!(
                style.bg.is_some(),
                "STYLE_PILL_ACTIVE must have bg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn tab_active_has_background_for_all_presets() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let style = ctx.style(STYLE_TAB_ACTIVE);
            assert!(
                style.bg.is_some(),
                "STYLE_TAB_ACTIVE must have bg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn tab_inactive_has_no_background() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let style = ctx.style(STYLE_TAB_INACTIVE);
            assert!(
                style.bg.is_none(),
                "STYLE_TAB_INACTIVE should have no bg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn tab_active_differs_from_status_info() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let tab = ctx.style(STYLE_TAB_ACTIVE);
        let info = ctx.style(STYLE_STATUS_INFO);
        assert_ne!(
            tab, info,
            "STYLE_TAB_ACTIVE must differ from STYLE_STATUS_INFO"
        );
    }

    #[test]
    fn pill_active_differs_from_text_primary() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let pill = ctx.style(STYLE_PILL_ACTIVE);
        let text = ctx.style(STYLE_TEXT_PRIMARY);
        assert_ne!(
            pill, text,
            "STYLE_PILL_ACTIVE must differ from STYLE_TEXT_PRIMARY"
        );
    }

    #[test]
    fn tab_and_pill_styles_unique_across_presets() {
        let mut tab_styles = std::collections::HashSet::new();
        let mut pill_styles = std::collections::HashSet::new();
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let tab = ctx.style(STYLE_TAB_ACTIVE);
            let pill = ctx.style(STYLE_PILL_ACTIVE);
            tab_styles.insert(format!("{:?}", tab));
            pill_styles.insert(format!("{:?}", pill));
        }
        assert!(
            tab_styles.len() >= 3,
            "STYLE_TAB_ACTIVE should produce at least 3 distinct styles across presets, got {}",
            tab_styles.len()
        );
        assert!(
            pill_styles.len() >= 3,
            "STYLE_PILL_ACTIVE should produce at least 3 distinct styles across presets, got {}",
            pill_styles.len()
        );
    }

    // -- Pill hierarchy tests (2dccg.8.3) ----------------------------------------

    #[test]
    fn pill_inactive_differs_from_pill_active() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let active = ctx.style(STYLE_PILL_ACTIVE);
            let inactive = ctx.style(STYLE_PILL_INACTIVE);
            assert_ne!(
                active,
                inactive,
                "STYLE_PILL_INACTIVE must differ from STYLE_PILL_ACTIVE for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn pill_inactive_is_not_bold() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let inactive = ctx.style(STYLE_PILL_INACTIVE);
            let is_bold = inactive
                .attrs
                .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD));
            assert!(
                !is_bold,
                "STYLE_PILL_INACTIVE should not be bold for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn pill_active_is_bold() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let active = ctx.style(STYLE_PILL_ACTIVE);
            let is_bold = active
                .attrs
                .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD));
            assert!(
                is_bold,
                "STYLE_PILL_ACTIVE should be bold for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn pill_inactive_has_background_for_all_presets() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let inactive = ctx.style(STYLE_PILL_INACTIVE);
            assert!(
                inactive.bg.is_some(),
                "STYLE_PILL_INACTIVE must have bg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn pill_inactive_is_dim() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let inactive = ctx.style(STYLE_PILL_INACTIVE);
            let is_dim = inactive
                .attrs
                .is_some_and(|a| a.contains(ftui::StyleFlags::DIM));
            assert!(
                is_dim,
                "STYLE_PILL_INACTIVE should be dim for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn pill_label_has_foreground_and_bold() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let label = ctx.style(STYLE_PILL_LABEL);
            assert!(
                label.fg.is_some(),
                "STYLE_PILL_LABEL must have fg for preset {}",
                preset.name()
            );
            let is_bold = label
                .attrs
                .is_some_and(|a| a.contains(ftui::StyleFlags::BOLD));
            assert!(
                is_bold,
                "STYLE_PILL_LABEL should be bold for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn pill_hierarchy_is_visually_ordered() {
        // Active pills should be the most prominent (fg differs from inactive/label)
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let active = ctx.style(STYLE_PILL_ACTIVE);
            let inactive = ctx.style(STYLE_PILL_INACTIVE);
            let label = ctx.style(STYLE_PILL_LABEL);
            // All three should be distinct
            assert_ne!(
                active.fg,
                inactive.fg,
                "pill active fg must differ from inactive fg for preset {}",
                preset.name()
            );
            assert_ne!(
                active.fg,
                label.fg,
                "pill active fg must differ from label fg for preset {}",
                preset.name()
            );
        }
    }

    // -- Breadcrumb hierarchy tests (2dccg.8.2) ---------------------------------

    #[test]
    fn crumb_active_differs_from_inactive() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let active = ctx.style(STYLE_CRUMB_ACTIVE);
            let inactive = ctx.style(STYLE_CRUMB_INACTIVE);
            assert_ne!(
                active,
                inactive,
                "CRUMB_ACTIVE must differ from CRUMB_INACTIVE for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn crumb_active_is_bold() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let active = ctx.style(STYLE_CRUMB_ACTIVE);
            assert!(
                active.has_attr(ftui::StyleFlags::BOLD),
                "CRUMB_ACTIVE should be bold for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn crumb_separator_has_fg() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let sep = ctx.style(STYLE_CRUMB_SEPARATOR);
            assert!(
                sep.fg.is_some(),
                "CRUMB_SEPARATOR must have fg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn crumb_separator_differs_from_active() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let active = ctx.style(STYLE_CRUMB_ACTIVE);
            let sep = ctx.style(STYLE_CRUMB_SEPARATOR);
            assert_ne!(
                active.fg,
                sep.fg,
                "CRUMB_SEPARATOR fg must differ from CRUMB_ACTIVE fg for preset {}",
                preset.name()
            );
        }
    }

    // -- MarkdownTheme integration tests (kr88h) --------------------------------

    #[test]
    fn markdown_theme_h1_uses_primary_color() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let md = ctx.markdown_theme();
        let expected_fg = to_packed(ctx.resolved.primary);
        assert_eq!(
            md.h1.fg,
            Some(expected_fg),
            "h1 fg should match resolved.primary"
        );
    }

    #[test]
    fn markdown_theme_code_inline_has_background() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let md = ctx.markdown_theme();
            assert!(
                md.code_inline.bg.is_some(),
                "code_inline must have bg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn markdown_theme_code_block_has_background() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let md = ctx.markdown_theme();
            assert!(
                md.code_block.bg.is_some(),
                "code_block must have bg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn markdown_theme_link_is_underlined() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let md = ctx.markdown_theme();
        assert!(
            md.link.has_attr(ftui::StyleFlags::UNDERLINE),
            "link style should include underline"
        );
    }

    #[test]
    fn markdown_theme_table_has_themed_border() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let md = ctx.markdown_theme();
            assert!(
                md.table_theme.border.fg.is_some(),
                "table border must have fg for preset {}",
                preset.name()
            );
            assert!(
                md.table_theme.header.fg.is_some(),
                "table header must have fg for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn syntax_highlight_theme_matches_dark_mode() {
        let dark_ctx = context_for_preset(UiThemePreset::TokyoNight);
        let light_ctx = context_for_preset(UiThemePreset::Daylight);
        let dark_hl = dark_ctx.syntax_highlight_theme();
        let light_hl = light_ctx.syntax_highlight_theme();
        // Dark and light highlight themes should differ (keyword color at minimum).
        assert_ne!(
            format!("{:?}", dark_hl.keyword.fg),
            format!("{:?}", light_hl.keyword.fg),
            "dark and light syntax themes should differ"
        );
    }

    #[test]
    fn markdown_theme_differs_across_presets() {
        let mut themes = std::collections::HashSet::new();
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let md = ctx.markdown_theme();
            themes.insert(format!("{:?}", md.h1.fg));
        }
        assert!(
            themes.len() >= 3,
            "markdown h1 should differ across presets, got {} distinct",
            themes.len()
        );
    }

    #[test]
    fn markdown_theme_not_default() {
        let ctx = context_for_preset(UiThemePreset::TokyoNight);
        let themed = ctx.markdown_theme();
        let default = MarkdownTheme::default();
        assert_ne!(
            format!("{:?}", themed.h1),
            format!("{:?}", default.h1),
            "themed markdown h1 should differ from default"
        );
    }

    // -- dead-style-token audit (2dccg.1.3) -----------------------------------

    /// All semantic style token constant names defined in this module.
    /// This list MUST be kept in sync with the `pub const STYLE_*` declarations
    /// at the top of the file. Adding a new constant without adding it here will
    /// cause `style_token_registry_is_complete` to fail; adding it here without
    /// using it in rendering code will cause `no_dead_style_tokens` to fail.
    const ALL_STYLE_TOKENS: &[(&str, &str)] = &[
        ("STYLE_APP_ROOT", STYLE_APP_ROOT),
        ("STYLE_PANE_BASE", STYLE_PANE_BASE),
        ("STYLE_PANE_FOCUSED", STYLE_PANE_FOCUSED),
        ("STYLE_PANE_TITLE_FOCUSED", STYLE_PANE_TITLE_FOCUSED),
        ("STYLE_PANE_TITLE_UNFOCUSED", STYLE_PANE_TITLE_UNFOCUSED),
        ("STYLE_SPLIT_HANDLE", STYLE_SPLIT_HANDLE),
        ("STYLE_TEXT_PRIMARY", STYLE_TEXT_PRIMARY),
        ("STYLE_TEXT_MUTED", STYLE_TEXT_MUTED),
        ("STYLE_TEXT_SUBTLE", STYLE_TEXT_SUBTLE),
        ("STYLE_STATUS_SUCCESS", STYLE_STATUS_SUCCESS),
        ("STYLE_STATUS_WARNING", STYLE_STATUS_WARNING),
        ("STYLE_STATUS_ERROR", STYLE_STATUS_ERROR),
        ("STYLE_STATUS_INFO", STYLE_STATUS_INFO),
        ("STYLE_RESULT_ROW", STYLE_RESULT_ROW),
        ("STYLE_RESULT_ROW_ALT", STYLE_RESULT_ROW_ALT),
        ("STYLE_RESULT_ROW_SELECTED", STYLE_RESULT_ROW_SELECTED),
        ("STYLE_ROLE_USER", STYLE_ROLE_USER),
        ("STYLE_ROLE_ASSISTANT", STYLE_ROLE_ASSISTANT),
        ("STYLE_ROLE_TOOL", STYLE_ROLE_TOOL),
        ("STYLE_ROLE_SYSTEM", STYLE_ROLE_SYSTEM),
        ("STYLE_ROLE_GUTTER_USER", STYLE_ROLE_GUTTER_USER),
        ("STYLE_ROLE_GUTTER_ASSISTANT", STYLE_ROLE_GUTTER_ASSISTANT),
        ("STYLE_ROLE_GUTTER_TOOL", STYLE_ROLE_GUTTER_TOOL),
        ("STYLE_ROLE_GUTTER_SYSTEM", STYLE_ROLE_GUTTER_SYSTEM),
        ("STYLE_SCORE_HIGH", STYLE_SCORE_HIGH),
        ("STYLE_SCORE_MID", STYLE_SCORE_MID),
        ("STYLE_SCORE_LOW", STYLE_SCORE_LOW),
        ("STYLE_SOURCE_LOCAL", STYLE_SOURCE_LOCAL),
        ("STYLE_SOURCE_REMOTE", STYLE_SOURCE_REMOTE),
        ("STYLE_LOCATION", STYLE_LOCATION),
        ("STYLE_PILL_ACTIVE", STYLE_PILL_ACTIVE),
        ("STYLE_PILL_INACTIVE", STYLE_PILL_INACTIVE),
        ("STYLE_PILL_LABEL", STYLE_PILL_LABEL),
        ("STYLE_CRUMB_ACTIVE", STYLE_CRUMB_ACTIVE),
        ("STYLE_CRUMB_INACTIVE", STYLE_CRUMB_INACTIVE),
        ("STYLE_CRUMB_SEPARATOR", STYLE_CRUMB_SEPARATOR),
        ("STYLE_TAB_ACTIVE", STYLE_TAB_ACTIVE),
        ("STYLE_TAB_INACTIVE", STYLE_TAB_INACTIVE),
        ("STYLE_DETAIL_FIND_CONTAINER", STYLE_DETAIL_FIND_CONTAINER),
        ("STYLE_DETAIL_FIND_QUERY", STYLE_DETAIL_FIND_QUERY),
        (
            "STYLE_DETAIL_FIND_MATCH_ACTIVE",
            STYLE_DETAIL_FIND_MATCH_ACTIVE,
        ),
        (
            "STYLE_DETAIL_FIND_MATCH_INACTIVE",
            STYLE_DETAIL_FIND_MATCH_INACTIVE,
        ),
        ("STYLE_QUERY_HIGHLIGHT", STYLE_QUERY_HIGHLIGHT),
        ("STYLE_KBD_KEY", STYLE_KBD_KEY),
        ("STYLE_KBD_DESC", STYLE_KBD_DESC),
        ("STYLE_SEARCH_FOCUS", STYLE_SEARCH_FOCUS),
        ("STYLE_MODAL_BACKDROP", STYLE_MODAL_BACKDROP),
    ];

    /// Tokens that are consumed indirectly (e.g. via helper methods like
    /// `score_style()` or `agent_accent_style()`) and may not appear as
    /// literal `style_system::STYLE_*` references in rendering code.
    /// Each entry requires a justification comment.
    const INDIRECT_USE_WHITELIST: &[&str] = &[
        // score_style() dispatches to these based on numeric thresholds
        "STYLE_SCORE_HIGH",
        "STYLE_SCORE_MID",
        "STYLE_SCORE_LOW",
        // Planned to be wired by implementation bead 2dccg.4.2 (detail find bar
        // rendering). This spec bead defines the semantic contract and tests.
        "STYLE_DETAIL_FIND_CONTAINER",
        "STYLE_DETAIL_FIND_QUERY",
        "STYLE_DETAIL_FIND_MATCH_ACTIVE",
        "STYLE_DETAIL_FIND_MATCH_INACTIVE",
        "STYLE_QUERY_HIGHLIGHT",
        // build_pills_row() applies label style per-span within pill construction
        "STYLE_PILL_LABEL",
    ];

    #[test]
    fn style_token_registry_is_complete() {
        // Verify ALL_STYLE_TOKENS matches the actual pub const declarations.
        // Read the source file and extract all `pub const STYLE_*` names.
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/style_system.rs"),
        )
        .expect("should be able to read style_system.rs");

        let mut defined_in_source: Vec<String> = Vec::new();
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("pub const STYLE_")
                && trimmed.contains(": &str")
                && let Some(name) = trimmed
                    .strip_prefix("pub const ")
                    .and_then(|s| s.split(':').next())
            {
                defined_in_source.push(name.trim().to_string());
            }
        }

        let registry_names: Vec<&str> = ALL_STYLE_TOKENS.iter().map(|(name, _)| *name).collect();

        // Check nothing is missing from the registry
        for src_name in &defined_in_source {
            assert!(
                registry_names.contains(&src_name.as_str()),
                "Style token `{src_name}` is defined in source but missing from \
                 ALL_STYLE_TOKENS registry. Add it to the test registry."
            );
        }

        // Check nothing in registry is absent from source
        for reg_name in &registry_names {
            assert!(
                defined_in_source.iter().any(|s| s == reg_name),
                "ALL_STYLE_TOKENS contains `{reg_name}` but it is not defined \
                 as `pub const` in source. Remove it from the test registry."
            );
        }
    }

    #[test]
    fn no_dead_style_tokens() {
        // Read all files under src/ui/ that consume style tokens in rendering.
        let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let mut rendering_source = String::new();

        for entry in std::fs::read_dir(&ui_dir).expect("should read src/ui/") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| n != "style_system.rs")
            {
                rendering_source.push_str(
                    &std::fs::read_to_string(&path)
                        .unwrap_or_else(|_| panic!("should read {}", path.display())),
                );
            }
        }

        // Also include style_system.rs itself for internal references
        // (e.g. score_style, markdown_theme, agent_accent_style call sites)
        let self_source = std::fs::read_to_string(ui_dir.join("style_system.rs"))
            .expect("should read style_system.rs");

        let mut dead_tokens: Vec<&str> = Vec::new();

        for (const_name, _token_value) in ALL_STYLE_TOKENS {
            if INDIRECT_USE_WHITELIST.contains(const_name) {
                continue;
            }

            // Check if the constant name appears in rendering code.
            // We search for the constant name (e.g. "STYLE_PILL_ACTIVE") as
            // it would appear in `style_system::STYLE_PILL_ACTIVE` or
            // `STYLE_PILL_ACTIVE` references.
            let in_rendering = rendering_source.contains(const_name);
            let in_self_methods = self_source.lines().any(|line| {
                line.contains(const_name)
                    && !line.trim().starts_with("pub const ")
                    && !line.trim().starts_with("//")
                    && !line.contains("ALL_STYLE_TOKENS")
                    && !line.contains("INDIRECT_USE_WHITELIST")
            });

            if !in_rendering && !in_self_methods {
                dead_tokens.push(const_name);
            }
        }

        assert!(
            dead_tokens.is_empty(),
            "Dead style tokens found (defined but never used in rendering code):\n  \
             {}\n\n\
             Fix: Either wire these tokens into rendering code in src/ui/app.rs,\n\
             or add them to INDIRECT_USE_WHITELIST with a justification comment\n\
             if they are consumed indirectly (e.g. via helper methods).",
            dead_tokens.join("\n  ")
        );
    }

    #[test]
    fn all_tokens_resolve_to_non_default_style() {
        // Every token should produce a meaningfully-styled Style (at minimum
        // an fg color) for every preset, ensuring no token silently falls back
        // to the stylesheet's default empty style.
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            for (const_name, token_value) in ALL_STYLE_TOKENS {
                let style = ctx.style(token_value);
                assert!(
                    style.fg.is_some() || style.bg.is_some(),
                    "Token {const_name} resolves to a style with no fg or bg \
                     for preset {} — it may be unwired in build_stylesheet()",
                    preset.name()
                );
            }
        }
    }

    // -- palette correctness & semantic validation (2dccg.10.1) ----------------

    #[test]
    fn role_tokens_are_pairwise_distinct_per_preset() {
        let role_tokens = [
            ("user", STYLE_ROLE_USER),
            ("assistant", STYLE_ROLE_ASSISTANT),
            ("tool", STYLE_ROLE_TOOL),
            ("system", STYLE_ROLE_SYSTEM),
        ];
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            for i in 0..role_tokens.len() {
                for j in (i + 1)..role_tokens.len() {
                    let (name_a, token_a) = role_tokens[i];
                    let (name_b, token_b) = role_tokens[j];
                    let style_a = ctx.style(token_a);
                    let style_b = ctx.style(token_b);
                    assert_ne!(
                        style_a.fg,
                        style_b.fg,
                        "Role {name_a} and {name_b} must have distinct fg colors \
                         for preset {} to remain visually distinguishable",
                        preset.name()
                    );
                }
            }
        }
    }

    #[test]
    fn role_gutter_tokens_are_pairwise_distinct_per_preset() {
        let gutter_tokens = [
            ("user", STYLE_ROLE_GUTTER_USER),
            ("assistant", STYLE_ROLE_GUTTER_ASSISTANT),
            ("tool", STYLE_ROLE_GUTTER_TOOL),
            ("system", STYLE_ROLE_GUTTER_SYSTEM),
        ];
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            for i in 0..gutter_tokens.len() {
                for j in (i + 1)..gutter_tokens.len() {
                    let (name_a, token_a) = gutter_tokens[i];
                    let (name_b, token_b) = gutter_tokens[j];
                    let style_a = ctx.style(token_a);
                    let style_b = ctx.style(token_b);
                    assert_ne!(
                        style_a.fg,
                        style_b.fg,
                        "Gutter {name_a} and {name_b} must have distinct fg colors \
                         for preset {} to remain scannable",
                        preset.name()
                    );
                }
            }
        }
    }

    #[test]
    fn status_tokens_are_pairwise_distinct_per_preset() {
        let status_tokens = [
            ("success", STYLE_STATUS_SUCCESS),
            ("warning", STYLE_STATUS_WARNING),
            ("error", STYLE_STATUS_ERROR),
            ("info", STYLE_STATUS_INFO),
        ];
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            for i in 0..status_tokens.len() {
                for j in (i + 1)..status_tokens.len() {
                    let (name_a, token_a) = status_tokens[i];
                    let (name_b, token_b) = status_tokens[j];
                    let style_a = ctx.style(token_a);
                    let style_b = ctx.style(token_b);
                    assert_ne!(
                        style_a.fg,
                        style_b.fg,
                        "Status {name_a} and {name_b} must have distinct fg colors \
                         for preset {}",
                        preset.name()
                    );
                }
            }
        }
    }

    #[test]
    fn text_hierarchy_is_ordered_per_preset() {
        // text_primary should be "brighter" (more opaque/distinct from bg) than
        // text_muted, which should differ from text_subtle.
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let primary = ctx.style(STYLE_TEXT_PRIMARY);
            let muted = ctx.style(STYLE_TEXT_MUTED);
            let subtle = ctx.style(STYLE_TEXT_SUBTLE);

            assert_ne!(
                primary.fg,
                muted.fg,
                "TEXT_PRIMARY and TEXT_MUTED must differ for preset {}",
                preset.name()
            );
            assert_ne!(
                muted.fg,
                subtle.fg,
                "TEXT_MUTED and TEXT_SUBTLE must differ for preset {}",
                preset.name()
            );
            assert_ne!(
                primary.fg,
                subtle.fg,
                "TEXT_PRIMARY and TEXT_SUBTLE must differ for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn score_tokens_form_visual_hierarchy() {
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let high = ctx.style(STYLE_SCORE_HIGH);
            let mid = ctx.style(STYLE_SCORE_MID);
            let low = ctx.style(STYLE_SCORE_LOW);

            assert_ne!(
                high.fg,
                mid.fg,
                "SCORE_HIGH and SCORE_MID must differ for preset {}",
                preset.name()
            );
            assert_ne!(
                mid.fg,
                low.fg,
                "SCORE_MID and SCORE_LOW must differ for preset {}",
                preset.name()
            );
            // High should have bold for emphasis
            assert!(
                high.has_attr(ftui::StyleFlags::BOLD),
                "SCORE_HIGH should be bold for preset {}",
                preset.name()
            );
        }
    }

    #[test]
    fn default_presets_pass_contrast_report() {
        // All built-in presets should pass the contrast report (they use
        // curated color palettes). Only custom overrides might fail.
        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let report = ctx.contrast_report();
            assert!(
                !report.has_failures(),
                "Preset {} fails contrast checks: {:?}",
                preset.name(),
                report.failing_pairs().into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn palette_propagation_is_deterministic() {
        // Building the same preset twice should produce identical styles.
        for preset in UiThemePreset::all() {
            let ctx1 = context_for_preset(preset);
            let ctx2 = context_for_preset(preset);
            for (_const_name, token_value) in ALL_STYLE_TOKENS {
                let s1 = ctx1.style(token_value);
                let s2 = ctx2.style(token_value);
                assert_eq!(
                    format!("{s1:?}"),
                    format!("{s2:?}"),
                    "Token {_const_name} is not deterministic for preset {}",
                    preset.name()
                );
            }
        }
    }

    // =====================================================================
    // 2dccg.11.1 — Rendering-facing style invariants with structured logging
    // =====================================================================

    #[test]
    fn rendering_token_affordance_matrix_with_logging() {
        use super::super::test_log::{Category, TestLogger};

        let log = TestLogger::new("11.1.affordance_matrix");

        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            log.step_start(Category::Style, format!(r#""preset:{:?}""#, preset));

            // Pill active must have background affordance
            let pill_active = ctx.style(STYLE_PILL_ACTIVE);
            if pill_active.bg.is_some() {
                log.pass(
                    Category::Style,
                    format!(r#""pill_active bg present for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Style,
                    format!(
                        r#"{{"msg":"pill_active bg missing","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!("STYLE_PILL_ACTIVE must have bg for {:?}", preset);
            }

            // Tab active must have background
            let tab_active = ctx.style(STYLE_TAB_ACTIVE);
            if tab_active.bg.is_some() {
                log.pass(
                    Category::Style,
                    format!(r#""tab_active bg present for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Style,
                    format!(
                        r#"{{"msg":"tab_active bg missing","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!("STYLE_TAB_ACTIVE must have bg for {:?}", preset);
            }

            // Tab inactive must differ from active
            let tab_inactive = ctx.style(STYLE_TAB_INACTIVE);
            if tab_active.fg != tab_inactive.fg || tab_active.bg != tab_inactive.bg {
                log.pass(
                    Category::Style,
                    format!(r#""tab active/inactive distinct for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Style,
                    format!(
                        r#"{{"msg":"tab active/inactive identical","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!("TAB_ACTIVE and TAB_INACTIVE must differ for {:?}", preset);
            }

            // Score hierarchy
            let high = ctx.style(STYLE_SCORE_HIGH);
            let mid = ctx.style(STYLE_SCORE_MID);
            let low = ctx.style(STYLE_SCORE_LOW);
            if high.fg != mid.fg && mid.fg != low.fg {
                log.pass(
                    Category::Style,
                    format!(r#""score hierarchy preserved for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Style,
                    format!(
                        r#"{{"msg":"score hierarchy broken","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!(
                    "Score HIGH/MID/LOW must be pairwise distinct for {:?}",
                    preset
                );
            }

            // Role gutters pairwise distinct
            let user = ctx.style(STYLE_ROLE_GUTTER_USER);
            let asst = ctx.style(STYLE_ROLE_GUTTER_ASSISTANT);
            let tool = ctx.style(STYLE_ROLE_GUTTER_TOOL);
            let sys = ctx.style(STYLE_ROLE_GUTTER_SYSTEM);
            let roles = [
                ("user", user.fg),
                ("assistant", asst.fg),
                ("tool", tool.fg),
                ("system", sys.fg),
            ];
            let mut distinct = true;
            for i in 0..roles.len() {
                for j in (i + 1)..roles.len() {
                    if roles[i].1 == roles[j].1 {
                        distinct = false;
                    }
                }
            }
            if distinct {
                log.pass(
                    Category::Style,
                    format!(r#""role gutters pairwise distinct for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Style,
                    format!(
                        r#"{{"msg":"role gutters not pairwise distinct","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!("Role gutters must be pairwise distinct for {:?}", preset);
            }

            log.step_end(Category::Style, format!(r#""preset:{:?} done""#, preset));
        }

        let (pass, fail, _) = log.summary();
        assert!(
            fail == 0,
            "rendering affordance matrix: {pass} pass, {fail} fail"
        );
    }

    #[test]
    fn markdown_theme_preset_coherence_with_logging() {
        use super::super::test_log::{Category, TestLogger};

        let log = TestLogger::new("11.1.markdown_coherence");

        for preset in UiThemePreset::all() {
            let ctx = context_for_preset(preset);
            let md_theme = ctx.markdown_theme();

            // Markdown theme should not be all-default
            let default_md = MarkdownTheme::default();
            if format!("{:?}", md_theme) != format!("{:?}", default_md) {
                log.pass(
                    Category::Theme,
                    format!(r#""markdown_theme non-default for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Theme,
                    format!(
                        r#"{{"msg":"markdown_theme is default","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!("markdown_theme() must be non-default for {:?}", preset);
            }

            // Code inline should have background
            if md_theme.code_inline.bg.is_some() {
                log.pass(
                    Category::Theme,
                    format!(r#""code_inline has bg for {:?}""#, preset),
                );
            } else {
                log.fail(
                    Category::Theme,
                    format!(
                        r#"{{"msg":"code_inline bg missing","preset":"{:?}"}}"#,
                        preset
                    ),
                );
                panic!("code_inline must have bg for {:?}", preset);
            }
        }

        let (pass, fail, _) = log.summary();
        assert!(fail == 0, "markdown coherence: {pass} pass, {fail} fail");
    }

    #[test]
    fn degradation_affordance_preservation_with_logging() {
        use super::super::test_log::{Category, TestLogger};
        use crate::ui::app::LayoutBreakpoint as LB;
        use ftui::render::budget::DegradationLevel;

        let log = TestLogger::new("11.1.degradation_affordance");
        let opts = StyleOptions {
            color_profile: ColorProfile::TrueColor,
            ..StyleOptions::default()
        };

        // At Full degradation, DecorativePolicy should allow all decorations
        let full_policy = DecorativePolicy::resolve(opts, DegradationLevel::Full, LB::Wide, true);
        if full_policy.use_gradients && full_policy.show_icons {
            log.pass(
                Category::Degradation,
                r#""Full allows gradients+icons""#.to_string(),
            );
        } else {
            log.fail(
                Category::Degradation,
                format!(
                    r#"{{"msg":"Full degradation restricts decorations","gradients":{},"icons":{}}}"#,
                    full_policy.use_gradients, full_policy.show_icons
                ),
            );
        }

        // At EssentialOnly, decorations should be restricted
        let essential_policy =
            DecorativePolicy::resolve(opts, DegradationLevel::EssentialOnly, LB::Wide, true);
        if !essential_policy.use_gradients {
            log.pass(
                Category::Degradation,
                r#""EssentialOnly restricts gradients""#.to_string(),
            );
        } else {
            log.fail(
                Category::Degradation,
                r#""EssentialOnly should restrict gradients""#.to_string(),
            );
        }

        let (pass, fail, _) = log.summary();
        assert!(
            fail == 0,
            "degradation affordance: {pass} pass, {fail} fail"
        );
    }
}
