//! Raw source preservation with admission checks shared by every capture route.
//!
//! Connector discovery is not the final privacy boundary: an empty inventory
//! can activate the indexer's legacy raw-mirror fallback (GH #486). Enforce
//! exclusions here too, before source access, mirror creation, locks or caches.

mod exclusions;
mod store;

pub use exclusions::RawMirrorSourceExcluded;
pub use store::*;

/// Capture an admitted source using the existing raw-mirror storage policy.
///
/// An excluded source returns [`RawMirrorSourceExcluded`] without reading the
/// source or changing the mirror. Existing captures are not purged by changing
/// scan exclusions; pruning remains a separate, explicit operation.
pub fn capture_source_file(
    input: RawMirrorCaptureInput<'_>,
) -> anyhow::Result<RawMirrorCaptureRecord> {
    exclusions::ensure_allowed(input.source_path)?;
    store::capture_source_file(input)
}

pub(crate) fn capture_source_file_with_chunk_policy(
    input: RawMirrorCaptureInput<'_>,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
) -> anyhow::Result<RawMirrorCaptureRecord> {
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
        assert!(stdout.contains(&format!("test {TARGET} ... ok")), "{stdout}");
        assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
    }
}
