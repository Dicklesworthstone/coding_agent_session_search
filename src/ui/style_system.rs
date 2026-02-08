//! FrankenTUI style-system scaffolding for cass.
//!
//! Centralizes:
//! - theme preset selection and [`ThemeColorOverrides`] (19 named color slots)
//! - color profile downgrade (mono / ansi16 / ansi256 / truecolor)
//! - env opt-outs (`NO_COLOR`, `CASS_NO_COLOR`, `CASS_NO_ICONS`, `CASS_NO_GRADIENT`)
//! - accessibility text markers (`CASS_A11Y`)
//! - semantic `StyleSheet` tokens consumed by all ftui views
//! - [`StyleContext`] facade for theme-aware style resolution in view code
//!
//! Widgets reference semantic token names (e.g. `STYLE_STATUS_SUCCESS`) rather
//! than raw colors, so preset changes and color profile downgrades propagate
//! automatically. The interactive theme editor (Ctrl+Shift+T in the TUI) writes
//! [`ThemeColorOverrides`] to `~/.config/cass/theme.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use ftui::render::cell::PackedRgba;
use ftui::style::theme::themes;
use ftui::{
    AdaptiveColor, Color, ColorProfile, ResolvedTheme, Style, StyleSheet, TerminalCapabilities,
    Theme, ThemeBuilder,
};
use serde::{Deserialize, Serialize};

pub const STYLE_APP_ROOT: &str = "app.root";
pub const STYLE_PANE_BASE: &str = "pane.base";
pub const STYLE_PANE_FOCUSED: &str = "pane.focused";
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
pub const STYLE_PILL_ACTIVE: &str = "pill.active";
pub const STYLE_TAB_ACTIVE: &str = "tab.active";
pub const STYLE_TAB_INACTIVE: &str = "tab.inactive";
pub const STYLE_KBD_KEY: &str = "kbd.key";
pub const STYLE_KBD_DESC: &str = "kbd.desc";
pub const THEME_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiThemePreset {
    #[default]
    Dark,
    Light,
    #[serde(alias = "high_contrast", alias = "highcontrast", alias = "hc")]
    HighContrast,
    #[serde(alias = "cat")]
    Catppuccin,
    Dracula,
    Nord,
}

impl UiThemePreset {
    pub const fn all() -> [Self; 6] {
        [
            Self::Dark,
            Self::Light,
            Self::Catppuccin,
            Self::Dracula,
            Self::Nord,
            Self::HighContrast,
        ]
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::HighContrast => "High Contrast",
            Self::Catppuccin => "Catppuccin",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
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
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "high-contrast" | "high_contrast" | "highcontrast" | "hc" => Some(Self::HighContrast),
            "catppuccin" | "cat" => Some(Self::Catppuccin),
            "dracula" => Some(Self::Dracula),
            "nord" => Some(Self::Nord),
            _ => None,
        }
    }

    fn base_theme(self) -> Theme {
        match self {
            Self::Dark => tokyo_night_theme(),
            Self::Light => themes::light(),
            Self::HighContrast => high_contrast_theme(),
            Self::Catppuccin => catppuccin_theme(),
            Self::Dracula => themes::dracula(),
            Self::Nord => themes::nord(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeColorOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_muted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_subtle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_focused: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollbar_track: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollbar_thumb: Option<String>,
}

impl ThemeColorOverrides {
    fn validate(&self) -> Result<(), ThemeConfigError> {
        validate_color_slot("primary", self.primary.as_deref())?;
        validate_color_slot("secondary", self.secondary.as_deref())?;
        validate_color_slot("accent", self.accent.as_deref())?;
        validate_color_slot("background", self.background.as_deref())?;
        validate_color_slot("surface", self.surface.as_deref())?;
        validate_color_slot("overlay", self.overlay.as_deref())?;
        validate_color_slot("text", self.text.as_deref())?;
        validate_color_slot("text_muted", self.text_muted.as_deref())?;
        validate_color_slot("text_subtle", self.text_subtle.as_deref())?;
        validate_color_slot("success", self.success.as_deref())?;
        validate_color_slot("warning", self.warning.as_deref())?;
        validate_color_slot("error", self.error.as_deref())?;
        validate_color_slot("info", self.info.as_deref())?;
        validate_color_slot("border", self.border.as_deref())?;
        validate_color_slot("border_focused", self.border_focused.as_deref())?;
        validate_color_slot("selection_bg", self.selection_bg.as_deref())?;
        validate_color_slot("selection_fg", self.selection_fg.as_deref())?;
        validate_color_slot("scrollbar_track", self.scrollbar_track.as_deref())?;
        validate_color_slot("scrollbar_thumb", self.scrollbar_thumb.as_deref())?;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.primary.is_none()
            && self.secondary.is_none()
            && self.accent.is_none()
            && self.background.is_none()
            && self.surface.is_none()
            && self.overlay.is_none()
            && self.text.is_none()
            && self.text_muted.is_none()
            && self.text_subtle.is_none()
            && self.success.is_none()
            && self.warning.is_none()
            && self.error.is_none()
            && self.info.is_none()
            && self.border.is_none()
            && self.border_focused.is_none()
            && self.selection_bg.is_none()
            && self.selection_fg.is_none()
            && self.scrollbar_track.is_none()
            && self.scrollbar_thumb.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_theme_config_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_preset: Option<UiThemePreset>,
    #[serde(default)]
    pub colors: ThemeColorOverrides,
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
        self.colors.validate()?;
        Ok(())
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            version: THEME_CONFIG_VERSION,
            base_preset: None,
            colors: ThemeColorOverrides::default(),
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
    #[error("invalid color value for `{field}`: {value}")]
    InvalidColorValue { field: &'static str, value: String },
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
            preset: UiThemePreset::Dark,
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

    fn from_env_values(values: EnvValues<'_>) -> Self {
        let preset = values
            .cass_theme
            .and_then(UiThemePreset::parse)
            .unwrap_or(UiThemePreset::Dark);

        let no_color_enabled = values.cass_no_color.is_some()
            || (env_truthy(values.cass_respect_no_color) && values.no_color.is_some());

        let detected_profile = ColorProfile::detect_from_env(None, values.colorterm, values.term);
        let profile_override = values.cass_color_profile.and_then(parse_color_profile);
        let color_profile = if no_color_enabled {
            ColorProfile::Mono
        } else {
            profile_override.unwrap_or(detected_profile)
        };

        let a11y = env_truthy(values.cass_a11y);
        let no_icons = values.cass_no_icons.is_some();
        let no_gradient = values.cass_no_gradient.is_some() || no_color_enabled || a11y;

        let dark_mode = match preset {
            UiThemePreset::Light => false,
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
        Self::build(options, None).expect("base style options must always produce a valid theme")
    }

    pub fn from_options_with_theme_config(
        mut options: StyleOptions,
        config: &ThemeConfig,
    ) -> Result<Self, ThemeConfigError> {
        config.validate()?;

        if let Some(base_preset) = config.base_preset {
            options.preset = base_preset;
            options.dark_mode = match base_preset {
                UiThemePreset::Light => false,
                UiThemePreset::HighContrast => Theme::detect_dark_mode(),
                _ => true,
            };
        }

        Self::build(options, Some(&config.colors))
    }

    fn build(
        options: StyleOptions,
        overrides: Option<&ThemeColorOverrides>,
    ) -> Result<Self, ThemeConfigError> {
        let mut theme = options.preset.base_theme();

        if let Some(overrides) = overrides {
            theme = apply_theme_overrides(theme, overrides)?;
        }

        if options.a11y && options.preset != UiThemePreset::HighContrast {
            theme = apply_a11y_overrides(theme);
        }

        theme = downgrade_theme_for_profile(theme, options.color_profile);

        let dark_mode = if options.preset == UiThemePreset::Light {
            false
        } else {
            options.dark_mode
        };
        let resolved = theme.resolve(dark_mode);
        let sheet = build_stylesheet(resolved, options);
        let role_markers = RoleMarkers::from_options(options);

        Ok(Self {
            options,
            theme,
            resolved,
            sheet,
            role_markers,
        })
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
        Style::new().fg(pane.accent).bold()
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

fn apply_theme_overrides(
    theme: Theme,
    overrides: &ThemeColorOverrides,
) -> Result<Theme, ThemeConfigError> {
    overrides.validate()?;

    if overrides.is_empty() {
        return Ok(theme);
    }

    let mut builder = ThemeBuilder::from_theme(theme);

    if let Some(value) = overrides.primary.as_deref() {
        builder = builder.primary(parse_color_slot("primary", value)?);
    }
    if let Some(value) = overrides.secondary.as_deref() {
        builder = builder.secondary(parse_color_slot("secondary", value)?);
    }
    if let Some(value) = overrides.accent.as_deref() {
        builder = builder.accent(parse_color_slot("accent", value)?);
    }
    if let Some(value) = overrides.background.as_deref() {
        builder = builder.background(parse_color_slot("background", value)?);
    }
    if let Some(value) = overrides.surface.as_deref() {
        builder = builder.surface(parse_color_slot("surface", value)?);
    }
    if let Some(value) = overrides.overlay.as_deref() {
        builder = builder.overlay(parse_color_slot("overlay", value)?);
    }
    if let Some(value) = overrides.text.as_deref() {
        builder = builder.text(parse_color_slot("text", value)?);
    }
    if let Some(value) = overrides.text_muted.as_deref() {
        builder = builder.text_muted(parse_color_slot("text_muted", value)?);
    }
    if let Some(value) = overrides.text_subtle.as_deref() {
        builder = builder.text_subtle(parse_color_slot("text_subtle", value)?);
    }
    if let Some(value) = overrides.success.as_deref() {
        builder = builder.success(parse_color_slot("success", value)?);
    }
    if let Some(value) = overrides.warning.as_deref() {
        builder = builder.warning(parse_color_slot("warning", value)?);
    }
    if let Some(value) = overrides.error.as_deref() {
        builder = builder.error(parse_color_slot("error", value)?);
    }
    if let Some(value) = overrides.info.as_deref() {
        builder = builder.info(parse_color_slot("info", value)?);
    }
    if let Some(value) = overrides.border.as_deref() {
        builder = builder.border(parse_color_slot("border", value)?);
    }
    if let Some(value) = overrides.border_focused.as_deref() {
        builder = builder.border_focused(parse_color_slot("border_focused", value)?);
    }
    if let Some(value) = overrides.selection_bg.as_deref() {
        builder = builder.selection_bg(parse_color_slot("selection_bg", value)?);
    }
    if let Some(value) = overrides.selection_fg.as_deref() {
        builder = builder.selection_fg(parse_color_slot("selection_fg", value)?);
    }
    if let Some(value) = overrides.scrollbar_track.as_deref() {
        builder = builder.scrollbar_track(parse_color_slot("scrollbar_track", value)?);
    }
    if let Some(value) = overrides.scrollbar_thumb.as_deref() {
        builder = builder.scrollbar_thumb(parse_color_slot("scrollbar_thumb", value)?);
    }

    Ok(builder.build())
}

fn validate_color_slot(field: &'static str, value: Option<&str>) -> Result<(), ThemeConfigError> {
    if let Some(raw) = value {
        parse_color_slot(field, raw)?;
    }
    Ok(())
}

fn parse_color_slot(field: &'static str, value: &str) -> Result<Color, ThemeConfigError> {
    parse_hex_color(value).ok_or_else(|| ThemeConfigError::InvalidColorValue {
        field,
        value: value.trim().to_string(),
    })
}

fn parse_hex_color(raw: &str) -> Option<Color> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    match hex.len() {
        3 => {
            let mut chars = hex.chars();
            let r = chars.next()?;
            let g = chars.next()?;
            let b = chars.next()?;
            Some(Color::rgb(
                nibble_to_u8(r)? * 17,
                nibble_to_u8(g)? * 17,
                nibble_to_u8(b)? * 17,
            ))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::rgb(r, g, b))
        }
        _ => None,
    }
}

fn nibble_to_u8(value: char) -> Option<u8> {
    value.to_digit(16).map(|digit| digit as u8)
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

fn build_stylesheet(resolved: ResolvedTheme, options: StyleOptions) -> StyleSheet {
    let sheet = StyleSheet::new();

    let zebra_bg = if options.gradients_enabled() {
        blend(resolved.surface, resolved.background, 0.35).downgrade(options.color_profile)
    } else {
        resolved.surface
    };

    let role_user = resolved.primary;
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
    sheet.define(STYLE_SCORE_MID, Style::new().fg(to_packed(resolved.info)));
    sheet.define(
        STYLE_SCORE_LOW,
        Style::new().fg(to_packed(resolved.text_subtle)).dim(),
    );

    sheet.define(
        STYLE_KBD_KEY,
        Style::new().fg(to_packed(resolved.accent)).bold(),
    );
    sheet.define(
        STYLE_KBD_DESC,
        Style::new().fg(to_packed(resolved.text_subtle)),
    );

    sheet.define(
        STYLE_PILL_ACTIVE,
        Style::new().fg(to_packed(resolved.secondary)).bold(),
    );

    sheet.define(
        STYLE_TAB_ACTIVE,
        Style::new()
            .fg(to_packed(resolved.accent))
            .bold()
            .underline(),
    );
    sheet.define(
        STYLE_TAB_INACTIVE,
        Style::new().fg(to_packed(resolved.text_muted)),
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
            contrast_check("text/background", resolved.text, resolved.background, 4.5),
            contrast_check("text/surface", resolved.text, resolved.surface, 4.5),
            contrast_check(
                "selection_fg/selection_bg",
                resolved.selection_fg,
                resolved.selection_bg,
                4.5,
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
        assert_eq!(UiThemePreset::parse("dark"), Some(UiThemePreset::Dark));
        assert_eq!(UiThemePreset::parse("light"), Some(UiThemePreset::Light));
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

        assert_eq!(UiThemePreset::Dark.next(), UiThemePreset::Light);
        assert_eq!(UiThemePreset::Light.previous(), UiThemePreset::Dark);
        assert_eq!(UiThemePreset::Dark.previous(), UiThemePreset::HighContrast);
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
    fn dark_preset_matches_tokyo_night_palette() {
        let context = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::Dark,
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
            preset: UiThemePreset::Dark,
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
            STYLE_KBD_KEY,
            STYLE_KBD_DESC,
            STYLE_PILL_ACTIVE,
            STYLE_TAB_ACTIVE,
            STYLE_TAB_INACTIVE,
        ] {
            assert!(context.sheet.contains(key), "missing style token: {key}");
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
            preset: UiThemePreset::Dark,
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
            let dark_mode = !matches!(preset, UiThemePreset::Light);
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
                ratio >= 4.5,
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
            colors: ThemeColorOverrides {
                text: Some("#E6EDF3".to_string()),
                background: Some("#0D1117".to_string()),
                accent: Some("#58A6FF".to_string()),
                border_focused: Some("#FFD700".to_string()),
                ..ThemeColorOverrides::default()
            },
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
            colors: ThemeColorOverrides {
                text: Some("#fefefe".to_string()),
                background: Some("#101218".to_string()),
                surface: Some("#1b1f2a".to_string()),
                selection_bg: Some("#ffd166".to_string()),
                selection_fg: Some("#111111".to_string()),
                ..ThemeColorOverrides::default()
            },
        };

        let json = config.to_json_pretty().expect("config should serialize");
        let expected = r##"{
  "version": 1,
  "base_preset": "catppuccin",
  "colors": {
    "background": "#101218",
    "surface": "#1b1f2a",
    "text": "#fefefe",
    "selection_bg": "#ffd166",
    "selection_fg": "#111111"
  }
}"##;
        assert_eq!(json, expected);
    }

    #[test]
    fn invalid_theme_color_is_rejected() {
        let config_json = r#"{
  "version": 1,
  "base_preset": "dark",
  "colors": {
    "text": "not-a-color"
  }
}"#;

        let err = ThemeConfig::from_json_str(config_json).expect_err("invalid color must fail");
        assert!(matches!(
            err,
            ThemeConfigError::InvalidColorValue { field: "text", .. }
        ));
    }

    #[test]
    fn theme_config_allows_known_preset_aliases() {
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
    fn custom_theme_downgrades_to_ansi16_profile() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Dark),
            colors: ThemeColorOverrides {
                text: Some("#00e5ff".to_string()),
                background: Some("#050608".to_string()),
                ..ThemeColorOverrides::default()
            },
        };

        let context = StyleContext::from_options_with_theme_config(
            StyleOptions {
                preset: UiThemePreset::Light,
                dark_mode: false,
                color_profile: ColorProfile::Ansi16,
                no_color: false,
                no_icons: false,
                no_gradient: false,
                a11y: false,
            },
            &config,
        )
        .expect("theme config should apply");

        assert!(matches!(context.resolved.text, Color::Ansi16(_)));
        assert!(matches!(context.resolved.background, Color::Ansi16(_)));
    }

    #[test]
    fn contrast_report_flags_low_contrast_theme() {
        let config = ThemeConfig {
            version: THEME_CONFIG_VERSION,
            base_preset: Some(UiThemePreset::Dark),
            colors: ThemeColorOverrides {
                text: Some("#222222".to_string()),
                background: Some("#202020".to_string()),
                ..ThemeColorOverrides::default()
            },
        };

        let context = StyleContext::from_options_with_theme_config(
            StyleOptions {
                preset: UiThemePreset::Dark,
                dark_mode: true,
                color_profile: ColorProfile::TrueColor,
                no_color: false,
                no_icons: false,
                no_gradient: false,
                a11y: false,
            },
            &config,
        )
        .expect("theme config should apply");

        let report = context.contrast_report();
        assert!(report.has_failures());
        assert!(report.failing_pairs().contains(&"text/background"));
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
            colors: ThemeColorOverrides {
                accent: Some("#ff00ff".to_string()),
                ..ThemeColorOverrides::default()
            },
        };

        config
            .save_to_path(&path)
            .expect("theme config should save to disk");
        let loaded = ThemeConfig::load_from_path(&path).expect("theme config should reload");
        assert_eq!(loaded, config);

        let _ = fs::remove_file(path);
    }
}
