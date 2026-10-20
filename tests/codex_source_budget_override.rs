//! `CASS_CODEX_MAX_SOURCE_BYTES` through the real connector registry, a
//! re-executed test child per policy value, and the real `cass` binary.
//!
//! These tests need the full crate (`get_connector_factories`, `assert_cmd`,
//! `cargo_bin!("cass")`), so they are their own target: the slim consumer
//! contract crate built by `scripts/test_codex_contract.py` compiles
//! `codex_source_containment.rs` and cannot link them (bead
//! coding_agent_session_search-2l1b0.62).
use anyhow::Result;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot, codex::CodexConnector};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

const LIMIT: u64 = 100 * 1024 * 1024;

fn fixture(root: &Path) -> Result<(ScanContext, [PathBuf; 3])> {
    let sessions = root.join(".codex/sessions");
    std::fs::create_dir_all(&sessions)?;
    let paths = ["a", "b", "c"].map(|name| sessions.join(format!("rollout-{name}.jsonl")));
    for (i, path) in paths.iter().enumerate() {
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- Three bounded synthetic fixtures.
        let row = json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":format!("containmentmarker{i}")}]
        }});
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- Three bounded synthetic fixtures.
        std::fs::write(path, format!("{row}\n"))?;
    }
    Ok((
        ScanContext::with_roots(
            root.join("archive"),
            vec![ScanRoot::local(root.join(".codex"))],
            None,
        ),
        paths,
    ))
}

// The module name is part of each child's `--exact` test path.
mod budget_override {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::io::{self, Read};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    const POLICY: &str = "CASS_CODEX_MAX_SOURCE_BYTES";
    const CHILD_ROOT: &str = "CASS_TEST_CODEX_BUDGET_ROOT";
    const CHILD_CASE: &str = "CASS_TEST_CODEX_BUDGET_CASE";

    fn paths(root: &Path) -> [PathBuf; 3] {
        ["a", "b", "c"].map(|name| {
            root.join(".codex/sessions")
                .join(format!("rollout-{name}.jsonl"))
        })
    }

    fn grow_modern_rollout(path: &Path) -> Result<()> {
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        // Real valid JSONL crosses the actual old cap. The unrecognized records
        // test file admission/streaming, NOT large-message memory or throughput.
        let padding = format!(
            "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
            "x".repeat(65_536)
        );
        while file.metadata()?.len() <= LIMIT {
            file.write_all(padding.as_bytes())?;
        }
        writeln!(
            file,
            "{}",
            json!({"type":"response_item", "payload":{
                "type":"function_call", "name":"exec_command", "call_id":"budget-call",
                "arguments":"{\"cmd\":\"echo budgettoolneedle 日本語\"}"
            }})
        )?;
        writeln!(
            file,
            "{}",
            json!({"type":"response_item", "payload":{
                "type":"message", "role":"user", "content":[{"type":"input_text", "text":"budgettailneedle"}]
            }})
        )?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(file.metadata()?.len() > LIMIT);
        Ok(())
    }

    fn receipt(path: &Path) -> Result<(blake3::Hash, u64, SystemTime)> {
        let mut file = fs::File::open(path)?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0_u8; 65_536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let metadata = file.metadata()?;
        Ok((hash.finalize(), metadata.len(), metadata.modified()?))
    }

    fn isolated(executable: &Path, root: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .current_dir(root)
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("XDG_CONFIG_HOME", root.join(".config"))
            .env("XDG_DATA_HOME", root.join(".local/share"))
            .env("APPDATA", root.join("AppData/Roaming"))
            .env("LOCALAPPDATA", root.join("AppData/Local"))
            .env("CODEX_HOME", root.join(".codex"))
            .env("CASS_DATA_DIR", root.join("archive"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728");
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }

    fn run_child(root: &Path, test: &str, case: &str, limit: Option<&str>) -> Result<()> {
        let target = format!("budget_override::{test}");
        let mut command = isolated(&std::env::current_exe()?, root);
        command
            .args(["--exact", &target, "--nocapture"])
            .env(CHILD_ROOT, root)
            .env(CHILD_CASE, case);
        if let Some(limit) = limit {
            command.env(POLICY, limit);
        }
        let output = assert_cmd::Command::from_std(command)
            .timeout(Duration::from_secs(120))
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "empty test selection: {stdout}"
        );
        Ok(())
    }

    fn compressed_metadata_fixture(root: &Path, compressed: &[u8]) -> Result<[PathBuf; 3]> {
        let sessions = root.join(".codex/sessions");
        fs::create_dir_all(&sessions)?;
        let paths = ["rollout-a.jsonl", "rollout-b.jsonl.zst", "rollout-c.jsonl"]
            .map(|name| sessions.join(name));
        for i in [0, 2] {
            let rows = [
                json!({"type":"session_meta","payload":{
                    "id":format!("compressed-neighbor-{i}"),"cwd":root.join("project") // ubs:ignore[rust.perf.string-alloc-in-loop] -- Two bounded synthetic fixture IDs.
                }}),
                json!({"type":"response_item","payload":{
                    "type":"message","role":"user","content":[{
                        "type":"input_text","text":format!("compressedneighbor{i}needle") // ubs:ignore[rust.perf.string-alloc-in-loop] -- Two bounded synthetic messages.
                    }]
                }}),
            ];
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- Two bounded synthetic fixtures.
            fs::write(&paths[i], format!("{}\n{}\n", rows[0], rows[1]))?;
        }
        fs::write(&paths[1], compressed)?;
        Ok(paths)
    }

    fn assert_only_compressed_neighbors(
        mut command: impl FnMut() -> Command,
        paths: &[PathBuf; 3],
    ) -> Result<()> {
        let output = assert_cmd::Command::from_std(command())
            .args(["stats", "--json"])
            .timeout(Duration::from_secs(30))
            .output()?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(output.status.success(), "{output:?}");
        let stats: Value = serde_json::from_slice(&output.stdout)?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(
            stats["conversations"], 2,
            "no fabricated empty conversation: {stats}"
        );
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(stats["messages"], 2, "{stats}");

        let output = assert_cmd::Command::from_std(command())
            .args([
                "search",
                "compressedneighbor0needle OR compressedneighbor2needle",
                "--mode",
                "lexical",
                "--json",
                "--robot-meta",
                "--no-maintenance",
                "--no-daemon",
                "--limit",
                "10",
                "--timeout",
                "5000",
            ])
            .timeout(Duration::from_secs(30))
            .output()?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout)?;
        let hits = result["hits"].as_array().expect("search hits");
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(hits.len(), 2, "{result}");
        for path in [&paths[0], &paths[2]] {
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(
                hits.iter()
                    .any(|hit| hit["agent"] == "codex"
                        && hit["source_path"].as_str() == path.to_str()),
                "missing {}: {result}",
                path.display()
            );
        }
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(result["_meta"]["search_mode"], "lexical", "{result}");
        Ok(())
    }

    /// GH #533: valid metadata-only zstd is a successful empty source, including
    /// on an ordinary incremental index; its neighbors still reach real search.
    #[test]
    fn compressed_metadata_only_indexes_neighbors_without_fabricating_a_conversation() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let metadata = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{
                "id":"metadata-only-gh533","cwd":"/synthetic/日本語"
            }})
        );
        let compressed = zstd::stream::encode_all(metadata.as_bytes(), 0)?;
        let paths = compressed_metadata_fixture(root.path(), &compressed)?;
        let before = paths
            .iter()
            .map(|path| receipt(path))
            .collect::<Result<Vec<_>>>()?;
        let binary = Path::new(assert_cmd::cargo::cargo_bin!("cass"));
        for run in 0..2 {
            let output = assert_cmd::Command::from_std(isolated(binary, root.path()))
                .args(["index", "--json", "--no-progress-events"])
                .timeout(Duration::from_secs(180))
                .output()?;
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(output.status.success(), "run {run}: {output:?}");
            let result: Value = serde_json::from_slice(&output.stdout)?;
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(result["success"], true, "run {run}: {result}");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(
                result["indexing_stats"]["scan_had_errors"], false,
                "{result}"
            );
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_ne!(result["partial"], true, "{result}");
            assert_only_compressed_neighbors(|| isolated(binary, root.path()), &paths)?;
            for (path, original) in paths.iter().zip(&before) {
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(
                    &receipt(path)?,
                    original,
                    "source changed on run {run}: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    /// An unfinished frame can decode a complete metadata record. Neither its
    /// zero-message result nor a subsequent incremental pass may hide the error.
    #[test]
    fn compressed_metadata_only_truncation_stays_partial_and_keeps_neighbors() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0)?;
        encoder.include_checksum(true)?;
        encoder.write_all(
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"metadata-only-gh533\"}}\n",
        )?;
        let mut compressed = encoder.finish()?;
        // Remove only a checksum byte, after the complete decoded JSON record.
        compressed.truncate(compressed.len() - 1);
        let paths = compressed_metadata_fixture(root.path(), &compressed)?;
        let before = paths
            .iter()
            .map(|path| receipt(path))
            .collect::<Result<Vec<_>>>()?;
        let binary = Path::new(assert_cmd::cargo::cargo_bin!("cass"));
        for run in 0..2 {
            let output = assert_cmd::Command::from_std(isolated(binary, root.path()))
                .args(["index", "--json", "--no-progress-events"])
                .timeout(Duration::from_secs(180))
                .output()?;
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(output.status.code(), Some(9), "run {run}: {output:?}");
            let result: Value = serde_json::from_slice(&output.stdout)?;
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(result["success"], false, "{result}");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(result["partial"], true, "{result}");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(result["coverage_status"], "incomplete", "{result}");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(
                result["indexing_stats"]["scan_had_errors"], true,
                "{result}"
            );
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(result["failed_connectors"], json!(["codex"]), "{result}");
            let codex = result["indexing_stats"]["connectors"]
                .as_array()
                .expect("connector stats")
                .iter()
                .find(|connector| connector["name"] == "codex")
                .expect("Codex stats");
            let detail = codex["error"].as_str().expect("rejected source reason");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(detail.contains("rollout-b.jsonl.zst"), "{detail}");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(detail.contains("unfinished_source"), "{detail}");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(
                result["hint"].as_str().is_some_and(|hint| hint
                    .contains("indexing_stats.connectors")
                    && hint.contains("rerun")),
                "actionable source failure: {result}"
            );
            assert_only_compressed_neighbors(|| isolated(binary, root.path()), &paths)?;
            for (path, original) in paths.iter().zip(&before) {
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(
                    &receipt(path)?,
                    original,
                    "source changed on run {run}: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    /// A traversal error must not certify a partial inventory or advance past
    /// older history. Run the real binary without root's DAC override so the
    /// fixture exercises an actual directory-open failure on Linux builders.
    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_directory_keeps_neighbors_and_retries_older_history() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;

        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))?;
        let compressed =
            zstd::stream::encode_all(&b"{\"type\":\"session_meta\",\"payload\":{}}\n"[..], 0)?;
        let paths = compressed_metadata_fixture(root.path(), &compressed)?;
        let blocked = root.path().join(".codex/sessions/blocked");
        fs::create_dir(&blocked)?;
        let hidden = blocked.join("rollout-hidden.jsonl");
        fs::write(
            &hidden,
            b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"discoveryrecoveryneedle\"}}\n",
        )?;
        fs::File::open(&hidden)?.set_times(
            fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)),
        )?;
        for directory in [
            root.path().join(".codex"),
            root.path().join(".codex/sessions"),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
        }
        for path in paths.iter().chain(std::iter::once(&hidden)) {
            fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
        }
        let before = paths
            .iter()
            .chain(std::iter::once(&hidden))
            .map(|path| receipt(path))
            .collect::<Result<Vec<_>>>()?;

        // Only the test's output directory is writable by the child. Provider
        // files stay read-only to it; no production source is chmod'ed or moved.
        let archive = root.path().join("archive");
        fs::create_dir(&archive)?;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o777))?;
        // Restore access even if a native assertion fails, so normal TempDir
        // teardown can inspect and clean up this synthetic fixture.
        struct RestoreDirectory(PathBuf);
        impl Drop for RestoreDirectory {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let restore = RestoreDirectory(blocked.clone());
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000))?;
        // A restricted user namespace can map only UID 0 while granting no
        // DAC override capability. It already observes real EACCES; switching
        // it to an unmapped UID would fail before the binary starts.
        let drop_root = match fs::read_dir(&blocked) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => false,
            Ok(_) => true,
            Err(error) => return Err(error.into()),
        };
        let binary = if drop_root {
            // A toolchain's temporary parent directory may be private to root.
            // Copy just the executable into this traversable synthetic fixture.
            let binary = root.path().join("cass-fixture");
            fs::copy(assert_cmd::cargo::cargo_bin!("cass"), &binary)?;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
            binary
        } else {
            PathBuf::from(assert_cmd::cargo::cargo_bin!("cass"))
        };
        let command = || {
            let mut command = isolated(&binary, root.path());
            command.env("CASS_RAW_MIRROR", "0");
            if drop_root {
                command.gid(65_534).uid(65_534);
            }
            command
        };

        for run in 0..2 {
            let output = assert_cmd::Command::from_std(command())
                .args(["index", "--json", "--no-progress-events"])
                .timeout(Duration::from_secs(180))
                .output()?;
            // ubs:ignore[rust.panic.assert-macros] -- Native incomplete-coverage regression.
            assert_eq!(output.status.code(), Some(9), "run {run}: {output:?}");
            let result: Value = serde_json::from_slice(&output.stdout)?;
            // ubs:ignore[rust.panic.assert-macros] -- Native incomplete-coverage regression.
            assert_eq!(result["coverage_status"], "incomplete", "{result}");
            // ubs:ignore[rust.panic.assert-macros] -- Native incomplete-coverage regression.
            assert_eq!(result["failed_connectors"], json!(["codex"]), "{result}");
            let codex = result["indexing_stats"]["connectors"]
                .as_array()
                .expect("connector stats")
                .iter()
                .find(|connector| connector["name"] == "codex")
                .expect("Codex stats");
            let detail = codex["error"].as_str().expect("directory failure");
            // ubs:ignore[rust.panic.assert-macros] -- Native actionable-error regression.
            assert!(detail.contains("blocked"), "{detail}");
            // ubs:ignore[rust.panic.assert-macros] -- Native actionable-error regression.
            assert!(
                detail.contains("Permission denied") || detail.contains("unreadable"),
                "{detail}"
            );
            assert_only_compressed_neighbors(&command, &paths)?;
        }

        drop(restore);
        let output = assert_cmd::Command::from_std(command())
            .args(["index", "--json", "--no-progress-events"])
            .timeout(Duration::from_secs(180))
            .output()?;
        // ubs:ignore[rust.panic.assert-macros] -- Ordinary retry must recover old-mtime history.
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout)?;
        // ubs:ignore[rust.panic.assert-macros] -- Ordinary retry must clear incomplete coverage.
        assert_eq!(
            result["indexing_stats"]["scan_had_errors"], false,
            "{result}"
        );
        let output = assert_cmd::Command::from_std(command())
            .args([
                "search",
                "discoveryrecoveryneedle OR compressedneighbor0needle OR compressedneighbor2needle",
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
                "--no-daemon",
                "--limit",
                "10",
                "--timeout",
                "5000",
            ])
            .timeout(Duration::from_secs(30))
            .output()?;
        // ubs:ignore[rust.panic.assert-macros] -- Real lexical recovery regression.
        assert!(output.status.success(), "{output:?}");
        let search: Value = serde_json::from_slice(&output.stdout)?;
        let hits = search["hits"].as_array().expect("search hits");
        // ubs:ignore[rust.panic.assert-macros] -- Real lexical recovery regression.
        assert_eq!(hits.len(), 3, "{search}");
        // ubs:ignore[rust.panic.assert-macros] -- Real lexical recovery regression.
        assert!(
            hits.iter()
                .any(|hit| hit["source_path"].as_str() == hidden.to_str()),
            "{search}"
        );
        for (path, original) in paths.iter().chain(std::iter::once(&hidden)).zip(&before) {
            // ubs:ignore[rust.panic.assert-macros] -- Source bytes, length and mtime are immutable.
            assert_eq!(
                &receipt(path)?,
                original,
                "source changed: {}",
                path.display()
            );
        }
        Ok(())
    }

    #[test]
    fn large_jsonl_budget_preserves_history_and_failure_boundaries() -> Result<()> {
        if let Ok(root) = dotenvy::var(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let paths = paths(&root);
            let case = dotenvy::var(CHILD_CASE)?;
            let (_, factory) = coding_agent_search::connectors::get_connector_factories()
                .into_iter()
                .find(|(name, _)| *name == "codex")
                .expect("Codex factory");
            let connector = factory();
            let ctx = ScanContext::with_roots(
                root.join("archive"),
                vec![ScanRoot::local(root.join(".codex"))],
                None,
            );
            let before = receipt(&paths[1])?;
            let mut completed = Vec::new();
            let mut complete = |done: &SourceCompletion| {
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(done.conversations_emitted, 1);
                completed.push(done.source.source_path.clone());
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            };
            let mut delivered = Vec::new();
            let result =
                connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                    if case == "sink-failure" && conversation.source_path == paths[1] {
                        return Err(io::Error::other("consumer stopped").into());
                    }
                    delivered.push(conversation);
                    Ok(())
                });
            let emitted: Vec<_> = delivered.iter().map(|c| c.source_path.clone()).collect();
            if case == "allowed" {
                result?;
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(emitted, paths);
                let large = &delivered[1];
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(
                    large.messages.len(),
                    3,
                    "primary/enrichment must not duplicate messages"
                );
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert!(
                    large
                        .messages
                        .iter()
                        .any(|m| m.content == "containmentmarker1")
                );
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert!(
                    large
                        .messages
                        .iter()
                        .any(|m| m.content == "budgettailneedle")
                );
                let tool = large
                    .messages
                    .iter()
                    .find(|m| !m.invocations.is_empty())
                    .expect("enriched tool call");
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(tool.invocations[0].call_id.as_deref(), Some("budget-call"));
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(
                    tool.invocations[0].arguments,
                    Some(json!({"cmd":"echo budgettoolneedle 日本語"}))
                );
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert!(
                    tool.content.contains("budgettoolneedle"),
                    "enrichment must run past the old cap"
                );
            } else if case == "sink-failure" {
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(emitted, [paths[0].clone()]);
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert!(result.unwrap_err().to_string().contains("consumer stopped"));
            } else {
                // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                assert_eq!(emitted, [paths[0].clone(), paths[2].clone()]);
                let error = result.unwrap_err();
                if case == "unfinished" {
                    // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                    assert_eq!(
                        error.downcast_ref::<io::Error>().unwrap().kind(),
                        io::ErrorKind::UnexpectedEof
                    );
                } else {
                    // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
                    assert!(
                        error
                            .to_string()
                            .contains("enrichment_read_budget_exceeded")
                    );
                }
            }
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(
                completed, emitted,
                "only successfully consumed sources may complete"
            );
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(receipt(&paths[1])?, before);
            return Ok(());
        }
        let root = tempfile::tempdir()?;
        let (_, paths) = fixture(root.path())?;
        grow_modern_rollout(&paths[1])?;
        let before = receipt(&paths[1])?;
        let exact_limit = before.1.to_string();
        let test = "large_jsonl_budget_preserves_history_and_failure_boundaries";
        run_child(root.path(), test, "default-rejection", None)?;
        run_child(root.path(), test, "allowed", Some(&exact_limit))?;
        run_child(root.path(), test, "sink-failure", Some("536870912"))?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(receipt(&paths[1])?, before);
        fs::OpenOptions::new()
            .append(true)
            .open(&paths[1])?
            .write_all(b"{\"type\":")?;
        let unfinished = receipt(&paths[1])?;
        run_child(root.path(), test, "unfinished", Some("536870912"))?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(receipt(&paths[1])?, unfinished);
        Ok(())
    }

    #[test]
    fn invalid_budget_is_rejected_before_source_callbacks() -> Result<()> {
        if let Ok(root) = dotenvy::var(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let ctx = ScanContext::with_roots(
                root.join("archive"),
                vec![ScanRoot::local(root.join(".codex"))],
                None,
            );
            let mut predicate =
                |_: &coding_agent_search::connectors::DiscoveredSourceFile| -> bool {
                    panic!("invalid budget must fail before admission callbacks")
                };
            let mut complete =
                |_: &SourceCompletion| -> Result<()> { panic!("invalid budget cannot complete") };
            let mut hooks = SourceScanHooks {
                should_scan_source: Some(&mut predicate),
                on_source_complete: Some(&mut complete),
            };
            let error = CodexConnector::new()
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_| {
                    panic!("invalid budget cannot emit")
                })
                .unwrap_err();
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(error.to_string().contains(POLICY));
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(!root.join("archive").exists());
            return Ok(());
        }
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        for limit in ["0", "not-a-byte-count", "1073741825"] {
            run_child(
                root.path(),
                "invalid_budget_is_rejected_before_source_callbacks",
                "invalid",
                Some(limit),
            )?;
        }
        Ok(())
    }

    #[test]
    fn larger_budget_recovers_rejected_history_into_real_lexical_search() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (_, paths) = fixture(root.path())?;
        grow_modern_rollout(&paths[1])?;
        let before = receipt(&paths[1])?;
        let binary = Path::new(assert_cmd::cargo::cargo_bin!("cass"));
        let mut rejected = assert_cmd::Command::from_std(isolated(binary, root.path()));
        let output = rejected
            .args(["index", "--full", "--json", "--no-progress-events"])
            .timeout(Duration::from_secs(180))
            .output()?;
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(output.status.code(), Some(9), "{output:?}");
        let diagnostics = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(
            diagnostics.contains("enrichment_read_budget_exceeded"),
            "{diagnostics}"
        );
        // 2l1b0.35: the connector diagnostic names the rejected rollout and the
        // setting that admits it, not an unreadable data directory.
        let failed: Value = serde_json::from_slice(&output.stdout)?;
        let connector_diagnostics = failed["indexing_stats"]["connector_diagnostics"]
            .as_array()
            .expect("connector diagnostics");
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(
            connector_diagnostics.iter().any(|diagnostic| {
                diagnostic["failure_kind"] == "source-over-read-budget"
                    && diagnostic["source_path"].as_str() == paths[1].to_str()
                    && diagnostic["safe_next_action"]
                        .as_str()
                        .is_some_and(|action| action.contains(POLICY))
            }),
            "{failed}"
        );
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert!(
            !connector_diagnostics
                .iter()
                .any(|diagnostic| diagnostic["failure_kind"] == "unreadable-source"),
            "{failed}"
        );
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(receipt(&paths[1])?, before);

        // Same archive, no --full: raising the budget must retry the rejected
        // source rather than advancing a watermark past its missing history.
        let mut admitted = assert_cmd::Command::from_std(isolated(binary, root.path()));
        admitted
            .env(POLICY, "536870912")
            .args(["index", "--json", "--no-progress-events"])
            .timeout(Duration::from_secs(180))
            .assert()
            .success();
        for needle in ["containmentmarker1", "budgettailneedle", "budgettoolneedle"] {
            let mut search = assert_cmd::Command::from_std(isolated(binary, root.path()));
            let output = search
                .args([
                    "search",
                    needle,
                    "--mode",
                    "lexical",
                    "--json",
                    "--robot-meta",
                    "--no-maintenance",
                    "--limit",
                    "10",
                    "--timeout",
                    "5000",
                ])
                .timeout(Duration::from_secs(30))
                .output()?;
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(output.status.success(), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout)?;
            let hits = value["hits"].as_array().expect("search hits");
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert!(
                hits.iter().any(|hit| hit["agent"] == "codex"
                    && hit["source_path"].as_str() == paths[1].to_str()),
                "missing {needle}: {value}"
            );
            // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
            assert_eq!(value["_meta"]["search_mode"], "lexical", "{value}");
        }
        // ubs:ignore[rust.panic.assert-macros] -- Intentional test assertion.
        assert_eq!(
            receipt(&paths[1])?,
            before,
            "index/search must preserve the provider source"
        );
        Ok(())
    }
}
