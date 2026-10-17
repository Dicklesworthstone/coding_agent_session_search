//! Raw source preservation with admission checks shared by every capture route.
//!
//! Connector discovery is not the final privacy boundary: an empty inventory
//! can activate the indexer's legacy raw-mirror fallback (GH #486). Enforce
//! exclusions here too, before source access, mirror creation, locks or caches.

mod exclusions;
mod store;

use std::fmt;

pub use exclusions::RawMirrorSourceExcluded;
pub use store::*;

/// Capture was switched off by the operator with `CASS_RAW_MIRROR` (GH #506).
/// Like [`RawMirrorSourceExcluded`], it names no source path or contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMirrorCaptureDisabled;

impl fmt::Display for RawMirrorCaptureDisabled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("capture_disabled: raw-mirror capture is off (CASS_RAW_MIRROR)")
    }
}

impl std::error::Error for RawMirrorCaptureDisabled {}

/// Whether sources are copied into the raw mirror. `CASS_RAW_MIRROR` defaults
/// on; `0`, `false`, `no` or `off` turns capture off, for machines whose
/// providers keep their own session history (GH #506). Existing captures stay
/// until `cass mirror prune` removes them, and indexing is unaffected; what is
/// lost is the copy that survives a provider deleting its source file.
pub fn capture_enabled() -> bool {
    dotenvy::var("CASS_RAW_MIRROR")
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(true)
}

/// Capture an admitted source using the existing raw-mirror storage policy.
///
/// With capture switched off this returns [`RawMirrorCaptureDisabled`], and an
/// excluded source returns [`RawMirrorSourceExcluded`]; neither reads the
/// source or changes the mirror. Existing captures are not purged by either
/// setting; pruning remains a separate, explicit operation.
pub fn capture_source_file(
    input: RawMirrorCaptureInput<'_>,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    if !capture_enabled() {
        return Err(RawMirrorCaptureDisabled.into());
    }
    exclusions::ensure_allowed(input.source_path)?;
    store::capture_source_file(input)
}

#[cfg(test)]
pub(crate) fn capture_source_file_with_chunk_policy(
    input: RawMirrorCaptureInput<'_>,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    if !capture_enabled() {
        return Err(RawMirrorCaptureDisabled.into());
    }
    exclusions::ensure_allowed(input.source_path)?;
    store::capture_source_file_with_chunk_policy(input, chunk_threshold_bytes, chunk_size_bytes)
}

#[cfg(all(test, unix))]
mod tests {
    /// The unchanged store test launches its child by this historical name.
    /// Forward both ordinary and child invocations to the real test; its own
    /// environment marker selects parent setup versus fresh-process checks.
    #[test]
    fn gh461_capture_cache_survives_fresh_process() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const TARGET: &str = "raw_mirror::store::tests::gh461_capture_cache_survives_fresh_process";
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TARGET, "--nocapture"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start real cache regression");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if child.try_wait().expect("child status").is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("cache regression forwarding exceeded 30 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().expect("cache regression output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains(&format!("test {TARGET} ... ok")),
            "{stdout}"
        );
        assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
    }

    const SWITCH_CHILD: &str = "raw_mirror::tests::gh506_capture_switch_child";
    const SWITCH_ROOT: &str = "CASS_RAW_MIRROR_SWITCH_TEST_ROOT";
    const SWITCH_EXPECT: &str = "CASS_RAW_MIRROR_SWITCH_TEST_EXPECT";

    /// GH #506: only an explicit off value disables capture, and a disabled
    /// capture refuses before touching the source or creating the mirror. Each
    /// setting runs in a child process, so this process's environment is never
    /// mutated under concurrently running tests.
    #[test]
    fn gh506_capture_switch_refuses_before_source_io_and_mirror_creation() {
        let cases = [
            (Some("off"), "disabled"),
            (Some("0"), "disabled"),
            (Some(" FALSE "), "disabled"),
            (Some("no"), "disabled"),
            (Some("1"), "enabled"),
            (Some("on"), "enabled"),
            (Some(""), "enabled"),
            (None, "enabled"),
        ];
        for (value, expect) in cases {
            let root = tempfile::tempdir().expect("switch fixture root");
            std::fs::write(root.path().join("session.jsonl"), b"{\"role\":\"user\"}\n")
                .expect("switch fixture source");
            let mut command =
                std::process::Command::new(std::env::current_exe().expect("test executable"));
            command
                .args(["--exact", SWITCH_CHILD, "--nocapture"])
                .current_dir(root.path())
                .env(SWITCH_ROOT, root.path())
                .env(SWITCH_EXPECT, expect);
            match value {
                Some(value) => command.env("CASS_RAW_MIRROR", value),
                None => command.env_remove("CASS_RAW_MIRROR"),
            };
            let output = command.output().expect("run capture switch child");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{value:?}:\n{stdout}\n{stderr}");
            assert!(
                stdout.contains("test result: ok. 1 passed"),
                "{value:?}: the child must run exactly one test\n{stdout}"
            );
        }
    }

    #[test]
    fn gh506_capture_switch_child() {
        use super::{
            RawMirrorCaptureDisabled, RawMirrorCaptureInput, capture_enabled, capture_source_file,
            capture_source_file_with_chunk_policy,
        };
        use std::path::{Path, PathBuf};

        fn input<'a>(data: &'a Path, source: &'a Path) -> RawMirrorCaptureInput<'a> {
            RawMirrorCaptureInput {
                data_dir: data,
                provider: "claude_code",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: source,
                db_links: &[],
            }
        }

        let Some(root) = std::env::var_os(SWITCH_ROOT) else {
            return;
        };
        let root = PathBuf::from(root);
        let expect = dotenvy::var(SWITCH_EXPECT).expect("switch expectation");
        let data = root.join("cass-data");
        let source = root.join("session.jsonl");
        if expect == "disabled" {
            assert!(!capture_enabled());
            let missing = root.join("not-present.jsonl");
            for path in [&source, &missing] {
                let error = capture_source_file(input(&data, path)).unwrap_err();
                assert!(error.downcast_ref::<RawMirrorCaptureDisabled>().is_some());
                assert!(!error.to_string().contains(path.to_string_lossy().as_ref()));
                let error =
                    capture_source_file_with_chunk_policy(input(&data, path), 1, 7).unwrap_err();
                assert!(error.downcast_ref::<RawMirrorCaptureDisabled>().is_some());
            }
            assert!(
                !data.exists(),
                "a disabled capture must not create the mirror"
            );
        } else {
            assert_eq!(expect, "enabled");
            assert!(capture_enabled());
            let record = capture_source_file(input(&data, &source)).expect("enabled capture");
            assert_eq!(
                record.source_size_bytes,
                std::fs::metadata(&source).unwrap().len()
            );
            assert!(data.join("raw-mirror").is_dir());
        }
    }
}
