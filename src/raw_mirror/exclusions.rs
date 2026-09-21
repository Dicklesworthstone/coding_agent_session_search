//! Last-mile exclusion enforcement for raw source capture, including fallback.

use std::fmt;
use std::path::Path;

use crate::connectors::codex::path_policy::ScanExclusions;

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
    let exclusions = ScanExclusions::from_env();
    exclusions.validate()?;
    if exclusions.excludes(path) {
        return Err(RawMirrorSourceExcluded.into());
    }
    Ok(())
}

#[cfg(test)]
mod alias_tests;
#[cfg(test)]
mod tests;
