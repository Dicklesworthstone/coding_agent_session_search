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
    if exclusions.excludes_raw_capture(path) {
        return Err(RawMirrorSourceExcluded.into());
    }
    Ok(())
}

#[cfg(test)]
mod alias_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod virtual_store_tests {
    use super::*;
    use crate::raw_mirror::{
        RawMirrorCaptureInput, capture_source_file, capture_source_file_with_chunk_policy,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD_ROOT: &str = "CASS_RAW_VIRTUAL_STORE_TEST_ROOT";
    const CHILD_SOURCE: &str = "CASS_RAW_VIRTUAL_STORE_TEST_SOURCE";
    const CHILD_TEST: &str = "raw_mirror::exclusions::virtual_store_tests::child";

    fn run_child(root: &Path, source: &Path, policy: &Path) -> anyhow::Result<()> {
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .current_dir(root)
            .env(CHILD_ROOT, root)
            .env(CHILD_SOURCE, source)
            .env("CASS_EXCLUDE_PATHS", policy)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("virtual store exclusion regression exceeded 60 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"));
        Ok(())
    }

    #[test]
    fn private_virtual_session_blocks_whole_store_capture_before_storage_initialization()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("sessions.db");
        fs::write(&source, b"private-session-body public-session-body")?;
        run_child(root.path(), &source, Path::new("sessions.db/private"))
    }

    #[test]
    fn private_virtual_session_blocks_all_standard_sqlite_sidecars() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("sessions.db"), b"shared database")?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let source = root.path().join(format!("sessions.db{suffix}"));
            fs::write(&source, b"private-session-body in a sidecar")?;
            run_child(root.path(), &source, Path::new("sessions.db/private"))?;
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_virtual_session_blocks_capture_through_source_and_policy_aliases()
    -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let source = root.path().join("sessions.db");
        fs::write(&source, b"private-session-body public-session-body")?;
        let alias = root.path().join("store-alias");
        symlink(&source, &alias)?;
        run_child(root.path(), &alias, &source.join("private"))?;
        run_child(root.path(), &source, &alias.join("private"))
    }

    #[test]
    fn child() -> anyhow::Result<()> {
        let Ok(root) = dotenvy::var(CHILD_ROOT) else {
            return Ok(());
        };
        let root = PathBuf::from(root);
        let source = PathBuf::from(dotenvy::var(CHILD_SOURCE)?);
        let before = fs::read(&source)?;
        let data = root.join("mirror");
        let input = || RawMirrorCaptureInput {
            data_dir: &data,
            provider: "opencode",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source,
            db_links: &[],
        };
        let errors = [
            capture_source_file(input()).unwrap_err(),
            capture_source_file_with_chunk_policy(input(), 1, 7).unwrap_err(),
        ];
        for error in errors {
            assert!(error.is::<RawMirrorSourceExcluded>(), "{error:#}");
        }
        assert!(
            !data.exists(),
            "excluded raw bytes must never initialize a mirror"
        );
        assert_eq!(fs::read(source)?, before);
        Ok(())
    }
}
