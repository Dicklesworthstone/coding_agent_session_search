//! Raw source preservation with admission checks shared by every capture route.
//!
//! Connector discovery is not the final privacy boundary: an empty inventory
//! can activate the indexer's legacy raw-mirror fallback (GH #486). Enforce
//! exclusions here too, before source access, mirror creation, locks or caches.

mod codec;
mod exclusions;
#[path = "raw_mirror_cli.rs"]
mod index_maintenance;
pub(crate) use index_maintenance::{
    begin_capture_run, capture_pause_is_summarized, capture_status, capture_status_warning,
    maintain_for_index,
};
// Keep the policy next to the store's private validation/publication helpers.
// Legacy manifests keep their original serialized representation and hashes.
mod store {
    include!("raw_mirror/store.rs");
    pub(super) mod policy {
        include!("raw_mirror/policy.rs");
    }
}

use std::fmt;

pub use exclusions::RawMirrorSourceExcluded;
pub use store::policy::{
    RawMirrorAutomaticRetention, RawMirrorCapturePaused, RawMirrorMaintenanceReport,
    RawMirrorRetentionPolicy, maintain_before_index,
};
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

/// These stores mix conversations with settings, credentials or internal
/// prompts. Every raw capture and repair path must reject the whole container.
pub(crate) fn is_sensitive_container_provider(provider: &str) -> bool {
    matches!(provider, "shelley" | "grok_bot" | "pi_durable" | "zcode")
}

/// The provider label is not a sufficient privacy boundary: explicit source
/// roots can reach another reader's pre-parse fallback. Check the physical
/// store's ZCode marker too, including future schemas and database sidecars.
/// Any unreadable SQLite identity fails closed instead of copying unknown data.
pub(crate) fn zcode_source_is_sensitive(path: &std::path::Path) -> anyhow::Result<bool> {
    if crate::connectors::zcode::owns_path(path) {
        return Ok(true);
    }
    let database = if let Some(name) = path.file_name()
        && let Some(suffix) = ["-wal", "-shm", "-journal"]
            .into_iter()
            .find(|suffix| name.as_encoded_bytes().ends_with(suffix.as_bytes()))
    {
        let name = name.to_str().ok_or_else(|| {
            anyhow::anyhow!("cannot verify raw-mirror privacy for a non-UTF-8 database sidecar")
        })?;
        path.with_file_name(&name[..name.len() - suffix.len()])
    } else {
        path.to_path_buf()
    };
    // A sidecar's owner is a separate path and may already have rotated. Check
    // its file type before opening it, and use the store's nonblocking opener
    // for the header so ordinary transcript probes never wait on a writer.
    let metadata = std::fs::symlink_metadata(&database)?;
    anyhow::ensure!(
        metadata.is_file(),
        "raw-mirror source owner is not a regular file"
    );
    if metadata.len() < 16 {
        return Ok(false);
    }
    let mut file = store::open_stable_source_file(&database, &metadata)?;
    let mut header = [0; 16];
    std::io::Read::read_exact(&mut file, &mut header)?;
    if &header != b"SQLite format 3\0" {
        return Ok(false);
    }
    franken_agent_detection::connectors::zcode::is_zcode_container(&database)
}

// GH #522: small chunks bound the changing tail of append-oriented transcripts.
// GH #562 adds per-chunk compression without changing these logical boundaries
// or assuming an unchanged prefix. Legacy captures remain readable.
const TRANSCRIPT_CHUNK_BYTES: usize = 256 * 1024;

fn is_append_transcript(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("jsonl") || extension.eq_ignore_ascii_case("ndjson")
        })
}

/// Capture an admitted source using the existing raw-mirror storage policy.
///
/// With capture switched off this returns [`RawMirrorCaptureDisabled`], and an
/// excluded source returns [`RawMirrorSourceExcluded`]; neither reads the
/// source or changes the mirror. Budget pressure returns
/// [`RawMirrorCapturePaused`] without discarding existing evidence.
pub fn capture_source_file(
    input: RawMirrorCaptureInput<'_>,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    if !capture_enabled() {
        return Err(RawMirrorCaptureDisabled.into());
    }
    exclusions::ensure_allowed(input.source_path)?;
    let policy = RawMirrorRetentionPolicy::from_env()?;
    let result = if is_append_transcript(input.source_path) {
        store::policy::capture(
            input.clone(),
            TRANSCRIPT_CHUNK_BYTES as u64,
            TRANSCRIPT_CHUNK_BYTES,
            policy,
        )
    } else {
        store::policy::capture_default(input.clone(), policy)
    };
    index_maintenance::observe_capture_result(&input, &result);
    result
}

/// Preserve exactly the Codex bytes admitted by a stable-prefix delivery.
///
/// Source policy and manifest identity use the original source name. Capture
/// reads exactly the admitted range through a verified descriptor and the
/// existing budgeted chunk writer; no second full-size temporary copy is made.
/// A later append must not change this capture or create a full-source cache entry.
pub(crate) fn capture_codex_prefix(
    input: RawMirrorCaptureInput<'_>,
    receipt: &crate::connectors::codex::StablePrefixReceipt,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    if !capture_enabled() {
        return Err(RawMirrorCaptureDisabled.into());
    }
    exclusions::ensure_allowed(input.source_path)?;
    anyhow::ensure!(
        input.provider == "codex" && input.source_path == receipt.source_path.as_path(),
        "Codex prefix capture does not match its admitted source"
    );
    // Physical ownership remains authoritative even when a caller labels a
    // sensitive store as Codex. Check before making another copy of its bytes.
    if zcode_source_is_sensitive(input.source_path)? {
        anyhow::bail!(
            "disabled_sensitive_container: ZCode database and store files cannot be raw-mirrored"
        );
    }
    let policy = RawMirrorRetentionPolicy::from_env()?;
    let result = store::policy::capture_codex_prefix(
        input.clone(),
        receipt,
        TRANSCRIPT_CHUNK_BYTES as u64,
        TRANSCRIPT_CHUNK_BYTES,
        policy,
    );
    index_maintenance::observe_capture_result(&input, &result);
    result
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
    let result = store::policy::capture(
        input.clone(),
        chunk_threshold_bytes,
        chunk_size_bytes,
        RawMirrorRetentionPolicy::from_env()?,
    );
    index_maintenance::observe_capture_result(&input, &result);
    result
}

#[cfg(test)]
mod append_tests;

#[cfg(all(test, unix))]
mod prefix_tests;

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
