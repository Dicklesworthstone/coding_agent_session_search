//! CASS-side compatibility guard for GH #486.
//!
//! The published FAD 0.3.0 connector lacks Codex exclusions. Its source hooks
//! let CASS enforce the policy before either parser runs without copying the
//! parser or using an unpublished dependency. Discovery uses the same policy.
//! Keep these lexical, component-aware semantics aligned with FAD's helpers;
//! applying them twice after an upstream upgrade is harmless.

use std::path::{Path, PathBuf};

pub(super) struct ScanExclusions {
    paths: Vec<PathBuf>,
}

impl ScanExclusions {
    pub(super) fn from_env() -> Self {
        Self::parse(&dotenvy::var("CASS_EXCLUDE_PATHS").unwrap_or_default())
    }

    fn parse(value: &str) -> Self {
        Self {
            paths: value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(PathBuf::from)
                .collect(),
        }
    }

    pub(super) fn excludes(&self, path: &Path) -> bool {
        self.paths.iter().any(|excluded| path.starts_with(excluded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_use_path_components_not_string_prefixes() {
        let exclusions = ScanExclusions::parse("sessions/private, sessions/rollout-a.jsonl");
        for path in [
            "sessions/private",
            "sessions/private/rollout-a.jsonl",
            "sessions/rollout-a.jsonl",
        ] {
            assert!(exclusions.excludes(Path::new(path)), "{path}");
        }
        for path in [
            "sessions/private-copy/rollout-a.jsonl",
            "sessions/rollout-a.jsonl-copy.jsonl",
            "sessions/public/rollout-a.jsonl",
        ] {
            assert!(!exclusions.excludes(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn exclusions_trim_mixed_delimiters_and_ignore_empty_entries() {
        let exclusions = ScanExclusions::parse(" , sessions/private ,\r\n sessions/public \n, ");
        assert_eq!(exclusions.paths.len(), 2);
        assert!(exclusions.excludes(Path::new("sessions/private/rollout.jsonl")));
        assert!(exclusions.excludes(Path::new("sessions/public/rollout.jsonl")));
        for value in ["", " , \r\n, "] {
            assert!(!ScanExclusions::parse(value).excludes(Path::new("sessions/rollout.jsonl")));
        }
    }
}
