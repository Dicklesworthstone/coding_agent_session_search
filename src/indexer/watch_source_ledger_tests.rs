//! Exercise provider boundary behavior without opening canonical or lexical
//! storage. The fixture reproduces Codex/Claude's completion rule: a delivered
//! prefix has no completion event if its source changed before certification.

use super::*;
use crate::connectors::{DetectionResult, DiscoveredSourceRole};
use franken_agent_detection::{SourceCompletion, SourceScanHooks};
use serial_test::serial;
use std::io::Write;
use tempfile::TempDir;

struct WithheldCompletionConnector {
    provider: &'static str,
    root: ScanRoot,
    paths: Vec<PathBuf>,
    changing: PathBuf,
    append_once: Cell<bool>,
    withheld: Cell<usize>,
}

impl Connector for WithheldCompletionConnector {
    fn detect(&self) -> DetectionResult {
        DetectionResult {
            detected: true,
            evidence: Vec::new(),
            root_paths: vec![self.root.path.clone()],
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_source_boundaries(
            ctx,
            &mut SourceScanHooks::default(),
            &mut |conversation| {
                conversations.push(conversation);
                Ok(())
            },
        )?;
        Ok(conversations)
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn discover_source_files(&self, _ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(self
            .paths
            .iter()
            .map(|path| {
                DiscoveredSourceFile::new(
                    self.provider,
                    &self.root,
                    path.clone(),
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata()
            })
            .collect())
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        assert_eq!(ctx.since_ts, None);
        for source in self.discover_source_files(ctx)? {
            if !hooks.should_scan(&source) {
                continue;
            }
            let content = fs::read_to_string(&source.source_path)?;
            on_conversation(NormalizedConversation {
                agent_slug: self.provider.to_string(),
                external_id: None,
                title: None,
                workspace: None,
                source_path: source.source_path.clone(),
                started_at: None,
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: None,
                    created_at: None,
                    content,
                    extra: serde_json::json!({}),
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                }],
            })?;
            if source.source_path == self.changing && self.append_once.replace(false) {
                // A size change is observable on every supported filesystem;
                // this needs neither a sleep nor a timestamp-granularity bet.
                OpenOptions::new()
                    .append(true)
                    .open(&source.source_path)?
                    .write_all(b" plus newly appended tail")?;
            }
            if source.fs_metadata_changed() {
                self.withheld.set(self.withheld.get() + 1);
            } else {
                hooks.complete(&SourceCompletion {
                    source,
                    required_sidecars: Vec::new(),
                    conversations_emitted: 1,
                })?;
            }
        }
        Ok(())
    }
}

fn detect_withheld_completion(next_source: bool) -> Result<()> {
    for (connector_name, provider) in [("codex", "codex"), ("claude", "claude_code")] {
        let temp = TempDir::new()?;
        let changing = temp.path().join("rollout-changing.jsonl");
        fs::write(&changing, b"delivered prefix")?;
        let stable = temp.path().join("rollout-stable.jsonl");
        let mut paths = vec![changing.clone()];
        if next_source {
            fs::write(&stable, b"stable sibling")?;
            paths.push(stable.clone());
        }
        let root = ScanRoot::local(temp.path().to_path_buf());
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root.clone()], Some(1));
        let connector = WithheldCompletionConnector {
            provider,
            root,
            paths,
            changing: changing.clone(),
            append_once: Cell::new(true),
            withheld: Cell::new(0),
        };
        let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
        let mut ledger = WatchSourceLedger::default();
        let mut delivered = Vec::new();
        ledger.scan(
            &connector,
            connector_name,
            &ctx,
            &HashMap::new(),
            true,
            false,
            &filter,
            None,
            &mut |conversation| {
                assert_eq!(conversation.messages.len(), 1);
                delivered.push((
                    conversation.source_path,
                    conversation.messages[0].content.clone(),
                ));
                Ok(true)
            },
        )?;
        let mut expected = vec![(changing.clone(), "delivered prefix".to_string())];
        if next_source {
            expected.push((stable.clone(), "stable sibling".to_string()));
        }
        assert_eq!(delivered, expected, "each source prefix is delivered once");
        assert_eq!(connector.withheld.get(), 1);
        assert!(
            ledger.unstable_sources,
            "a provider's omitted completion must keep the changing source eligible"
        );
        assert!(!ledger.completions.contains_key(&changing));
        assert_eq!(ledger.completions.len(), usize::from(next_source));
        assert!(filter.take_deferred_sources().is_empty());
        if next_source {
            assert!(ledger.completions.contains_key(&stable));
        }

        // The following scan observes the complete bytes and emits completion.
        // Instability must not become sticky after the source has settled.
        let mut retry = WatchSourceLedger::default();
        let mut delivered = Vec::new();
        retry.scan(
            &connector,
            connector_name,
            &ctx,
            &HashMap::new(),
            true,
            false,
            &filter,
            None,
            &mut |conversation| {
                delivered.push((
                    conversation.source_path,
                    conversation.messages[0].content.clone(),
                ));
                Ok(true)
            },
        )?;
        expected[0].1 = "delivered prefix plus newly appended tail".to_string();
        assert_eq!(delivered, expected);
        assert!(!retry.unstable_sources);
        assert_eq!(connector.withheld.get(), 1);
        assert_eq!(retry.completions.len(), expected.len());
        assert_eq!(retry.completions[&changing].remaining, 1);
        assert!(filter.take_deferred_sources().is_empty());
    }
    Ok(())
}

#[test]
#[serial]
fn gh538_watch_ledger_detects_withheld_completion_at_scan_end() -> Result<()> {
    detect_withheld_completion(false)
}

#[test]
#[serial]
fn gh538_watch_ledger_detects_withheld_completion_at_next_source() -> Result<()> {
    detect_withheld_completion(true)
}

const PI_FAMILY_CONTRACT_CHILD: &str = "CASS_TEST_WATCH_PI_FAMILY_CONTRACT_CHILD";
const PI_FAMILY_CONTRACT_STATE: &str = "CASS_TEST_WATCH_PI_FAMILY_CONTRACT_STATE";

fn write_pi_family_contract_session(state: &Path) -> Result<PathBuf> {
    let sessions = state.join("launch-sessions");
    fs::create_dir_all(&sessions)?;
    let path = sessions.join("2026-08-24T12-00-00_watch.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"watch-contract\",\"timestamp\":\"2026-08-24T12:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-08-24T12:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"keep the latest profile\"}}\n",
        ),
    )?;
    Ok(path)
}

fn run_pi_family_contract_child(
    state: &Path,
    phase: &str,
    profile: &str,
    pi_override: Option<&Path>,
) -> Result<()> {
    // Each invocation models a watcher restart under a different live policy.
    // Child-only environment changes cannot race other connector tests.
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "indexer::watch_source_ledger::boundary_tests::gh574_watch_pi_family_contract_child",
            "--nocapture",
        ])
        .current_dir(state)
        .env(PI_FAMILY_CONTRACT_CHILD, phase)
        .env(PI_FAMILY_CONTRACT_STATE, state)
        .env("PI_CODING_AGENT_SESSION_DIR", state.join("launch-sessions"))
        .env("OMP_PROFILE", profile)
        .env("RUST_MIN_STACK", "134217728");
    for key in [
        "PI_SESSIONS_DIR",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
        "PI_PROFILE",
        "CASS_OMP_DATA_ROOT",
        "CASS_EXCLUDE_PATHS",
        "CASS_EXCLUDE_WORKSPACES",
        "CASS_SKIP_SUBAGENTS",
    ] {
        command.env_remove(key);
    }
    if let Some(path) = pi_override {
        command.env("PI_SESSIONS_DIR", path);
    }
    let output = command.output()?;
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "watch contract phase {phase:?}, profile {profile:?} failed: {diagnostic}",
    );
    assert!(
        diagnostic.contains("watch-pi-family-contract-ok"),
        "watch contract child did not execute: {diagnostic}",
    );
    Ok(())
}

#[test]
fn gh574_watch_receipts_track_live_pi_family_ownership_and_profile() -> Result<()> {
    let temp = TempDir::new()?;
    let path = write_pi_family_contract_session(temp.path())?;
    let bytes = fs::read(&path)?;
    run_pi_family_contract_child(temp.path(), "create", "work", None)?;
    run_pi_family_contract_child(temp.path(), "reuse", "work", None)?;
    run_pi_family_contract_child(temp.path(), "reparse", "review", None)?;
    run_pi_family_contract_child(temp.path(), "reuse", "review", None)?;
    // Returning to an earlier profile must replace the latest archive parse,
    // rather than finding a separate receipt under the old profile's key.
    run_pi_family_contract_child(temp.path(), "reparse", "work", None)?;
    let other_pi_store = temp.path().join("other-pi-store");
    fs::create_dir(&other_pi_store)?;
    run_pi_family_contract_child(temp.path(), "reparse", "work", Some(&other_pi_store))?;
    run_pi_family_contract_child(temp.path(), "reuse", "work", Some(&other_pi_store))?;
    assert_eq!(fs::read(path)?, bytes, "only parsing policy changed");
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh574_watch_uncacheable_pi_family_policy_keeps_sources_eligible() -> Result<()> {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};

    let temp = TempDir::new()?;
    write_pi_family_contract_session(temp.path())?;
    run_pi_family_contract_child(temp.path(), "create", "work", None)?;
    // A Unicode override can resolve through an alias to a native path which
    // JSON cannot represent. The actual session remains Unicode and parsable.
    let native = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"pi-\xff-store".to_vec()));
    fs::create_dir(&native)?;
    let alias = temp.path().join("pi-alias");
    symlink(&native, &alias)?;
    run_pi_family_contract_child(temp.path(), "uncacheable", "work", Some(&alias))
}

#[test]
fn gh574_watch_pi_family_contract_child() -> Result<()> {
    let Ok(phase) = dotenvy::var(PI_FAMILY_CONTRACT_CHILD) else {
        return Ok(());
    };
    let state = PathBuf::from(dotenvy::var(PI_FAMILY_CONTRACT_STATE)?);
    let sessions = state.join("launch-sessions");
    let path = sessions.join("2026-08-24T12-00-00_watch.jsonl");
    let saved_path = state.join("source-receipts.json");
    let mut saved: HashMap<String, String> = if phase == "create" {
        HashMap::new()
    } else {
        serde_json::from_slice(&fs::read(&saved_path)?)?
    };
    let (producer_contract, cacheable) = source_ledger_producer_contract("omp")?;
    if phase == "uncacheable" {
        assert!(!cacheable);
        assert!(source_ledger_producer_contract("codex")?.1);
        // Even a legacy receipt that matches the base contract must not be
        // reused when the Pi-family context cannot be encoded faithfully.
        for observation in saved.values_mut() {
            let mut value: serde_json::Value = serde_json::from_str(observation)?;
            value["producer_contract"] = serde_json::json!(producer_contract);
            *observation = value.to_string();
        }
    } else {
        assert!(cacheable);
    }
    let ctx = ScanContext::with_roots(
        state.join("cass-state"),
        vec![ScanRoot::local(sessions)],
        Some(i64::MAX),
    );
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut delivered = Vec::new();
    ledger.scan(
        &crate::connectors::omp::OmpConnector::new(),
        "omp",
        &ctx,
        &saved,
        true,
        true,
        &filter,
        None,
        &mut |conversation| {
            delivered.push(conversation);
            Ok(true)
        },
    )?;
    if phase == "reuse" {
        assert!(delivered.is_empty());
        assert_eq!(ledger.reused_sources, vec![path]);
        assert!(ledger.completions.is_empty());
    } else {
        assert_eq!(
            delivered.len(),
            1,
            "the unchanged transcript must be reparsed"
        );
        assert_eq!(delivered[0].agent_slug, "omp");
        assert_eq!(delivered[0].source_path, path);
        assert_eq!(
            delivered[0].metadata["profile"],
            dotenvy::var("OMP_PROFILE")?
        );
        assert_eq!(delivered[0].messages[0].content, "keep the latest profile");
        assert!(ledger.reused_sources.is_empty());
        if phase == "uncacheable" {
            assert!(ledger.completions.is_empty());
            assert_eq!(ledger.admitted_keys, saved.keys().cloned().collect());
        } else {
            assert_eq!(ledger.completions.len(), 1);
            let completion = &ledger.completions[&path];
            let value: serde_json::Value = serde_json::from_str(&completion.entry.observation)?;
            assert_eq!(value["producer_contract"], producer_contract);
            assert_eq!(completion.remaining, 1);
            if phase == "reparse" {
                assert_eq!(saved.len(), 1);
                assert_ne!(
                    saved.get(&completion.entry.key),
                    Some(&completion.entry.observation),
                    "the same source key must replace its obsolete interpretation",
                );
                assert!(saved.contains_key(&completion.entry.key));
            }
            saved.insert(
                completion.entry.key.clone(),
                completion.entry.observation.clone(),
            );
            fs::write(saved_path, serde_json::to_vec(&saved)?)?;
        }
    }
    println!("watch-pi-family-contract-ok");
    Ok(())
}
