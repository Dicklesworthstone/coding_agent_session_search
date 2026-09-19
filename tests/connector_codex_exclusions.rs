//! GH #486: exercise the CASS wrapper, not only the upstream connector.
//! Each operation uses the real environment reader in an isolated child process.

use std::path::{Path, PathBuf};
use std::process::Command;

use coding_agent_search::connectors::{
    Connector, DiscoveredSourceFile, NormalizedConversation, ScanContext, ScanRoot,
    codex::CodexConnector,
};
use franken_agent_detection::{SourceCompletion, SourceScanHooks};
use tempfile::TempDir;

const CHILD_ROOT: &str = "CASS_CODEX_EXCLUSIONS_TEST_ROOT";
const CHILD_MODE: &str = "CASS_CODEX_EXCLUSIONS_TEST_MODE";
const CHILD_EXPECTED: &str = "CASS_CODEX_EXCLUSIONS_TEST_EXPECTED";
const CHILD_FILES: &str = "CASS_CODEX_EXCLUSIONS_TEST_FILES";

struct Fixture {
    root: TempDir,
    files: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let month = root.path().join(".codex/sessions/2026/09");
        let private = month.join("18");
        let files = vec![
            private.join("rollout-private.jsonl"),
            private.join("rollout-legacy.json"),
            private.join("rollout-private.jsonl-copy.jsonl"),
            month.join("18-copy/rollout-sibling.jsonl"),
            month.join("19/rollout-public.jsonl"),
        ];
        for file in &files {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let content = if file.extension().unwrap() == "json" {
                r#"{"items":[{"role":"user","content":"fixture message"}]}"#
            } else {
                r#"{"type":"response_item","payload":{"role":"user","content":"fixture message"}}"#
            };
            std::fs::write(file, format!("{content}\n")).unwrap();
        }
        Self { root, files }
    }

    fn run(&self, exclusions: &str, expected_indices: &[usize]) {
        let expected: Vec<_> = expected_indices.iter().map(|&i| &self.files[i]).collect();
        let expected = serde_json::to_string(&expected).unwrap();
        let files = serde_json::to_string(&self.files).unwrap();
        for mode in [
            "default",
            "home",
            "codex",
            "sessions",
            "files",
            "overlapping",
        ] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "codex_exclusions_child", "--nocapture"])
                .current_dir(self.root.path())
                .env(CHILD_ROOT, self.root.path())
                .env(CHILD_MODE, mode)
                .env(CHILD_EXPECTED, &expected)
                .env(CHILD_FILES, &files)
                .env("CODEX_HOME", self.root.path().join(".codex"))
                .env("CASS_EXCLUDE_PATHS", exclusions)
                .output()
                .expect("run CASS Codex exclusion regression");
            assert!(
                output.status.success(),
                "mode={mode}, exclusions={exclusions:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
}

fn context(root: &Path, files: &[PathBuf], mode: &str, since: Option<i64>) -> ScanContext {
    let data_dir = root.join("cass");
    let home = root.join(".codex");
    let sessions = home.join("sessions");
    let paths = match mode {
        "default" => return ScanContext::local_default(data_dir, since),
        "home" => vec![root.to_path_buf()],
        "codex" => vec![home],
        "sessions" => vec![sessions],
        "files" => files.to_vec(),
        "overlapping" => {
            let mut paths = vec![root.to_path_buf(), home, sessions];
            paths.extend_from_slice(files);
            paths
        }
        _ => panic!("unknown root mode: {mode}"),
    };
    ScanContext::with_roots(
        data_dir,
        paths.into_iter().map(ScanRoot::local).collect(),
        since,
    )
}

fn assert_paths(mut actual: Vec<PathBuf>, expected: &[PathBuf]) {
    let mut expected = expected.to_vec();
    actual.sort();
    expected.sort();
    // Do not deduplicate: overlapping roots must emit each admitted source once.
    assert_eq!(actual, expected);
}

fn assert_conversations(conversations: &[NormalizedConversation], expected: &[PathBuf]) {
    for conversation in conversations {
        assert_eq!(conversation.agent_slug, "codex");
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].content, "fixture message");
    }
    assert_paths(
        conversations
            .iter()
            .map(|conversation| conversation.source_path.clone())
            .collect(),
        expected,
    );
}

fn assert_boundaries(connector: &CodexConnector, ctx: &ScanContext, expected: &[PathBuf]) {
    let mut visited = Vec::new();
    let mut completed = Vec::new();
    let mut conversations = Vec::new();
    {
        let mut should_scan = |source: &DiscoveredSourceFile| {
            assert!(expected.contains(&source.source_path));
            visited.push(source.source_path.clone());
            true
        };
        let mut complete = |done: &SourceCompletion| {
            assert_eq!(done.conversations_emitted, 1);
            assert!(done.required_sidecars.is_empty());
            completed.push(done.source.source_path.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut should_scan),
            on_source_complete: Some(&mut complete),
        };
        connector
            .scan_with_source_boundaries(ctx, &mut hooks, &mut |conversation| {
                conversations.push(conversation);
                Ok(())
            })
            .unwrap();
    }
    assert_paths(visited, expected);
    assert_paths(completed, expected);
    assert_conversations(&conversations, expected);

    // The new guard must compose with the host's durable-reuse predicate.
    let mut should_scan = |source: &DiscoveredSourceFile| {
        assert!(expected.contains(&source.source_path));
        false
    };
    let mut complete = |_: &SourceCompletion| panic!("a skipped source cannot complete");
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut should_scan),
        on_source_complete: Some(&mut complete),
    };
    connector
        .scan_with_source_boundaries(ctx, &mut hooks, &mut |_| {
            panic!("host-skipped source was parsed")
        })
        .unwrap();
}

#[test]
fn codex_exclusions_child() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = dotenvy::var(CHILD_MODE).unwrap();
    let expected: Vec<PathBuf> =
        serde_json::from_str(&dotenvy::var(CHILD_EXPECTED).unwrap()).unwrap();
    let files: Vec<PathBuf> = serde_json::from_str(&dotenvy::var(CHILD_FILES).unwrap()).unwrap();
    let connector = CodexConnector::new();
    for since in [None, Some(0)] {
        let ctx = context(&root, &files, &mode, since);
        assert_paths(
            connector
                .discover_source_files(&ctx)
                .unwrap()
                .into_iter()
                .map(|source| source.source_path)
                .collect(),
            &expected,
        );
        assert_conversations(&connector.scan(&ctx).unwrap(), &expected);
        let mut streamed = Vec::new();
        connector
            .scan_with_callback(&ctx, &mut |conversation| {
                streamed.push(conversation);
                Ok(())
            })
            .unwrap();
        assert_conversations(&streamed, &expected);
        assert_boundaries(&connector, &ctx, &expected);
        if !expected.is_empty() {
            let error = connector
                .scan_with_callback(&ctx, &mut |_| anyhow::bail!("sink must still abort"))
                .unwrap_err();
            assert_eq!(error.to_string(), "sink must still abort");
        }
    }
}

#[test]
fn codex_exclusions_exact_files_preserve_siblings() {
    let fixture = Fixture::new();
    let exclusions = format!(
        " , {} ,\r\n {} \n, ",
        fixture.files[0].display(),
        fixture.files[1].display(),
    );
    fixture.run(&exclusions, &[2, 3, 4]);
}

#[test]
fn codex_exclusions_parent_directory_preserves_prefix_sibling() {
    let fixture = Fixture::new();
    fixture.run(
        fixture.files[0].parent().unwrap().to_str().unwrap(),
        &[3, 4],
    );
}

#[test]
fn codex_exclusions_sessions_and_home_roots() {
    let fixture = Fixture::new();
    for root in [
        fixture.root.path().join(".codex/sessions"),
        fixture.root.path().join(".codex"),
        fixture.root.path().to_path_buf(),
    ] {
        fixture.run(root.to_str().unwrap(), &[]);
    }
}

#[test]
fn codex_exclusions_empty_or_unrelated_preserve_all_sources() {
    let fixture = Fixture::new();
    for exclusions in ["", " , \n , "] {
        fixture.run(exclusions, &[0, 1, 2, 3, 4]);
    }
    fixture.run(
        fixture.root.path().join("unrelated").to_str().unwrap(),
        &[0, 1, 2, 3, 4],
    );
}

#[test]
fn codex_exclusions_precede_enrichment_and_size_rejection() {
    let mut fixture = Fixture::new();
    let sessions = fixture.root.path().join(".codex/sessions");
    let unfinished = sessions.join("rollout-unfinished.jsonl");
    let oversized = sessions.join("rollout-oversized.jsonl");
    let content = concat!(
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"private\"}}\n",
        "{\"type\":\"response_item\""
    );
    std::fs::write(&unfinished, content).unwrap();
    let size = 100 * 1024 * 1024 + 1;
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(size)
        .unwrap();
    fixture.files.extend([unfinished.clone(), oversized.clone()]);
    let exclusions = format!("{},{}", unfinished.display(), oversized.display());
    fixture.run(&exclusions, &[0, 1, 2, 3, 4]);
    assert_eq!(std::fs::read_to_string(&unfinished).unwrap(), content);
    assert_eq!(std::fs::metadata(&oversized).unwrap().len(), size);
}
