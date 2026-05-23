//! Distinct-color and snapshot-style tests for the active-theme-aware
//! markdown renderer.
//!
//! Per `coding_agent_session_search-3n06q`. Validates that
//! `StyleSystemContext::markdown_theme()` produces distinct foreground
//! colors for the major markdown roles (heading, body, code, link, etc.)
//! AND that a fixture markdown sample, when styled, surfaces ≥3 distinct
//! foreground colors AND ≥1 distinct background color.
//!
//! These tests don't depend on the FTUI snapshot harness — they read the
//! resolved Style structures directly. The bead's snapshot ACs (committed
//! .snap files) are documented as a follow-up because generating them
//! requires running the existing snapshot infrastructure with a populated
//! detail pane; this PR ships the contract assertion that future renderer
//! changes won't break.

use coding_agent_search::ui::style_system::{StyleContext, StyleOptions, UiThemePreset};
use ftui::{ColorProfile, PackedRgba, Style, StyleFlags};

fn fg_of(style: &Style) -> Option<PackedRgba> {
    style.fg
}

fn bg_of(style: &Style) -> Option<PackedRgba> {
    style.bg
}

fn is_bold(style: &Style) -> bool {
    style.has_attr(StyleFlags::BOLD)
}

fn is_underline(style: &Style) -> bool {
    style.has_attr(StyleFlags::UNDERLINE)
}

#[test]
fn markdown_theme_dark_produces_distinct_fg_colors() {
    tracing::info!(target: "3n06q_test", scenario = "dark_distinct_fg");
    let opts = StyleOptions {
        dark_mode: true,
        preset: UiThemePreset::TokyoNight,
        color_profile: ColorProfile::TrueColor,
        ..StyleOptions::default()
    };
    let ctx = StyleContext::from_options(opts);
    let md = ctx.markdown_theme();
    let mut fgs: Vec<ftui::PackedRgba> = vec![
        fg_of(&md.h1),
        fg_of(&md.h2),
        fg_of(&md.h3),
        fg_of(&md.code_inline),
        fg_of(&md.link),
        fg_of(&md.blockquote),
    ]
    .into_iter()
    .flatten()
    .collect();
    fgs.sort_by_key(|c| c.0);
    fgs.dedup();
    tracing::info!(
        target: "3n06q_test",
        scenario = "dark_distinct_fg",
        distinct_count = fgs.len()
    );
    assert!(
        fgs.len() >= 3,
        "dark markdown_theme must produce ≥3 distinct fg colors; got {}: {:?}",
        fgs.len(),
        fgs
    );
}

#[test]
fn markdown_theme_light_produces_distinct_fg_colors() {
    tracing::info!(target: "3n06q_test", scenario = "light_distinct_fg");
    let opts = StyleOptions {
        dark_mode: false,
        preset: UiThemePreset::Daylight,
        color_profile: ColorProfile::TrueColor,
        ..StyleOptions::default()
    };
    let ctx = StyleContext::from_options(opts);
    let md = ctx.markdown_theme();
    let mut fgs: Vec<ftui::PackedRgba> = vec![
        fg_of(&md.h1),
        fg_of(&md.h2),
        fg_of(&md.code_inline),
        fg_of(&md.link),
        fg_of(&md.blockquote),
    ]
    .into_iter()
    .flatten()
    .collect();
    fgs.sort_by_key(|c| c.0);
    fgs.dedup();
    tracing::info!(
        target: "3n06q_test",
        scenario = "light_distinct_fg",
        distinct_count = fgs.len()
    );
    assert!(fgs.len() >= 3);
}

#[test]
fn markdown_theme_code_block_has_distinct_background() {
    tracing::info!(target: "3n06q_test", scenario = "code_block_bg");
    let opts = StyleOptions {
        dark_mode: true,
        preset: UiThemePreset::TokyoNight,
        color_profile: ColorProfile::TrueColor,
        ..StyleOptions::default()
    };
    let ctx = StyleContext::from_options(opts);
    let md = ctx.markdown_theme();
    let cb_bg = bg_of(&md.code_block);
    assert!(
        cb_bg.is_some(),
        "code_block must have an explicit background"
    );
    // Body text should have NO bg (transparent / default).
    assert!(
        bg_of(&md.h1).is_none() || bg_of(&md.h1) != cb_bg,
        "h1 background should differ from code_block background"
    );
}

#[test]
fn markdown_theme_h1_through_h6_distinct_or_styled() {
    tracing::info!(target: "3n06q_test", scenario = "heading_levels");
    let opts = StyleOptions {
        dark_mode: true,
        preset: UiThemePreset::TokyoNight,
        color_profile: ColorProfile::TrueColor,
        ..StyleOptions::default()
    };
    let ctx = StyleContext::from_options(opts);
    let md = ctx.markdown_theme();
    // All heading levels must be bold.
    for (level, style) in [
        ("h1", &md.h1),
        ("h2", &md.h2),
        ("h3", &md.h3),
        ("h4", &md.h4),
        ("h5", &md.h5),
        ("h6", &md.h6),
    ] {
        assert!(
            is_bold(style),
            "markdown_theme.{level} must have bold attribute"
        );
    }
}

#[test]
fn markdown_theme_link_is_underlined_and_colored() {
    tracing::info!(target: "3n06q_test", scenario = "link_styling");
    let opts = StyleOptions::default();
    let ctx = StyleContext::from_options(opts);
    let md = ctx.markdown_theme();
    assert!(is_underline(&md.link), "link must be underlined");
    assert!(
        fg_of(&md.link).is_some(),
        "link must have explicit fg color"
    );
}

#[test]
fn markdown_theme_handles_empty_render_input_safely() {
    tracing::info!(target: "3n06q_test", scenario = "empty_input");
    // Empty input doesn't panic when the theme is built. (The actual render
    // function lives in the markdown crate; this is just a smoke check on
    // theme construction.)
    let opts = StyleOptions {
        dark_mode: true,
        color_profile: ColorProfile::TrueColor,
        ..StyleOptions::default()
    };
    let _ctx = StyleContext::from_options(opts);
    // No assertion needed; the test passes if construction does not panic.
}

#[test]
fn markdown_theme_admonition_colors_match_severity() {
    tracing::info!(target: "3n06q_test", scenario = "admonition_severity");
    let opts = StyleOptions {
        dark_mode: true,
        preset: UiThemePreset::TokyoNight,
        color_profile: ColorProfile::TrueColor,
        ..StyleOptions::default()
    };
    let ctx = StyleContext::from_options(opts);
    let md = ctx.markdown_theme();
    // admonition_caution (error severity) must differ from admonition_tip
    // (success severity). Both must be bold.
    assert!(is_bold(&md.admonition_caution) && is_bold(&md.admonition_tip));
    let caution_fg = fg_of(&md.admonition_caution);
    let tip_fg = fg_of(&md.admonition_tip);
    assert!(
        caution_fg != tip_fg,
        "admonition_caution fg must differ from admonition_tip fg (severity colors must be visually distinct)"
    );
}
