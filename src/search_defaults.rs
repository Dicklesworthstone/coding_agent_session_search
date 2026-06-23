//! Configurable defaults for `cass search` / `cass pack` (gh #303).
//!
//! Historically the `--timeout`, `--limit` and `--mode` flags on `cass search`
//! had only their clap-level defaults: omit `--timeout` and the search ran with
//! no wall-clock bound at all, so a broad query with `--limit 0` could spin a
//! large index indefinitely with no way to auto-terminate. Agents that drive
//! cass via fixed prompt templates could not opt into a default timeout without
//! appending `--timeout <ms>` to every single invocation across every agent
//! config — fragile, and easy to forget.
//!
//! This module lets the operator set a *default* search timeout (and, while we
//! are here, default `limit` and `mode`) once, via either:
//!
//! 1. An **environment variable** — `CASS_SEARCH_TIMEOUT_MS` (the issue also
//!    mentions `CASS_SEARCH_TIMEOUT`; both are accepted, `_MS` wins if both are
//!    set, since the value is unambiguously milliseconds to match the existing
//!    `--timeout` flag). Likewise `CASS_SEARCH_LIMIT` and `CASS_SEARCH_MODE`.
//! 2. A **config file** — `~/.config/cass/cass.toml` (XDG-resolved exactly like
//!    the existing `sources.toml`), with a `[search]` table:
//!
//!    ```toml
//!    [search]
//!    timeout_ms = 300000   # 5 minutes
//!    limit      = 200
//!    mode       = "hybrid"
//!    ```
//!
//! The original issue example used YAML; cass already standardizes on TOML for
//! its user config (`sources.toml`), so the config file is TOML for consistency
//! and to reuse the existing `toml` dependency and XDG path resolution. No new
//! dependency is introduced.
//!
//! ## Precedence (highest wins)
//!
//! ```text
//! explicit CLI flag  >  environment variable  >  config file  >  built-in default
//! ```
//!
//! For `timeout` the built-in default is "no timeout" (`None`), preserving the
//! pre-#303 behavior for anyone who configures nothing. For `limit` the built-in
//! default is clap's `0` ("no limit", still RAM-capped downstream), and for
//! `mode` the built-in default is "unset" (`None` → hybrid-preferred downstream).
//!
//! All resolution logic is pure (`resolve_*` take the already-read env/config
//! values as arguments) so it is unit-tested without mutating process-global env
//! — important because the test suite runs in parallel and every search test
//! transitively reads env.

use std::path::PathBuf;

use serde::Deserialize;

/// The `[search]` table of `~/.config/cass/cass.toml`.
///
/// Every field is optional: an absent field falls through to the next lower
/// precedence source. Unknown keys are ignored (forward-compatible).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct SearchDefaults {
    /// Default search timeout in milliseconds. `None` = no timeout (legacy
    /// behavior). A value of `0` is treated as "no timeout" as well, so the
    /// config can't accidentally make every search fail instantly.
    pub timeout_ms: Option<u64>,
    /// Default result limit. `None` = use clap's `0` ("no limit", RAM-capped).
    pub limit: Option<usize>,
    /// Default search mode: `lexical`, `semantic`, or `hybrid`. Stored as a
    /// string here and validated at resolution time so an invalid value yields
    /// a clear error rather than a confusing deserialize failure for the whole
    /// config file.
    pub mode: Option<String>,
}

/// Top-level shape of `~/.config/cass/cass.toml`.
///
/// Only the `[search]` table is consumed today. Other tables are ignored so the
/// same file can grow additional sections later without breaking older binaries.
#[derive(Debug, Clone, Default, Deserialize)]
struct CassConfigFile {
    #[serde(default)]
    search: SearchDefaults,
}

/// Errors surfaced while loading the config file. Kept narrow and stringly so
/// callers can fold them into the existing `CliError` surface.
#[derive(Debug)]
pub enum ConfigLoadError {
    /// The file exists but could not be read.
    Read(std::io::Error),
    /// The file exists but is not valid TOML.
    Parse(String),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::Read(e) => write!(f, "failed to read cass config: {e}"),
            ConfigLoadError::Parse(e) => {
                write!(f, "invalid cass config (~/.config/cass/cass.toml): {e}")
            }
        }
    }
}

impl std::error::Error for ConfigLoadError {}

/// Resolve the path to the global cass config file (`cass.toml`), mirroring the
/// XDG resolution used for `sources.toml` so both live side-by-side.
///
/// - Primary: `$XDG_CONFIG_HOME/cass/cass.toml`
/// - Then: platform config dir (e.g. `~/Library/Application Support/cass/` on
///   macOS, `~/.config/cass/` on Linux) when it already exists
/// - Then: `~/.config/cass/cass.toml` when it already exists
/// - Else: the platform path (for documentation / future creation)
pub fn config_path() -> Option<PathBuf> {
    config_path_from_parts(
        dotenvy::var("XDG_CONFIG_HOME").ok().map(PathBuf::from),
        dirs::config_dir(),
        dirs::home_dir(),
    )
}

fn config_path_from_parts(
    xdg_config_home: Option<PathBuf>,
    platform_config_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        let trimmed = xdg.as_os_str().is_empty();
        if !trimmed {
            return Some(xdg.join("cass").join("cass.toml"));
        }
    }

    let platform_path = platform_config_dir.map(|p| p.join("cass").join("cass.toml"));
    if let Some(ref path) = platform_path
        && path.exists()
    {
        return Some(path.clone());
    }

    if let Some(home) = home_dir {
        let dot_config = home.join(".config").join("cass").join("cass.toml");
        if dot_config.exists() {
            return Some(dot_config);
        }
    }

    platform_path
}

/// Load `[search]` defaults from the config file.
///
/// Returns `Ok(SearchDefaults::default())` when the file is absent (the common
/// case) — only a present-but-broken file is an error, so a missing config is
/// never fatal.
pub fn load_search_defaults() -> Result<SearchDefaults, ConfigLoadError> {
    let Some(path) = config_path() else {
        return Ok(SearchDefaults::default());
    };
    load_search_defaults_from(&path)
}

/// Pure-ish loader against an explicit path (used by `load_search_defaults` and
/// by tests). A non-existent path yields defaults; a present file is read and
/// parsed.
pub fn load_search_defaults_from(
    path: &std::path::Path,
) -> Result<SearchDefaults, ConfigLoadError> {
    if !path.exists() {
        return Ok(SearchDefaults::default());
    }
    let contents = std::fs::read_to_string(path).map_err(ConfigLoadError::Read)?;
    parse_search_defaults(&contents)
}

/// Parse the `[search]` table out of a TOML config string.
pub fn parse_search_defaults(contents: &str) -> Result<SearchDefaults, ConfigLoadError> {
    let file: CassConfigFile =
        toml::from_str(contents).map_err(|e| ConfigLoadError::Parse(e.to_string()))?;
    Ok(file.search)
}

/// Read the timeout environment variable, accepting `CASS_SEARCH_TIMEOUT_MS`
/// (preferred, unambiguous units) and the issue's `CASS_SEARCH_TIMEOUT` as an
/// alias. `_MS` wins when both are set.
pub fn timeout_env() -> Option<String> {
    dotenvy::var("CASS_SEARCH_TIMEOUT_MS")
        .ok()
        .or_else(|| dotenvy::var("CASS_SEARCH_TIMEOUT").ok())
}

/// Read the limit environment variable (`CASS_SEARCH_LIMIT`).
pub fn limit_env() -> Option<String> {
    dotenvy::var("CASS_SEARCH_LIMIT").ok()
}

/// Read the mode environment variable (`CASS_SEARCH_MODE`).
pub fn mode_env() -> Option<String> {
    dotenvy::var("CASS_SEARCH_MODE").ok()
}

/// A single resolution outcome plus which source produced it, so callers can
/// surface a clear error message and (optionally) tell the user where a value
/// came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSource {
    CliFlag,
    EnvVar,
    ConfigFile,
    BuiltIn,
}

/// Resolve the effective search timeout (ms) with full precedence.
///
/// * `cli` — the value of `--timeout`, if the flag was passed.
/// * `env` — the raw `CASS_SEARCH_TIMEOUT[_MS]` string, if set.
/// * `config` — the `[search].timeout_ms` from the config file, if present.
///
/// A timeout of `0` (from env or config) is normalized to `None` ("no timeout")
/// so a misconfiguration can't make every search fail before it starts; an
/// explicit `--timeout 0` on the CLI is likewise treated as "no timeout".
/// A non-numeric env value is a hard error (the operator clearly meant to set
/// it; silently ignoring would hide the typo).
pub fn resolve_timeout_ms(
    cli: Option<u64>,
    env: Option<&str>,
    config: Option<u64>,
) -> Result<(Option<u64>, DefaultSource), String> {
    if let Some(v) = cli {
        return Ok((normalize_timeout(v), DefaultSource::CliFlag));
    }
    if let Some(raw) = env {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // An empty env var is treated as "unset" so `CASS_SEARCH_TIMEOUT=`
            // (a common way to clear a value in a shell rc) falls through.
        } else {
            let parsed: u64 = trimmed.parse().map_err(|_| {
                format!(
                    "CASS_SEARCH_TIMEOUT_MS must be a non-negative integer (milliseconds), got {raw:?}"
                )
            })?;
            return Ok((normalize_timeout(parsed), DefaultSource::EnvVar));
        }
    }
    if let Some(v) = config {
        return Ok((normalize_timeout(v), DefaultSource::ConfigFile));
    }
    Ok((None, DefaultSource::BuiltIn))
}

fn normalize_timeout(v: u64) -> Option<u64> {
    if v == 0 { None } else { Some(v) }
}

/// Resolve the effective `--limit` with full precedence.
///
/// The CLI default is `0` ("no limit"), so we cannot distinguish "user passed
/// `--limit 0`" from "user omitted `--limit`" at this layer — the caller passes
/// `cli_was_explicit` (true when the flag appeared on the command line). When
/// the flag was omitted, env then config then the built-in `0` apply.
pub fn resolve_limit(
    cli_value: usize,
    cli_was_explicit: bool,
    env: Option<&str>,
    config: Option<usize>,
) -> Result<(usize, DefaultSource), String> {
    if cli_was_explicit {
        return Ok((cli_value, DefaultSource::CliFlag));
    }
    if let Some(raw) = env {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let parsed: usize = trimmed.parse().map_err(|_| {
                format!("CASS_SEARCH_LIMIT must be a non-negative integer, got {raw:?}")
            })?;
            return Ok((parsed, DefaultSource::EnvVar));
        }
    }
    if let Some(v) = config {
        return Ok((v, DefaultSource::ConfigFile));
    }
    Ok((cli_value, DefaultSource::BuiltIn))
}

/// Resolve the effective `--mode` string with full precedence. Returns the
/// canonical lowercase mode name (`lexical` | `semantic` | `hybrid`) or `None`
/// when nothing is configured (caller falls back to hybrid-preferred).
pub fn resolve_mode(
    cli: Option<&str>,
    env: Option<&str>,
    config: Option<&str>,
) -> Result<(Option<String>, DefaultSource), String> {
    if let Some(m) = cli {
        return Ok((Some(validate_mode(m)?), DefaultSource::CliFlag));
    }
    if let Some(m) = env {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            return Ok((Some(validate_mode(trimmed)?), DefaultSource::EnvVar));
        }
    }
    if let Some(m) = config {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            return Ok((Some(validate_mode(trimmed)?), DefaultSource::ConfigFile));
        }
    }
    Ok((None, DefaultSource::BuiltIn))
}

fn validate_mode(value: &str) -> Result<String, String> {
    match value.to_ascii_lowercase().as_str() {
        m @ ("lexical" | "semantic" | "hybrid") => Ok(m.to_string()),
        other => Err(format!(
            "invalid search mode {other:?}; expected one of: lexical, semantic, hybrid"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_cli_beats_env_and_config() {
        let (v, src) = resolve_timeout_ms(Some(1000), Some("2000"), Some(3000)).unwrap();
        assert_eq!(v, Some(1000));
        assert_eq!(src, DefaultSource::CliFlag);
    }

    #[test]
    fn timeout_env_beats_config_when_no_cli() {
        let (v, src) = resolve_timeout_ms(None, Some("2000"), Some(3000)).unwrap();
        assert_eq!(v, Some(2000));
        assert_eq!(src, DefaultSource::EnvVar);
    }

    #[test]
    fn timeout_config_used_when_no_cli_or_env() {
        let (v, src) = resolve_timeout_ms(None, None, Some(3000)).unwrap();
        assert_eq!(v, Some(3000));
        assert_eq!(src, DefaultSource::ConfigFile);
    }

    #[test]
    fn timeout_builtin_is_none() {
        let (v, src) = resolve_timeout_ms(None, None, None).unwrap();
        assert_eq!(v, None);
        assert_eq!(src, DefaultSource::BuiltIn);
    }

    #[test]
    fn timeout_zero_normalizes_to_none_everywhere() {
        assert_eq!(resolve_timeout_ms(Some(0), None, None).unwrap().0, None);
        assert_eq!(resolve_timeout_ms(None, Some("0"), None).unwrap().0, None);
        assert_eq!(resolve_timeout_ms(None, None, Some(0)).unwrap().0, None);
    }

    #[test]
    fn timeout_empty_env_falls_through_to_config() {
        let (v, src) = resolve_timeout_ms(None, Some("   "), Some(3000)).unwrap();
        assert_eq!(v, Some(3000));
        assert_eq!(src, DefaultSource::ConfigFile);
    }

    #[test]
    fn timeout_non_numeric_env_is_an_error() {
        let err = resolve_timeout_ms(None, Some("soon"), None).unwrap_err();
        assert!(err.contains("CASS_SEARCH_TIMEOUT_MS"), "{err}");
    }

    #[test]
    fn limit_explicit_cli_wins_even_at_zero() {
        let (v, src) = resolve_limit(0, true, Some("200"), Some(300)).unwrap();
        assert_eq!(v, 0);
        assert_eq!(src, DefaultSource::CliFlag);
    }

    #[test]
    fn limit_env_then_config_then_builtin() {
        assert_eq!(
            resolve_limit(0, false, Some("200"), Some(300)).unwrap(),
            (200, DefaultSource::EnvVar)
        );
        assert_eq!(
            resolve_limit(0, false, None, Some(300)).unwrap(),
            (300, DefaultSource::ConfigFile)
        );
        assert_eq!(
            resolve_limit(0, false, None, None).unwrap(),
            (0, DefaultSource::BuiltIn)
        );
    }

    #[test]
    fn limit_non_numeric_env_is_an_error() {
        assert!(resolve_limit(0, false, Some("lots"), None).is_err());
    }

    #[test]
    fn mode_precedence_and_canonicalization() {
        assert_eq!(
            resolve_mode(Some("LEXICAL"), Some("semantic"), Some("hybrid")).unwrap(),
            (Some("lexical".to_string()), DefaultSource::CliFlag)
        );
        assert_eq!(
            resolve_mode(None, Some("Semantic"), Some("hybrid")).unwrap(),
            (Some("semantic".to_string()), DefaultSource::EnvVar)
        );
        assert_eq!(
            resolve_mode(None, None, Some("Hybrid")).unwrap(),
            (Some("hybrid".to_string()), DefaultSource::ConfigFile)
        );
        assert_eq!(
            resolve_mode(None, None, None).unwrap(),
            (None, DefaultSource::BuiltIn)
        );
    }

    #[test]
    fn mode_invalid_value_is_an_error() {
        assert!(resolve_mode(None, Some("fuzzy"), None).is_err());
        assert!(resolve_mode(None, None, Some("vector")).is_err());
    }

    #[test]
    fn parse_full_search_table() {
        let toml = r#"
            [search]
            timeout_ms = 300000
            limit = 200
            mode = "hybrid"
        "#;
        let d = parse_search_defaults(toml).unwrap();
        assert_eq!(d.timeout_ms, Some(300000));
        assert_eq!(d.limit, Some(200));
        assert_eq!(d.mode.as_deref(), Some("hybrid"));
    }

    #[test]
    fn parse_partial_search_table() {
        let toml = "[search]\ntimeout_ms = 5000\n";
        let d = parse_search_defaults(toml).unwrap();
        assert_eq!(d.timeout_ms, Some(5000));
        assert_eq!(d.limit, None);
        assert_eq!(d.mode, None);
    }

    #[test]
    fn parse_empty_or_unrelated_config_is_default() {
        assert_eq!(
            parse_search_defaults("").unwrap(),
            SearchDefaults::default()
        );
        // An unrelated table (e.g. a future section) must not error.
        assert_eq!(
            parse_search_defaults("[other]\nfoo = 1\n").unwrap(),
            SearchDefaults::default()
        );
    }

    #[test]
    fn parse_unknown_keys_in_search_table_are_ignored() {
        let toml = "[search]\ntimeout_ms = 1\nfuture_key = true\n";
        let d = parse_search_defaults(toml).unwrap();
        assert_eq!(d.timeout_ms, Some(1));
    }

    #[test]
    fn parse_broken_toml_is_an_error() {
        assert!(parse_search_defaults("[search\ntimeout_ms = ").is_err());
    }

    #[test]
    fn load_from_missing_path_is_default() {
        let d = load_search_defaults_from(std::path::Path::new(
            "/nonexistent/cass/cass.toml.definitely-not-here",
        ))
        .unwrap();
        assert_eq!(d, SearchDefaults::default());
    }

    #[test]
    fn config_path_prefers_xdg_config_home() {
        let p = config_path_from_parts(
            Some(PathBuf::from("/xdg")),
            Some(PathBuf::from("/platform")),
            Some(PathBuf::from("/home/u")),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/xdg/cass/cass.toml"));
    }

    #[test]
    fn config_path_falls_back_to_platform_when_no_files_exist() {
        // Neither platform nor ~/.config file exists -> platform path returned.
        let p = config_path_from_parts(
            None,
            Some(PathBuf::from("/platform")),
            Some(PathBuf::from("/home/definitely-missing")),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/platform/cass/cass.toml"));
    }
}
