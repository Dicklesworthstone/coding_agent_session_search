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

    /// FAD validates every configured namespace before Codex discovery. Keep
    /// an unresolvable policy from becoming a successful, wholly vetoed scan.
    pub(crate) fn validate_codex(&self) -> io::Result<()> {
        self.validate()?;
        for path in &self.paths {
            let absolute = self.absolute(path).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unresolvable Codex exclusion")
            })?;
            resolve_existing_ancestor(&absolute)?;
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
        // An unresolved namespace cannot establish that the source is outside
        // the excluded scope. Discovery uses the fallible entry point below.
        self.try_excludes(path).unwrap_or(true)
    }

    fn try_excludes(&self, path: &Path) -> io::Result<bool> {
        self.validate()?;
        if self.paths.is_empty() {
            return Ok(false);
        }
        // Preserve direct exclusions even for nonexistent or unreadable paths,
        // without touching the source or resolving any filesystem alias.
        if self.paths.iter().any(|excluded| path.starts_with(excluded)) {
            return Ok(true);
        }
        let absolute = self.absolute(path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "unresolvable source path")
        })?;
        let mut resolved_source = None;
        for excluded in &self.paths {
            let excluded = self.absolute(excluded).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unresolvable exclusion path")
            })?;
            if absolute.starts_with(&excluded) {
                return Ok(true);
            }
            // Resolve BEFORE folding '..': a symlink followed by '..' refers
            // to the target's parent, not the lexical link's parent.
            if resolved_source.is_none() {
                resolved_source = Some(resolve_existing_ancestor(&absolute)?);
            }
            let excluded = resolve_existing_ancestor(&excluded)?;
            if resolved_source
                .as_ref()
                .is_some_and(|source: &PathBuf| source.starts_with(excluded))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    fn excludes_codex(&self, path: &Path) -> bool {
        self.try_excludes_codex(path).unwrap_or(true)
    }

    /// Codex treats a plain rollout and its compressed representation as one
    /// exact file exclusion. Keep the suffix spelling used by each path, and
    /// resolve aliases before removing the compression suffix, as FAD does.
    ///
    /// Discovery must distinguish a confirmed exclusion from a namespace it
    /// cannot resolve. Boolean admission callers continue to fail closed.
    pub(crate) fn try_excludes_codex(&self, path: &Path) -> io::Result<bool> {
        if self.try_excludes(path)? {
            return Ok(true);
        }
        if self.paths.is_empty() {
            return Ok(false);
        }
        let Some(logical_source) = codex_logical_path(path) else {
            return Ok(false);
        };
        let absolute_source = self.absolute(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "unresolvable Codex source path",
            )
        })?;
        if absolute_source.is_dir() {
            return Ok(false);
        }
        let mut resolved_source = None;
        for excluded in &self.paths {
            let absolute_excluded = self.absolute(excluded).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unresolvable Codex exclusion")
            })?;
            if absolute_excluded.is_dir() {
                continue;
            }
            if let Some(logical_excluded) = codex_logical_path(excluded)
                && self.absolute(&logical_source) == self.absolute(&logical_excluded)
            {
                return Ok(true);
            }
            if resolved_source.is_none() {
                resolved_source = Some(codex_logical_path(&resolve_existing_ancestor(
                    &absolute_source,
                )?));
            }
            let resolved = resolve_existing_ancestor(&absolute_excluded)?;
            if let Some(logical_excluded) = codex_logical_path(&resolved)
                && resolved_source.as_ref().and_then(Option::as_ref) == Some(&logical_excluded)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn codex_logical_path(path: &Path) -> Option<PathBuf> {
    if franken_agent_detection::connectors::codex::is_compressed_rollout(path) {
        Some(path.with_extension(""))
    } else if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-"))
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        Some(path.to_path_buf())
    } else {
        None
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
    fn namespace_errors_remain_fail_closed_and_are_reported_to_discovery() -> io::Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        symlink("cycle", root.path().join("cycle"))?;
        let source = root.path().join("cycle/rollout-hidden.jsonl");
        let error = fs::canonicalize(&source).unwrap_err();
        let exclusions = policy(root.path(), "unrelated");
        exclusions.validate_codex()?;
        assert_eq!(
            exclusions.try_excludes_codex(&source).unwrap_err().kind(),
            error.kind()
        );
        assert!(exclusions.excludes(&source));
        assert!(exclusions.excludes_codex(&source));
        Ok(())
    }

    #[test]
    fn codex_file_exclusions_cover_both_representations_and_keep_directory_semantics()
    -> io::Result<()> {
        let root = tempfile::tempdir()?;
        for name in [
            "rollout-a.jsonl",
            "rollout-a.jsonl.zst",
            "rollout-b.JSONL",
            "rollout-b.JSONL.ZST",
        ] {
            std::fs::write(root.path().join(name), b"fixture")?;
        }
        for (plain, compressed) in [
            ("rollout-a.jsonl", "rollout-a.jsonl.zst"),
            ("rollout-b.JSONL", "rollout-b.JSONL.ZST"),
        ] {
            for excluded in [plain, compressed] {
                let policy = policy(root.path(), excluded);
                assert!(policy.excludes_codex(&root.path().join(plain)));
                assert!(policy.excludes_codex(&root.path().join(compressed)));
                assert!(!policy.excludes_codex(&root.path().join("rollout-neighbor.jsonl")));
            }
        }
        let directory = root.path().join("rollout-directory.jsonl");
        fs::create_dir(&directory)?;
        let exclusions = policy(root.path(), "rollout-directory.jsonl");
        assert!(exclusions.excludes_codex(&directory.join("rollout-private.jsonl")));
        assert!(!exclusions.excludes_codex(&root.path().join("rollout-directory.jsonl.zst")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn codex_logical_exclusions_resolve_leaf_aliases_before_format_suffixes() -> io::Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let plain = root.path().join("rollout-target.jsonl");
        let compressed = root.path().join("rollout-target.jsonl.zst");
        fs::write(&plain, b"plain")?;
        fs::write(&compressed, b"compressed")?;
        symlink(&plain, root.path().join("private-link"))?;
        symlink(&compressed, root.path().join("rollout-alias.jsonl.zst"))?;
        assert!(policy(root.path(), "private-link").excludes_codex(&compressed));
        assert!(
            policy(root.path(), "rollout-target.jsonl")
                .excludes_codex(&root.path().join("rollout-alias.jsonl.zst"))
        );
        Ok(())
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
    fn a_colon_separated_list_is_one_entry_not_several() {
        // The indexer's GH #489 warning relies on this: a PATH-style list is
        // one nonexistent path that excludes nothing, not two exclusions.
        let exclusions = ScanExclusions::parse_in("/a/x:/b/y", None);
        assert_eq!(exclusions.paths, [PathBuf::from("/a/x:/b/y")]);
        // Without a drive these are not absolute on Windows, where an
        // unresolvable source fails closed instead.
        #[cfg(unix)]
        {
            assert!(!exclusions.excludes(Path::new("/a/x/rollout.jsonl")));
            assert!(!exclusions.excludes(Path::new("/b/y/rollout.jsonl")));
        }
    }
}
