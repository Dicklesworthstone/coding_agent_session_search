//! Operator scan exclusions shared by Codex admission and raw-mirror capture.
//!
//! Compare path components, not string prefixes. Relative names use the working
//! directory captured with the policy; existing filesystem aliases are resolved
//! at each admission (never cached across symlink changes). Missing final names
//! and virtual sessions beneath a database file are compared through their
//! nearest existing ancestor. Resolution reads only
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

    /// Bind scan reuse to the namespaces admission actually compares. Raw
    /// relative names alone do not identify an exclusion after a cwd change,
    /// and a symlink can be retargeted without changing its configured name.
    /// Unresolvable aliases must disable reuse instead of certifying a scan.
    pub(crate) fn reuse_identity(&self) -> io::Result<Vec<(PathBuf, PathBuf)>> {
        self.validate()?;
        self.paths
            .iter()
            .map(|path| {
                let absolute = self.absolute(path).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "unresolvable exclusion path")
                })?;
                let resolved = resolve_existing_ancestor(&absolute)?;
                Ok((absolute, resolved))
            })
            .collect()
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

    /// Raw capture copies an entire physical file. Excluding a logical session
    /// beneath a shared store therefore also excludes that store from capture,
    /// while ordinary source admission may still index its permitted sessions.
    pub(crate) fn excludes_raw_capture(&self, path: &Path) -> bool {
        self.try_excludes_raw_capture(path).unwrap_or(true)
    }

    fn try_excludes_raw_capture(&self, path: &Path) -> io::Result<bool> {
        if self.try_excludes(path)? {
            return Ok(true);
        }
        if self.paths.is_empty() {
            return Ok(false);
        }
        let absolute = self.absolute(path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "unresolvable source path")
        })?;
        match std::fs::metadata(&absolute) {
            Ok(metadata) if metadata.is_file() => {}
            // Reverse containment applies only to whole-file capture. Do not
            // turn one excluded child into an exclusion of its parent directory.
            Ok(_) => return Ok(false),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
        let physical = resolve_existing_ancestor(&absolute)?;
        let sidecar_container = sqlite_sidecar_container(&physical)?;
        for excluded in &self.paths {
            let excluded = self.absolute(excluded).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unresolvable exclusion path")
            })?;
            let excluded = resolve_existing_ancestor(&excluded)?;
            if excluded != physical && excluded.starts_with(&physical) {
                return Ok(true);
            }
            if sidecar_container
                .as_ref()
                .is_some_and(|container| excluded.starts_with(container))
            {
                return Ok(true);
            }
        }
        Ok(false)
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

/// Interpret sidecar names only after resolving source aliases. A symlink
/// called `public-wal` can point to an unrelated public file. The companion
/// container must be an existing regular file and its aliases are resolved too.
fn sqlite_sidecar_container(physical: &Path) -> io::Result<Option<PathBuf>> {
    let Some(name) = physical.file_name() else {
        return Ok(None);
    };
    let Some(suffix) = ["-wal", "-shm", "-journal"]
        .into_iter()
        .find(|suffix| {
            let tail = &name.as_encoded_bytes()[name.len().saturating_sub(suffix.len())..];
            #[cfg(windows)]
            {
                tail.eq_ignore_ascii_case(suffix.as_bytes())
            }
            #[cfg(not(windows))]
            {
                tail == suffix.as_bytes()
            }
        })
        .filter(|suffix| name.len() > suffix.len())
    else {
        return Ok(None);
    };
    // A UTF-8 policy alias can resolve to a native filename that is not UTF-8.
    // Remove only the ASCII sidecar suffix; lossy conversion would change the
    // container identity and could let an excluded session's bytes through.
    #[cfg(unix)]
    let base = {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = name.as_bytes();
        std::ffi::OsString::from_vec(bytes[..bytes.len() - suffix.len()].to_vec())
    };
    #[cfg(windows)]
    let base = {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut units: Vec<u16> = name.encode_wide().collect();
        units.truncate(units.len() - suffix.len());
        std::ffi::OsString::from_wide(&units)
    };
    #[cfg(not(any(unix, windows)))]
    let base = {
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "unresolvable sidecar filename")
        })?;
        std::ffi::OsString::from(&name[..name.len() - suffix.len()])
    };
    let container = physical.with_file_name(base);
    match std::fs::metadata(&container) {
        Ok(metadata) if metadata.is_file() => resolve_existing_ancestor(&container).map(Some),
        Ok(_) => Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
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
            // Shared-store connectors identify a conversation as
            // `database_file/session_id`. The suffix is a logical identifier,
            // so canonicalizing it reports ENOTDIR even though the database
            // itself is resolvable. Retain that suffix after resolving the
            // physical file, just as for a not-yet-created descendant. Other
            // namespace errors still fail closed.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
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
        assert_eq!(relative.reuse_identity()?, absolute.reuse_identity()?);
        let other_cwd = tempfile::tempdir()?;
        assert_ne!(
            relative.reuse_identity()?,
            policy(other_cwd.path(), "sessions/private").reuse_identity()?,
            "relative exclusions in different namespaces cannot share a completion",
        );
        assert!(
            ScanExclusions::parse_in("sessions/private", None)
                .reuse_identity()
                .is_err()
        );
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
    fn virtual_database_sources_are_not_excluded_by_unrelated_paths() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("opencode.db");
        fs::write(&database, b"shared-store fixture")?;
        fs::create_dir(root.path().join("private"))?;
        let source = database.join("ses-allowed");
        let expected = database.canonicalize()?.join("ses-allowed");
        assert_eq!(resolve_existing_ancestor(&source)?, expected);

        for excluded in ["private", "missing/private", "opencode.db-copy"] {
            let exclusions = policy(root.path(), excluded);
            assert!(!exclusions.try_excludes(&source)?, "{excluded}");
            assert!(
                !exclusions.try_excludes(Path::new("opencode.db/ses-allowed"))?,
                "{excluded}"
            );
        }
        Ok(())
    }

    #[test]
    fn virtual_database_exclusions_keep_file_and_session_scopes() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("opencode.db");
        fs::write(&database, b"shared-store fixture")?;
        let private = database.join("ses-private");
        let public = database.join("ses-public");
        let whole_store = policy(root.path(), "opencode.db");
        assert!(whole_store.try_excludes(&database)?);
        assert!(whole_store.try_excludes(&private)?);
        assert!(whole_store.try_excludes(&public)?);
        assert!(!whole_store.try_excludes(&root.path().join("opencode.db-copy/ses-private"))?);

        let session = policy(root.path(), "opencode.db/ses-private");
        session.validate_codex()?;
        assert!(session.try_excludes(&private)?);
        assert!(!session.try_excludes(&database)?);
        assert!(!session.try_excludes(&public)?);
        assert!(!session.try_excludes(&database.join("ses-private-copy"))?);
        assert!(session.try_excludes(&database.join("ses-public/../ses-private"))?);
        assert!(session.excludes_raw_capture(&database));
        assert!(!session.excludes_raw_capture(root.path()));
        let sibling = root.path().join("opencode.db-copy");
        fs::write(&sibling, b"public sibling store")?;
        assert!(!session.excludes_raw_capture(&sibling));
        assert!(!policy(root.path(), "missing/private").excludes_raw_capture(&database));
        Ok(())
    }

    #[test]
    fn raw_virtual_exclusions_cover_sqlite_sidecars_without_expanding_directory_scope()
    -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("opencode.db");
        let sibling = root.path().join("opencode.db-copy");
        fs::write(&database, b"shared store")?;
        fs::write(&sibling, b"sibling store")?;
        let exclusions = policy(root.path(), "opencode.db/ses-private");
        for suffix in ["-wal", "-shm", "-journal"] {
            let private_sidecar = root.path().join(format!("opencode.db{suffix}"));
            let public_sidecar = root.path().join(format!("opencode.db-copy{suffix}"));
            fs::write(&private_sidecar, b"private raw bytes")?;
            fs::write(&public_sidecar, b"public raw bytes")?;
            assert!(exclusions.excludes_raw_capture(&private_sidecar));
            assert!(!exclusions.excludes_raw_capture(&public_sidecar));
            assert!(policy(root.path(), "opencode.db").excludes_raw_capture(&private_sidecar));
        }
        assert!(!exclusions.excludes_raw_capture(root.path()));
        assert!(!exclusions.try_excludes(&database.join("ses-public"))?);
        let ordinary = root.path().join("missing-wal");
        fs::write(&ordinary, b"not a companion of any physical container")?;
        assert!(!policy(root.path(), "missing/session").excludes_raw_capture(&ordinary));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn raw_sidecars_resolve_source_and_container_aliases_before_suffix_matching() -> io::Result<()>
    {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let database = root.path().join("opencode.db");
        let wal = root.path().join("opencode.db-wal");
        let public = root.path().join("public");
        fs::write(&database, b"shared store")?;
        fs::write(&wal, b"private wal")?;
        fs::write(&public, b"public data")?;
        symlink(&database, root.path().join("database-alias"))?;
        symlink(&wal, root.path().join("wal-alias"))?;
        symlink(&public, root.path().join("database-alias-wal"))?;
        let exclusions = policy(root.path(), "database-alias/private");
        assert!(exclusions.excludes_raw_capture(&wal));
        assert!(exclusions.excludes_raw_capture(&root.path().join("wal-alias")));
        assert!(!exclusions.excludes_raw_capture(&root.path().join("database-alias-wal")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn raw_sidecar_exclusions_preserve_non_utf8_physical_names() -> io::Result<()> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let database = root
            .path()
            .join(OsString::from_vec(b"private-\xff.db".to_vec()));
        let public = root
            .path()
            .join(OsString::from_vec(b"public-\xfe.db".to_vec()));
        fs::write(&database, b"shared store")?;
        fs::write(&public, b"public store")?;
        symlink(&database, root.path().join("database-alias"))?;
        let exclusions = policy(root.path(), "database-alias/private");
        assert!(exclusions.excludes_raw_capture(&database));
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut private_name = database.as_os_str().to_owned();
            private_name.push(suffix);
            let private_sidecar = PathBuf::from(private_name);
            let mut public_name = public.as_os_str().to_owned();
            public_name.push(suffix);
            let public_sidecar = PathBuf::from(public_name);
            fs::write(&private_sidecar, b"private raw bytes")?;
            fs::write(&public_sidecar, b"public raw bytes")?;
            assert!(exclusions.excludes_raw_capture(&private_sidecar));
            assert!(!exclusions.excludes_raw_capture(&public_sidecar));
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn raw_sidecar_exclusions_preserve_windows_case_aliases() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("sessions.db");
        fs::write(&database, b"shared store")?;
        let exclusions = policy(root.path(), "sessions.db/private");
        for suffix in ["-WAL", "-sHm", "-JoUrNaL"] {
            let sidecar = root.path().join(format!("sessions.db{suffix}"));
            fs::write(&sidecar, b"private raw bytes")?;
            assert!(exclusions.excludes_raw_capture(&sidecar));
        }
        assert!(!exclusions.try_excludes(&database.join("public"))?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn virtual_database_sources_resolve_file_and_directory_aliases_before_comparison()
    -> io::Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let store_dir = root.path().join("private/store");
        fs::create_dir_all(&store_dir)?;
        let database = store_dir.join("opencode.db");
        fs::write(&database, b"shared-store fixture")?;
        symlink(&database, root.path().join("database-alias"))?;
        symlink(&store_dir, root.path().join("directory-alias"))?;
        let direct = database.join("ses-private");
        let file_alias = root.path().join("database-alias/ses-private");
        let directory_alias = root.path().join("directory-alias/opencode.db/ses-private");
        for excluded in [
            "private/store/opencode.db/ses-private",
            "database-alias/ses-private",
            "directory-alias/opencode.db/ses-private",
            "directory-alias/../store/opencode.db/ses-private",
        ] {
            let exclusions = policy(root.path(), excluded);
            exclusions.validate_codex()?;
            for source in [&direct, &file_alias, &directory_alias] {
                assert!(exclusions.try_excludes(source)?, "{excluded}: {source:?}");
                assert!(
                    !exclusions.try_excludes(&source.with_file_name("ses-public"))?,
                    "{excluded}: {source:?}"
                );
                assert!(
                    exclusions.excludes_raw_capture(source.parent().unwrap()),
                    "a private virtual session must prevent capture of its physical store"
                );
            }
        }
        assert!(policy(root.path(), "private").try_excludes(&file_alias)?);
        assert!(!policy(root.path(), "unrelated").try_excludes(&file_alias)?);
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
        let alias_exclusion = policy(root.path(), "alias");
        let prior_identity = alias_exclusion.reuse_identity()?;
        assert!(!exclusions.excludes(&alias.join("source.jsonl")));
        // Replace a test-owned link atomically; no production path is removed.
        let replacement = root.path().join("replacement");
        symlink(&private, &replacement)?;
        fs::rename(replacement, &alias)?;
        assert!(exclusions.excludes(&alias.join("source.jsonl")));
        assert_ne!(alias_exclusion.reuse_identity()?, prior_identity);
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
