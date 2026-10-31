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
