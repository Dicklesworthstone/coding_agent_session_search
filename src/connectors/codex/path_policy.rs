//! Operator scan exclusions shared by Codex admission and raw-mirror capture.
//!
//! Compare path components, not string prefixes. Relative names use the working
//! directory captured with the policy; existing filesystem aliases are resolved
//! at each admission (never cached across symlink changes). Missing final names
//! are compared through their nearest existing ancestor. Resolution reads only
//! filesystem metadata, never source contents, and never creates directories.
//!
//! This is an admission policy, not a filesystem sandbox: a hostile concurrent
//! namespace change between admission and opening still requires handle-bound
//! enforcement at the reader. Do not claim canonicalization eliminates that race.

use std::io;
use std::path::{Component, Path, PathBuf};

/// GH #489: `CASS_EXCLUDE_PATHS` is comma/newline-delimited, but a PATH-style
/// list (`/a/x:/b/y`) is an easy mistake. It is parsed as one nonexistent path
/// that excludes nothing, so the sources the operator meant to skip are still
/// scanned. Name the entries that look like that. An entry only qualifies when
/// every colon-separated piece is itself an absolute or home-relative path and
/// the whole entry does not exist, so Windows drive paths (`C:\x`) and real
/// names containing `:` never warn.
pub(crate) fn colon_separated_exclusion_warning(value: &str) -> Option<String> {
    let suspicious: Vec<&str> = value
        .split([',', '\n'])
        .map(str::trim)
        .filter(|entry| {
            let mut pieces = entry.split(':');
            let looks_like_list = entry.contains(':')
                && pieces.all(|piece| piece.starts_with('/') || piece.starts_with("~/"));
            looks_like_list && !Path::new(entry).exists()
        })
        .collect();
    (!suspicious.is_empty()).then(|| {
        format!(
            "CASS_EXCLUDE_PATHS separates entries with commas or newlines, not colons; \
             these entries do not exist and exclude nothing: {}. Use a comma, e.g. {}",
            suspicious.join(", "),
            suspicious[0].replace(':', ",")
        )
    })
}

pub(crate) struct ScanExclusions {
    paths: Vec<PathBuf>,
    cwd: Option<PathBuf>,
    invalid: bool,
}

impl ScanExclusions {
    pub(crate) fn from_env() -> Self {
        match dotenvy::var("CASS_EXCLUDE_PATHS") {
            Ok(value) => Self::parse(&value),
            Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => Self::parse(""),
            // A malformed/non-Unicode policy must not silently disable privacy.
            Err(_) => Self {
                paths: Vec::new(),
                cwd: None,
                invalid: true,
            },
        }
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.invalid || (self.cwd.is_none() && self.paths.iter().any(|path| path.is_relative()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CASS_EXCLUDE_PATHS requires Unicode paths and a resolvable working directory for relative exclusions",
            ));
        }
        Ok(())
    }

    fn parse(value: &str) -> Self {
        Self::parse_in(value, std::env::current_dir().ok())
    }

    fn parse_in(value: &str, cwd: Option<PathBuf>) -> Self {
        Self {
            paths: value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(PathBuf::from)
                .collect(),
            cwd,
            invalid: false,
        }
    }

    fn absolute(&self, path: &Path) -> Option<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.as_ref()?.join(path)
        };
        // In particular, do not guess another Windows drive's working directory.
        absolute.is_absolute().then_some(absolute)
    }

    pub(crate) fn excludes(&self, path: &Path) -> bool {
        if self.invalid {
            return true;
        }
        if self.paths.is_empty() {
            return false;
        }
        // Preserve direct exclusions even for nonexistent or unreadable paths,
        // without touching the source or resolving any filesystem alias.
        if self.paths.iter().any(|excluded| path.starts_with(excluded)) {
            return true;
        }
        let Some(absolute) = self.absolute(path) else {
            return true;
        };
        let mut resolved_source = None;
        for excluded in &self.paths {
            let Some(excluded) = self.absolute(excluded) else {
                return true;
            };
            if absolute.starts_with(&excluded) {
                return true;
            }
            // Resolve BEFORE folding '..': a symlink followed by '..' refers
            // to the target's parent, not the lexical link's parent.
            if resolved_source.is_none() {
                match resolve_existing_ancestor(&absolute) {
                    Ok(resolved) => resolved_source = Some(resolved),
                    Err(_) => return true,
                }
            }
            match resolve_existing_ancestor(&excluded) {
                Ok(excluded) => {
                    if resolved_source
                        .as_ref()
                        .is_some_and(|source: &PathBuf| source.starts_with(excluded))
                    {
                        return true;
                    }
                }
                // With an active policy, inability to establish the namespace
                // is not evidence that a source is outside the excluded scope.
                Err(_) => return true,
            }
        }
        false
    }
}

fn resolve_existing_ancestor(path: &Path) -> io::Result<PathBuf> {
    for ancestor in path.ancestors() {
        match std::fs::canonicalize(ancestor) {
            Ok(parent) => {
                let suffix = path.strip_prefix(ancestor).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid exclusion path ancestry",
                    )
                })?;
                let mut resolved = PathBuf::new();
                for component in parent.join(suffix).components() {
                    match component {
                        Component::CurDir => {}
                        Component::ParentDir => {
                            resolved.pop();
                        }
                        other => resolved.push(other.as_os_str()),
                    }
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "exclusion path has no resolvable ancestor",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn policy(root: &Path, value: &str) -> ScanExclusions {
        ScanExclusions::parse_in(value, Some(root.to_path_buf()))
    }

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

    #[test]
    fn relative_and_absolute_spellings_share_one_scope() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let private = root.path().join("sessions/private");
        let relative = policy(root.path(), "sessions/private");
        let absolute = policy(root.path(), private.to_str().unwrap());
        for exclusions in [relative, absolute] {
            assert!(exclusions.excludes(&private.join("rollout.jsonl")));
            assert!(exclusions.excludes(Path::new("sessions/private/rollout.jsonl")));
            assert!(!exclusions.excludes(&root.path().join("sessions/private-copy/rollout.jsonl")));
        }
        Ok(())
    }

    #[test]
    fn parent_components_and_missing_final_names_cannot_bypass_scope() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("sessions/public"))?;
        fs::create_dir_all(root.path().join("sessions/private"))?;
        let exclusions = policy(root.path(), "sessions/private");
        assert!(exclusions.excludes(&root.path().join("sessions/public/../private/missing.jsonl")));
        let exclusions = policy(root.path(), "sessions/public/../private");
        assert!(exclusions.excludes(&root.path().join("sessions/private/missing.jsonl")));
        assert!(!exclusions.excludes(&root.path().join("sessions/public/allowed.jsonl")));
        Ok(())
    }

    #[test]
    fn empty_policy_does_not_require_a_working_directory() {
        assert!(!ScanExclusions::parse_in(" , \n", None).excludes(Path::new("source.jsonl")));
        assert!(ScanExclusions::parse_in("private", None).excludes(Path::new("source.jsonl")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_on_either_side_preserve_exclusions() -> io::Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let private = root.path().join("private");
        let alias = root.path().join("alias");
        fs::create_dir(&private)?;
        fs::write(private.join("source.jsonl"), "private fixture")?;
        symlink(&private, &alias)?;
        let direct = policy(root.path(), "private");
        assert!(direct.excludes(&alias.join("source.jsonl")));
        assert!(direct.excludes(&alias.join("not-created-yet.jsonl")));
        assert!(policy(root.path(), "alias").excludes(&private.join("source.jsonl")));
        assert!(!direct.excludes(&root.path().join("private-copy/source.jsonl")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_resolved_before_parent_components() -> io::Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("actual/child"))?;
        fs::create_dir_all(root.path().join("actual/private"))?;
        fs::write(
            root.path().join("actual/private/source.jsonl"),
            "private fixture",
        )?;
        symlink(root.path().join("actual/child"), root.path().join("entry"))?;
        let exclusions = policy(root.path(), "actual/private");
        assert!(exclusions.excludes(&root.path().join("entry/../private/source.jsonl")));
        // Lexically folding entry/.. would incorrectly protect this other path.
        assert!(!exclusions.excludes(&root.path().join("private/source.jsonl")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn existing_policy_observes_retargeted_aliases() -> io::Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let private = root.path().join("private");
        let public = root.path().join("public");
        let alias = root.path().join("alias");
        fs::create_dir(&private)?;
        fs::create_dir(&public)?;
        symlink(&public, &alias)?;
        let exclusions = policy(root.path(), "private");
        assert!(!exclusions.excludes(&alias.join("source.jsonl")));
        // Replace a test-owned link atomically; no production path is removed.
        let replacement = root.path().join("replacement");
        symlink(&private, &replacement)?;
        fs::rename(replacement, &alias)?;
        assert!(exclusions.excludes(&alias.join("source.jsonl")));
        Ok(())
    }

    #[test]
    fn colon_separated_exclusions_are_named_with_a_comma_suggestion() {
        let warning = colon_separated_exclusion_warning(
            "/data/old-codex/a.jsonl:/data/old-codex/b.jsonl,~/x:~/y",
        )
        .expect("PATH-style lists must warn");
        assert!(warning.contains("/data/old-codex/a.jsonl:/data/old-codex/b.jsonl"));
        assert!(warning.contains("~/x:~/y"));
        assert!(warning.contains("/data/old-codex/a.jsonl,/data/old-codex/b.jsonl"));
        // The parsed policy really does treat the colon list as one path.
        assert_eq!(
            ScanExclusions::parse_in("/a/x:/b/y", None).paths,
            [PathBuf::from("/a/x:/b/y")]
        );
    }

    #[test]
    fn well_formed_and_drive_style_exclusions_do_not_warn() -> io::Result<()> {
        assert_eq!(colon_separated_exclusion_warning(""), None);
        assert_eq!(colon_separated_exclusion_warning("/a/x,/b/y\n~/z"), None);
        // Windows drive paths and a relative piece are not colon-separated lists.
        assert_eq!(
            colon_separated_exclusion_warning(r"C:\Users\me\.codex"),
            None
        );
        assert_eq!(colon_separated_exclusion_warning("/a/x:relative"), None);
        // A real path whose name contains ':' exists, so it is not a mistake.
        #[cfg(unix)]
        {
            let root = tempfile::tempdir()?;
            let odd = root.path().join("a:");
            fs::create_dir(&odd)?;
            let entry = format!("{}/b", odd.display());
            fs::create_dir(&entry)?;
            assert_eq!(colon_separated_exclusion_warning(&entry), None);
        }
        Ok(())
    }
}
