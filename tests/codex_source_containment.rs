//! Real FAD scan + CASS enrichment, without models or an authoritative archive.
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
        let row = json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":format!("containmentmarker{i}")}]
        }});
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

#[test]
fn oversized_rollout_does_not_hide_later_sources_or_complete_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (ctx, paths) = fixture(dir.path())?;
    // All lines are valid JSON, and the first record is a real conversation.
    // Padding is never read by the corrected preflight. This is not a malformed
    // file test masquerading as a size rejection.
    let mut file = std::fs::OpenOptions::new().append(true).open(&paths[1])?;
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(65536)
    );
    while file.metadata()?.len() <= LIMIT {
        file.write_all(padding.as_bytes())?;
    }
    drop(file);
    let before = paths
        .iter()
        .map(std::fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut delivered = Vec::new();
    let mut completed = Vec::new();
    let mut completion = |done: &SourceCompletion| {
        completed.push(done.source.source_path.clone());
        Ok(())
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut completion),
    };
    let error = CodexConnector::new()
        .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        delivered,
        [paths[0].clone(), paths[2].clone()],
        "an oversized middle source must not hide the last source"
    );
    assert_eq!(
        completed, delivered,
        "a discarded source must never receive completion"
    );
    let error = error.to_string();
    assert!(error.contains("enrichment_read_budget_exceeded"), "{error}");
    assert!(error.contains("rollout-b.jsonl"), "{error}");
    // Reuse is the host's decision: completed neighbors can be skipped while B
    // remains a reported rejection, rather than becoming an ordinary skip.
    let mut scanned = Vec::new();
    let mut predicate = |source: &coding_agent_search::connectors::DiscoveredSourceFile| {
        scanned.push(source.source_path.clone());
        !completed.contains(&source.source_path)
    };
    let mut repeated = 0;
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut predicate),
        on_source_complete: None,
    };
    assert!(
        CodexConnector::new()
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_| {
                repeated += 1;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(scanned, paths);
    assert_eq!(repeated, 0);
    for (path, original) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path)?, original);
    }
    Ok(())
}

#[test]
fn callback_reports_partial_coverage_and_all_or_error_scan_does_not_claim_success() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (ctx, paths) = fixture(dir.path())?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&paths[1])?
        .set_len(LIMIT + 1)?;
    let mut emitted = Vec::new();
    assert!(
        CodexConnector::new()
            .scan_with_callback(&ctx, &mut |conversation| {
                emitted.push(conversation.source_path);
                Ok(())
            })
            .is_err()
    );
    assert_eq!(emitted, [paths[0].clone(), paths[2].clone()]);
    assert!(CodexConnector::new().scan(&ctx).is_err());
    Ok(())
}

#[test]
fn intentionally_excluded_source_is_not_reported_as_an_attempted_failure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (ctx, paths) = fixture(dir.path())?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&paths[1])?
        .set_len(LIMIT + 1)?;
    let mut predicate = |source: &coding_agent_search::connectors::DiscoveredSourceFile| {
        source.source_path != paths[1]
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut predicate),
        on_source_complete: None,
    };
    let mut emitted = Vec::new();
    CodexConnector::new().scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
        emitted.push(conversation.source_path);
        Ok(())
    })?;
    assert_eq!(emitted, [paths[0].clone(), paths[2].clone()]);
    Ok(())
}

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
        assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "empty test selection: {stdout}"
        );
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
                assert_eq!(emitted, paths);
                let large = &delivered[1];
                assert_eq!(
                    large.messages.len(),
                    3,
                    "primary/enrichment must not duplicate messages"
                );
                assert!(
                    large
                        .messages
                        .iter()
                        .any(|m| m.content == "containmentmarker1")
                );
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
                assert_eq!(tool.invocations[0].call_id.as_deref(), Some("budget-call"));
                assert_eq!(
                    tool.invocations[0].arguments,
                    Some(json!({"cmd":"echo budgettoolneedle 日本語"}))
                );
                assert!(
                    tool.content.contains("budgettoolneedle"),
                    "enrichment must run past the old cap"
                );
            } else if case == "sink-failure" {
                assert_eq!(emitted, [paths[0].clone()]);
                assert!(result.unwrap_err().to_string().contains("consumer stopped"));
            } else {
                assert_eq!(emitted, [paths[0].clone(), paths[2].clone()]);
                let error = result.unwrap_err();
                if case == "unfinished" {
                    assert_eq!(
                        error.downcast_ref::<io::Error>().unwrap().kind(),
                        io::ErrorKind::UnexpectedEof
                    );
                } else {
                    assert!(
                        error
                            .to_string()
                            .contains("enrichment_read_budget_exceeded")
                    );
                }
            }
            assert_eq!(
                completed, emitted,
                "only successfully consumed sources may complete"
            );
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
        assert_eq!(receipt(&paths[1])?, before);
        fs::OpenOptions::new()
            .append(true)
            .open(&paths[1])?
            .write_all(b"{\"type\":")?;
        let unfinished = receipt(&paths[1])?;
        run_child(root.path(), test, "unfinished", Some("536870912"))?;
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
            assert!(error.to_string().contains(POLICY));
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
        assert_eq!(output.status.code(), Some(9), "{output:?}");
        let diagnostics = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostics.contains("enrichment_read_budget_exceeded"),
            "{diagnostics}"
        );
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
            assert!(output.status.success(), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout)?;
            let hits = value["hits"].as_array().expect("search hits");
            assert!(
                hits.iter().any(|hit| hit["agent"] == "codex"
                    && hit["source_path"].as_str() == paths[1].to_str()),
                "missing {needle}: {value}"
            );
            assert_eq!(value["_meta"]["search_mode"], "lexical", "{value}");
        }
        assert_eq!(
            receipt(&paths[1])?,
            before,
            "index/search must preserve the provider source"
        );
        Ok(())
    }
}
