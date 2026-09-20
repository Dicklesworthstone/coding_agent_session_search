//! Last-mile exclusion enforcement for raw source capture, including fallback.

use std::fmt;
use std::path::Path;

/// Capture was intentionally refused by the operator's scan-path policy.
/// This error deliberately contains neither source contents nor source paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMirrorSourceExcluded;

impl fmt::Display for RawMirrorSourceExcluded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("excluded_source: raw-mirror capture blocked by CASS_EXCLUDE_PATHS")
    }
}

impl std::error::Error for RawMirrorSourceExcluded {}

pub(super) fn ensure_allowed(path: &Path) -> anyhow::Result<()> {
    let value = dotenvy::var("CASS_EXCLUDE_PATHS").unwrap_or_default();
    // Match connector semantics: exact files or component-aware directory
    // prefixes, comma/newline separators, surrounding whitespace ignored.
    // No canonicalization or stat is needed to refuse an excluded path.
    if value
        .split([',', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .any(|part| path.starts_with(Path::new(part)))
    {
        return Err(RawMirrorSourceExcluded.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
